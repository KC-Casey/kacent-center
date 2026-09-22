import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { Check, Folder, Plus, Trash2, X } from 'lucide-react';
import type { ServiceConfig } from '../types';
import Modal from './Modal';
import { trapDialogFocus } from './focusTrap';

interface Props {
  service: ServiceConfig;
  onClose: () => void;
  onSave: (service: ServiceConfig) => Promise<void>;
  onDirtyChange: (dirty: boolean) => void;
  chooseDirectory: () => Promise<string | null>;
}

interface EnvironmentRow { key: string; value: string }

function parsePortList(value: string): { ports: number[]; invalid: string[] } {
  const parts = value.split(/[\s,;，；]+/).map((item) => item.trim()).filter(Boolean);
  const ports: number[] = [];
  const invalid: string[] = [];
  for (const part of parts) {
    const port = Number(part);
    if (!/^\d+$/.test(part) || !Number.isInteger(port) || port < 1 || port > 65535) invalid.push(part);
    else if (!ports.includes(port)) ports.push(port);
  }
  return { ports, invalid };
}

/**
 * 外部记录只编辑本应用保存的名称、备注与后续转换配置；空工作目录或命令可以保留，
 * 这样接入只观察的进程时无需臆造启动方式，用户也能之后补齐转换所需资料。
 */
