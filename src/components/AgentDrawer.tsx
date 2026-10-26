import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Bot, Check, Plus, RefreshCw, Trash2, X } from 'lucide-react';
import type { AgentAvailability, AgentProfile, ServiceConfig } from '../types';
import Modal from './Modal';
import Select from './Select';
import { agentPresets, matchesAgentPreset } from '../agentPresets';
import { trapDialogFocus } from './focusTrap';

interface AgentDrawerProps {
  agents: AgentProfile[];
  services: ServiceConfig[];
  initialServiceId?: string;
  onSave: (agent: AgentProfile) => Promise<AgentProfile>;
  onDelete: (agentId: string) => Promise<void>;
  onCheck: (agentId: string, serviceId?: string) => Promise<AgentAvailability>;
  onDirtyChange: (dirty: boolean) => void;
  onClose: () => void;
}

interface AgentEnvRow { key: string; value: string }
type PendingAgentAction = { kind: 'existing'; agent: AgentProfile } | { kind: 'new'; name: string; executable: string } | { kind: 'close' };


function emptyAgent(name = ''): AgentProfile {
  return { id: '', name, executable: '', args: [], env: {} };
}

function fingerprintAgent(agent: AgentProfile, argsText: string, rows: AgentEnvRow[]): string {
  const env = Object.fromEntries(rows.map((row) => [row.key.trim(), row.value]).filter(([key]) => Boolean(key)));
  return JSON.stringify({
    id: agent.id,
    name: agent.name,
    executable: agent.executable,
    args: argsText.split(/\r?\n/).map((line) => line.trim()).filter(Boolean),
    env,
  });
}

/**
 * AI 工具仅作为终端程序配置管理：参数逐行保存为 argv，检测只查询可执行程序，
 * 不在应用中拼接 shell 命令、安装 CLI 或请求任何模型服务。
 */