export default function ExternalServiceDrawer({ service, onClose, onSave, onDirtyChange, chooseDirectory }: Props) {
  const [name, setName] = useState(service.name);
  const [notes, setNotes] = useState(service.notes);
  const [workingDir, setWorkingDir] = useState(service.workingDir);
  const [command, setCommand] = useState(service.command);
  const [environmentType, setEnvironmentType] = useState<ServiceConfig['environmentType']>(service.environmentType);
  const [environmentDir, setEnvironmentDir] = useState(service.environmentDir);
  const [executableDirsText, setExecutableDirsText] = useState(service.executableDirs.join('\n'));
  const [shell, setShell] = useState(service.shell);
  const [expectedPortsText, setExpectedPortsText] = useState(service.expectedPorts.join(', '));
  const [environmentRows, setEnvironmentRows] = useState<EnvironmentRow[]>(Object.entries(service.env).map(([key, value]) => ({ key, value })));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [discardPrompt, setDiscardPrompt] = useState(false);
  const drawerRef = useRef<HTMLElement>(null);
  const initialFingerprint = useMemo(() => JSON.stringify({
    name: service.name, notes: service.notes, workingDir: service.workingDir, command: service.command,
    environmentType: service.environmentType, environmentDir: service.environmentDir, executableDirsText: service.executableDirs.join('\n'),
    shell: service.shell, expectedPortsText: service.expectedPorts.join(', '), environmentRows: Object.entries(service.env).map(([key, value]) => ({ key, value })),
  }), [service]);
  const currentFingerprint = JSON.stringify({ name, notes, workingDir, command, environmentType, environmentDir, executableDirsText, shell, expectedPortsText, environmentRows });
  const dirty = initialFingerprint !== currentFingerprint;

  useEffect(() => trapDialogFocus(drawerRef), []);
  useEffect(() => {
    onDirtyChange(dirty);
    return () => onDirtyChange(false);
  }, [dirty, onDirtyChange]);

  const requestClose = () => {
    if (saving) return;
    if (dirty) setDiscardPrompt(true);
    else onClose();
  };

  const selectDirectory = async () => {
    try {
      const selected = await chooseDirectory();
      if (selected) setWorkingDir(selected);
    } catch (reason) {
      setError(`无法选择目录：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const selectEnvironmentDirectory = async () => {
    try {
      const selected = await chooseDirectory();
      if (selected) setEnvironmentDir(selected);
    } catch (reason) {
      setError(`无法选择环境目录：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const normalizedName = name.trim();
    if (!normalizedName) return setError('请填写服务名称。');
    const parsedPorts = parsePortList(expectedPortsText);
    if (parsedPorts.invalid.length) return setError(`预期端口无效：${parsedPorts.invalid.join('、')}。请输入 1–65535 的端口号。`);
    const env: Record<string, string> = {};
    for (const row of environmentRows) {
      const key = row.key.trim();
      if (!key && !row.value.trim()) continue;
      if (!key) return setError('环境变量有值但缺少名称。');
      if (Object.prototype.hasOwnProperty.call(env, key)) return setError(`环境变量“${key}”重复。`);
      env[key] = row.value;
    }
    setSaving(true);
    setError('');
    try {
      await onSave({
        ...service,
        name: normalizedName,
        notes,
        workingDir: workingDir.trim(),
        command: command.trim(),
        environmentType,
        environmentDir: environmentDir.trim(),
        executableDirs: executableDirsText.split(/\r?\n/).map((line) => line.trim()).filter(Boolean),
        shell: shell.trim(),
        expectedPorts: parsedPorts.ports,
        env,
        updatedAt: '',
      });
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setSaving(false);
    }
  };

  return <>
    <div className="drawer-scrim" onMouseDown={(event) => event.target === event.currentTarget && requestClose()}>
      <aside ref={drawerRef} className="external-service-drawer" role="dialog" aria-modal="true" aria-labelledby="external-service-drawer-title">
        <header className="drawer-header"><div><h2 id="external-service-drawer-title">编辑外部服务</h2><p>只修改应用保存的服务记录，不会改变正在运行的进程。</p></div><button className="icon-button" type="button" aria-label="关闭外部服务配置" title="关闭" onClick={requestClose}><X size={18} /></button></header>
        <form id="external-service-form" className="drawer-form" onSubmit={(event) => void submit(event)}>
          <section className="form-section"><div className="section-heading"><h3>进程身份</h3></div><div className="external-identity-summary"><span><small>来源</small><strong>外部进程</strong></span><span><small>PID</small><strong>{service.external?.identity.pid ?? '未知'}</strong></span><span><small>分类</small><strong>{service.external?.category === 'system' ? '系统' : service.external?.category === 'nonSystem' ? '非系统' : '待确认'}</strong></span></div></section>
          <section className="form-section"><div className="section-heading"><h3>服务信息</h3></div>
            <label className="field"><span>显示名称 <b aria-hidden="true">*</b></span><input autoFocus value={name} onChange={(event) => setName(event.target.value)} placeholder="例如：开发 API" /></label>
            <label className="field"><span>工作目录</span><div className="path-input-row"><input value={workingDir} onChange={(event) => setWorkingDir(event.target.value)} placeholder="可留空，稍后补充" /><button className="icon-button" type="button" aria-label="选择工作目录" title="选择目录" onClick={() => void selectDirectory()}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectDirectory()}>选择</button></div><small>创建独立终端、Agent 或转换为平台托管前需要有效工作目录。</small>{service.external?.workingDir && service.external.workingDir !== workingDir && <small className="external-suggestion">扫描快照建议：<code>{service.external.workingDir}</code><button className="text-button" type="button" onClick={() => setWorkingDir(service.external!.workingDir!)}>使用建议目录</button></small>}</label>
            <label className="field"><span>启动命令</span><input className="mono-input" value={command} onChange={(event) => setCommand(event.target.value)} placeholder="可留空，转换托管前再补齐" /><small>保存命令不会启动、停止或重启现有进程。</small></label>
            <div className="form-grid two-columns">
              <label className="field"><span>环境类型</span><select value={environmentType} onChange={(event) => setEnvironmentType(event.target.value as ServiceConfig['environmentType'])}><option value="system">系统环境</option><option value="venv">Python 虚拟环境</option><option value="custom">自定义环境</option></select></label>
              <label className="field"><span>环境目录</span><div className="path-input-row"><input value={environmentDir} onChange={(event) => setEnvironmentDir(event.target.value)} placeholder="可留空，使用系统环境" /><button className="icon-button" type="button" aria-label="选择环境目录" title="选择目录" onClick={() => void selectEnvironmentDirectory()}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectEnvironmentDirectory()}>选择</button></div></label>
            </div>
            <label className="field"><span>终端程序</span><input className="mono-input" value={shell} onChange={(event) => setShell(event.target.value)} placeholder="留空使用系统默认终端" /><small>可填写可执行程序名或完整路径。</small></label>
            <label className="field"><span>预期端口</span><input className="mono-input" value={expectedPortsText} onChange={(event) => setExpectedPortsText(event.target.value)} placeholder="例如：8080, 8081" /><small>转换后用于检查端口冲突；可留空。</small></label>
            <label className="field"><span>自定义程序目录</span><textarea rows={2} value={executableDirsText} onChange={(event) => setExecutableDirsText(event.target.value)} placeholder="每行一个目录" /></label>
            <div className="field"><div className="section-heading section-heading-action"><span>环境变量</span><button className="text-button" type="button" onClick={() => setEnvironmentRows((rows) => [...rows, { key: '', value: '' }])}><Plus size={14} />添加</button></div>
              {environmentRows.length === 0 ? <p className="inline-empty">未添加环境变量。</p> : <div className="env-editor">{environmentRows.map((row, index) => <div className="env-edit-row" key={`${index}-${row.key}`}><input aria-label={`外部服务环境变量第 ${index + 1} 行名称`} value={row.key} onChange={(event) => setEnvironmentRows((rows) => rows.map((item, itemIndex) => itemIndex === index ? { ...item, key: event.target.value } : item))} placeholder="名称" /><input aria-label={`外部服务环境变量第 ${index + 1} 行值`} value={row.value} onChange={(event) => setEnvironmentRows((rows) => rows.map((item, itemIndex) => itemIndex === index ? { ...item, value: event.target.value } : item))} placeholder="值" /><button className="icon-button danger-quiet" type="button" aria-label={`删除外部服务环境变量第 ${index + 1} 行`} onClick={() => setEnvironmentRows((rows) => rows.filter((_, itemIndex) => itemIndex !== index))}><Trash2 size={15} /></button></div>)}</div>}
            </div>
            <label className="field"><span>备注</span><textarea rows={5} value={notes} onChange={(event) => setNotes(event.target.value)} placeholder="记录进程用途、来源或后续操作说明。" /></label>
          </section>
          {error && <div className="form-error" role="alert">{error}</div>}
        </form>
        <footer className="drawer-footer"><span>外部进程保持运行</span><div><button className="button secondary" type="button" onClick={requestClose} disabled={saving}>取消</button><button className="button primary" type="submit" form="external-service-form" disabled={saving}>{saving ? <><span className="spinner" />保存中…</> : <><Check size={16} />保存修改</>}</button></div></footer>
      </aside>
    </div>
    {discardPrompt && <Modal title="放弃外部服务配置修改？" onCancel={() => setDiscardPrompt(false)} actions={[
      { label: '继续编辑', onClick: () => setDiscardPrompt(false), variant: 'secondary', autoFocus: true },
      { label: '放弃修改', onClick: onClose, variant: 'danger' },
    ]}><p>当前表单有未保存修改，关闭后这些修改会丢失。进程本身会继续运行。</p></Modal>}
  </>;
}