export default function AgentDrawer({ agents, services, initialServiceId, onSave, onDelete, onCheck, onDirtyChange, onClose }: AgentDrawerProps) {
  const [selectedId, setSelectedId] = useState(agents[0]?.id ?? '');
  const [draft, setDraft] = useState<AgentProfile>(() => agents[0] ? { ...agents[0], args: [...agents[0].args], env: { ...agents[0].env } } : emptyAgent());
  const [argsText, setArgsText] = useState(draft.args.join('\n'));
  const [envRows, setEnvRows] = useState<AgentEnvRow[]>(Object.entries(draft.env).map(([key, value]) => ({ key, value })));
  const [serviceId, setServiceId] = useState(initialServiceId ?? '');
  const [availability, setAvailability] = useState<AgentAvailability | null>(null);
  const [checking, setChecking] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [savedFingerprint, setSavedFingerprint] = useState(() => fingerprintAgent(agents[0] ?? emptyAgent(), (agents[0]?.args ?? []).join('\n'), Object.entries(agents[0]?.env ?? {}).map(([key, value]) => ({ key, value }))));
  const [deletePrompt, setDeletePrompt] = useState(false);
  const [pendingAction, setPendingAction] = useState<PendingAgentAction | null>(null);
  const drawerRef = useRef<HTMLElement>(null);
  const isDirty = fingerprintAgent(draft, argsText, envRows) !== savedFingerprint;
  const preset = agentPresets.find(item => matchesAgentPreset(draft, item));
  useEffect(() => trapDialogFocus(drawerRef), []);
  useLayoutEffect(() => {
    onDirtyChange(isDirty);
    return () => onDirtyChange(false);
  }, [isDirty, onDirtyChange]);

  function loadAgent(agent: AgentProfile) {
    const copy = { ...agent, args: [...agent.args], env: { ...agent.env } };
    setDraft(copy);
    setArgsText(copy.args.join('\n'));
    setEnvRows(Object.entries(copy.env).map(([key, value]) => ({ key, value })));
    setSavedFingerprint(fingerprintAgent(copy, copy.args.join('\n'), Object.entries(copy.env).map(([key, value]) => ({ key, value }))));
    setAvailability(null);
    setError('');
  }

  const startNew = (name = '', executable = '') => {
    setSelectedId('');
    const next = emptyAgent(name);
    next.executable = executable;
    setDraft(next);
    setArgsText('');
    setEnvRows([]);
    setSavedFingerprint(fingerprintAgent(next, '', []));
    setAvailability(null);
    setError('');
  };

  const requestExisting = (agent: AgentProfile) => {
    if (agent.id === selectedId) return;
    if (isDirty) setPendingAction({ kind: 'existing', agent });
    else { setSelectedId(agent.id); loadAgent(agent); }
  };

  const requestNew = (name = '', executable = '') => {
    if (isDirty) setPendingAction({ kind: 'new', name, executable });
    else startNew(name, executable);
  };

  const requestClose = () => {
    if (isDirty) setPendingAction({ kind: 'close' });
    else onClose();
  };

  const discardAndContinue = () => {
    const action = pendingAction;
    setPendingAction(null);
    if (!action) return;
    if (action.kind === 'close') onClose();
    else if (action.kind === 'new') startNew(action.name, action.executable);
    else { setSelectedId(action.agent.id); loadAgent(action.agent); }
  };

  const save = async () => {
    const name = draft.name.trim();
    const executable = draft.executable.trim();
    if (!name) return setError('请填写工具名称。');
    if (!executable) return setError('请填写可执行程序名称或路径。');
    const env: Record<string, string> = {};
    for (const row of envRows) {
      const key = row.key.trim();
      if (!key && !row.value.trim()) continue;
      if (!key) return setError('环境变量有值但缺少名称。');
      if (Object.prototype.hasOwnProperty.call(env, key)) return setError(`环境变量“${key}”重复。`);
      env[key] = row.value;
    }
    const payload: AgentProfile = {
      ...draft,
      name,
      executable,
      args: argsText.split(/\r?\n/).map((line) => line.trim()).filter(Boolean),
      env,
    };
    setSaving(true);
    setError('');
    try {
      const saved = await onSave(payload);
      setSelectedId(saved.id);
      loadAgent(saved);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setSaving(false);
    }
  };

  const check = async () => {
    if (!draft.id || isDirty) {
      setError('请先保存工具配置，再进行检测。');
      return;
    }
    setChecking(true);
    setAvailability(null);
    setError('');
    try {
      setAvailability(await onCheck(draft.id, serviceId || undefined));
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setChecking(false);
    }
  };

  const remove = async () => {
    if (!draft.id) {
      startNew();
      setDeletePrompt(false);
      return;
    }
    try {
      await onDelete(draft.id);
      const next = agents.filter((agent) => agent.id !== draft.id)[0];
      if (next) {
        setSelectedId(next.id);
        loadAgent(next);
      } else startNew();
      setDeletePrompt(false);
    } catch (reason) {
      setDeletePrompt(false);
      setError(reason instanceof Error ? reason.message : String(reason));
    }
  };

  return (
    <>
      <div className="drawer-scrim" onMouseDown={(event) => event.target === event.currentTarget && requestClose()}>
        <aside ref={drawerRef} className="agent-drawer" role="dialog" aria-modal="true" aria-labelledby="agent-drawer-title">
          <header className="drawer-header">
            <div>
              <h2 id="agent-drawer-title">AI 工具</h2>
              <p>配置本机 CLI 程序，检测结果只反映当前电脑是否可用。</p>
            </div>
            <button className="icon-button" type="button" aria-label="关闭 AI 工具设置" title="关闭" onClick={requestClose}><X size={18} /></button>
          </header>

          <div className="agent-layout">
            <nav className="agent-list" aria-label="已配置的 AI 工具">
              <div className="agent-list-heading"><span>工具列表</span><button className="icon-button small" type="button" aria-label="添加自定义工具" title="添加自定义工具" onClick={() => requestNew()}><Plus size={16} /></button></div>
              {agents.length === 0 && <p className="agent-list-empty">还没有配置 AI 工具。</p>}
              {agents.map((agent) => (
                <button key={agent.id} type="button" className={`agent-list-item ${selectedId === agent.id ? 'selected' : ''}`} onClick={() => requestExisting(agent)}>
                  <Bot size={16} /><span>{agent.name}</span>
                </button>
              ))}
              <div className="agent-templates">
                <span>添加常用工具</span>
                {agentPresets.filter((template) => !agents.some((agent) => matchesAgentPreset(agent, template))).map((template) => (
                  <button type="button" key={template.name} onClick={() => requestNew(template.name, template.executable)}>
                    <Plus size={13} /> {template.name}
                  </button>
                ))}
                <button type="button" onClick={() => requestNew('自定义工具')}><Plus size={13} /> 自定义工具</button>
              </div>
            </nav>

            <div className="agent-form-area">
              <div className="agent-form-title">
                <div><span className="eyebrow">工具配置</span><h3>{draft.name || '新工具'}</h3></div>
                {draft.id && <button className="icon-button danger-quiet" type="button" aria-label="删除此工具" title="删除此工具" onClick={() => setDeletePrompt(true)}><Trash2 size={16} /></button>}
              </div>
              {preset && <p className="agent-preset-description">{preset.description}</p>}
              <div className="form-grid two-columns">
                <label className="field"><span>显示名称 <b aria-hidden="true">*</b></span><input autoFocus value={draft.name} onChange={(event) => setDraft((value) => ({ ...value, name: event.target.value }))} placeholder="例如：Codex" /></label>
                <label className="field"><span>可执行程序 <b aria-hidden="true">*</b></span><input className="mono-input" value={draft.executable} onChange={(event) => setDraft((value) => ({ ...value, executable: event.target.value }))} placeholder="例如：codex" /></label>
              </div>
              <label className="field">
                <span>启动参数</span>
                <textarea className="mono-input" rows={5} value={argsText} onChange={(event) => setArgsText(event.target.value)} placeholder={'每行一个参数\n例如：--model\no4-mini'} />
                <small>每行作为一个独立参数传给程序，不会经过 shell 拼接。</small>
              </label>
              <div className="field">
                <div className="section-heading section-heading-action"><span>环境变量</span><button className="text-button" type="button" onClick={() => setEnvRows((rows) => [...rows, { key: '', value: '' }])}><Plus size={15} /> 添加</button></div>
                {envRows.length === 0 ? <p className="inline-empty">未添加环境变量。</p> : <div className="env-editor">
                  {envRows.map((row, index) => <div className="env-edit-row" key={`${index}-${row.key}`}>
                    <input aria-label={`AI 工具环境变量第 ${index + 1} 行名称`} value={row.key} onChange={(event) => setEnvRows((rows) => rows.map((item, itemIndex) => itemIndex === index ? { ...item, key: event.target.value } : item))} placeholder="名称" />
                    <input aria-label={`AI 工具环境变量第 ${index + 1} 行值`} value={row.value} onChange={(event) => setEnvRows((rows) => rows.map((item, itemIndex) => itemIndex === index ? { ...item, value: event.target.value } : item))} placeholder="值" />
                    <button className="icon-button danger-quiet" type="button" aria-label={`删除 AI 工具环境变量第 ${index + 1} 行`} onClick={() => setEnvRows((rows) => rows.filter((_, itemIndex) => itemIndex !== index))}><Trash2 size={16} /></button>
                  </div>)}
                </div>}
              </div>

              <div className="agent-check-card">
                <div><strong>可用性检测</strong><p>{serviceId ? '可结合当前服务的环境与程序搜索路径检测。' : '检测系统环境中的可执行程序。'}</p></div>
                <label className="field compact-field"><span>关联服务（可选）</span><Select value={serviceId} onChange={setServiceId} options={[{ value: '', label: '系统环境' }, ...services.map((item) => ({ value: item.id, label: item.name }))]} /></label>
                <button type="button" className="button secondary" onClick={() => void check()} disabled={checking || !draft.id || isDirty}><RefreshCw size={15} className={checking ? 'spin-icon' : ''} />{checking ? '检测中…' : '重新检测'}</button>
                {availability && <div className={`availability-result ${availability.available ? 'available' : 'unavailable'}`} role="status">
                  <span className="status-dot" />
                  <div><strong>{availability.available ? '已找到可执行程序' : '无法启动所选 Agent'}</strong><p>{availability.available ? (availability.resolvedPath ?? '检测通过。') : (availability.error ?? '未找到可执行程序，请安装对应 CLI，或在 AI 工具设置中指定程序路径。')}</p></div>
                </div>}
              </div>
              {error && <div className="form-error" role="alert">{error}</div>}
            </div>
          </div>

          <footer className="drawer-footer">
            <span>不会自动安装工具或创建账号</span>
            <div>
              <button className="button secondary" type="button" onClick={requestClose}>关闭</button>
              <button className="button primary" type="button" onClick={() => void save()} disabled={saving}>{saving ? <><span className="spinner" /> 保存中…</> : <><Check size={16} /> 保存工具</>}</button>
            </div>
          </footer>
        </aside>
      </div>
      {deletePrompt && <Modal title="删除 AI 工具配置？" tone="danger" onCancel={() => setDeletePrompt(false)} actions={[
        { label: '取消', onClick: () => setDeletePrompt(false), variant: 'secondary', autoFocus: true },
        { label: '删除配置', onClick: () => void remove(), variant: 'danger' },
      ]}><p>“{draft.name || '此工具'}”的本机配置将被删除。不会卸载该程序。</p></Modal>}
      {pendingAction && <Modal title="放弃 AI 工具配置修改？" onCancel={() => setPendingAction(null)} actions={[
        { label: '继续编辑', onClick: () => setPendingAction(null), variant: 'secondary', autoFocus: true },
        { label: '放弃并继续', onClick: discardAndContinue, variant: 'danger' },
      ]}><p>当前工具配置有未保存的修改，继续后这些修改会丢失。</p></Modal>}
    </>
  );
}
