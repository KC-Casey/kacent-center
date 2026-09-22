import { useEffect, useMemo, useRef, useState, type FormEvent } from 'react';
import { Check, ExternalLink, Folder, X } from 'lucide-react';
import type { AttachExternalRequest, DiscoveredProcess, ServiceConfig } from '../types';
import Modal from './Modal';
import { trapDialogFocus } from './focusTrap';

interface Props {
  process: DiscoveredProcess;
  existingServices: ServiceConfig[];
  onClose: () => void;
  onAttach: (request: AttachExternalRequest, command: string) => Promise<void>;
  onDirtyChange: (dirty: boolean) => void;
  chooseDirectory: () => Promise<string | null>;
}

/**
 * 接入只引用后端发现返回的完整进程 identity；PID 相同但创建时间不同的实例不能串接。
 * 外部服务可以先不填目录或命令，后续需要在本机启动终端或转换托管时再补齐。
 */
export default function AttachExternalDrawer({ process, existingServices, onClose, onAttach, onDirtyChange, chooseDirectory }: Props) {
  const initialExternalService = existingServices.find((service) => service.id === process.managedServiceId) ?? null;
  const [existingServiceId, setExistingServiceId] = useState(initialExternalService?.id ?? '');
  const [name, setName] = useState(initialExternalService?.name ?? process.instance.name ?? '外部进程');
  // 重新关联旧记录时保留用户已经清空的目录；扫描到的路径只作为可选建议。
  const [workingDir, setWorkingDir] = useState(initialExternalService ? initialExternalService.workingDir : process.instance.workingDir || '');
  const [command, setCommand] = useState(initialExternalService?.command ?? '');
  const [notes, setNotes] = useState(initialExternalService?.notes ?? '');
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [discardPrompt, setDiscardPrompt] = useState(false);
  const drawerRef = useRef<HTMLElement>(null);
  const externalOptions = useMemo(() => existingServices.filter((service) => service.source === 'external'), [existingServices]);
  const initialFingerprint = useMemo(() => JSON.stringify({
    existingServiceId: initialExternalService?.id ?? '',
    name: initialExternalService?.name ?? process.instance.name ?? '外部进程',
    workingDir: initialExternalService ? initialExternalService.workingDir : process.instance.workingDir || '',
    command: initialExternalService?.command ?? '',
    notes: initialExternalService?.notes ?? '',
  }), [initialExternalService, process]);
  const currentFingerprint = JSON.stringify({ existingServiceId, name, workingDir, command, notes });
  const dirty = currentFingerprint !== initialFingerprint;
  const identityUsable = Number.isInteger(process.instance.identity?.pid) && process.instance.identity.pid > 0 && Boolean(process.instance.identity.createdAt);

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

  const chooseExisting = (serviceId: string) => {
    setExistingServiceId(serviceId);
    const service = externalOptions.find((item) => item.id === serviceId);
    if (service) {
      setName(service.name);
      setWorkingDir(service.workingDir);
      setCommand(service.command);
      setNotes(service.notes);
    } else {
      setName(process.instance.name || '外部进程');
      setWorkingDir(process.instance.workingDir || '');
      setCommand('');
      setNotes('');
    }
  };

  const selectDirectory = async () => {
    try {
      const directory = await chooseDirectory();
      if (directory) setWorkingDir(directory);
    } catch (reason) {
      setError(`无法选择目录：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    if (!identityUsable) return setError('发现结果没有可靠的进程身份，无法安全接入。');
    const normalizedName = name.trim();
    if (!normalizedName) return setError('请填写服务名称。');
    setSaving(true);
    setError('');
    try {
      await onAttach({
        identity: process.instance.identity,
        name: normalizedName,
        notes,
        workingDir: workingDir.trim(),
        existingServiceId: existingServiceId || null,
      }, command.trim());
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setSaving(false);
    }
  };

  return <>
    <div className="drawer-scrim" onMouseDown={(event) => event.target === event.currentTarget && requestClose()}>
      <aside ref={drawerRef} className="external-service-drawer attach-external-drawer" role="dialog" aria-modal="true" aria-labelledby="attach-external-title">
        <header className="drawer-header"><div><h2 id="attach-external-title">接入外部进程</h2><p>保存进程关联信息；接入不会重启现有进程。</p></div><button className="icon-button" type="button" aria-label="关闭接入表单" title="关闭" onClick={requestClose}><X size={18} /></button></header>
        <form id="attach-external-form" className="drawer-form" onSubmit={(event) => void submit(event)}>
          <section className="form-section"><div className="section-heading"><h3>发现到的进程</h3></div>
            <div className="attach-process-summary"><span className="source-badge external-source"><ExternalLink size={13} />外部进程</span><strong>{process.instance.name || '未知进程'}</strong><span>PID {process.instance.identity.pid}</span><code title={process.instance.executable ?? '未知'}>{process.instance.executable || '可执行文件未知'}</code></div>
            {process.familyIdentity && <p className="field-hint">本次关联 PID {process.instance.identity.pid}。同组进程的端口在发现列表集中展示，停止操作仍只发送给本次选中的进程。</p>}
            {!identityUsable && <div className="form-error" role="alert">缺少可靠进程身份，接入操作已禁用。</div>}
          </section>
          <section className="form-section"><div className="section-heading"><h3>应用服务记录</h3><p>可复用先前退出的外部服务记录。</p></div>
            <label className="field"><span>现有外部服务记录</span><select value={existingServiceId} onChange={(event) => chooseExisting(event.target.value)}><option value="">新建外部服务记录</option>{externalOptions.map((service) => <option key={service.id} value={service.id}>{service.name} · PID {service.external?.identity.pid ?? '未知'}</option>)}</select><small>选择现有记录会更新它关联的进程实例。</small></label>
            <label className="field"><span>显示名称 <b aria-hidden="true">*</b></span><input autoFocus value={name} onChange={(event) => setName(event.target.value)} placeholder="例如：本地开发 API" /></label>
            <label className="field"><span>工作目录</span><div className="path-input-row"><input value={workingDir} onChange={(event) => setWorkingDir(event.target.value)} placeholder="可留空，稍后补充" /><button className="icon-button" type="button" aria-label="选择工作目录" title="选择目录" onClick={() => void selectDirectory()}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectDirectory()}>选择</button></div><small>独立终端、Agent 与托管转换需要工作目录。</small>{process.instance.workingDir && process.instance.workingDir !== workingDir && <small className="external-suggestion">扫描快照建议：<code>{process.instance.workingDir}</code><button className="text-button" type="button" onClick={() => setWorkingDir(process.instance.workingDir!)}>使用建议目录</button></small>}</label>
            <label className="field"><span>启动命令</span><input className="mono-input" value={command} onChange={(event) => setCommand(event.target.value)} placeholder="可留空，转换托管前再补齐" /><small>命令只保存在记录中，不会在接入时运行。</small></label>
            <label className="field"><span>备注</span><textarea rows={4} value={notes} onChange={(event) => setNotes(event.target.value)} placeholder="记录用途、所有者或需要留意的事项。" /></label>
          </section>
          {error && <div className="form-error" role="alert">{error}</div>}
        </form>
        <footer className="drawer-footer"><span>不会重启或自动终止目标进程</span><div><button className="button secondary" type="button" onClick={requestClose} disabled={saving}>取消</button><button className="button primary" type="submit" form="attach-external-form" disabled={saving || !identityUsable}>{saving ? <><span className="spinner" />接入中…</> : <><Check size={16} />接入服务</>}</button></div></footer>
      </aside>
    </div>
    {discardPrompt && <Modal title="放弃接入信息？" onCancel={() => setDiscardPrompt(false)} actions={[
      { label: '继续编辑', onClick: () => setDiscardPrompt(false), variant: 'secondary', autoFocus: true },
      { label: '放弃', onClick: onClose, variant: 'danger' },
    ]}><p>当前接入表单有未保存修改，关闭后这些修改会丢失。</p></Modal>}
  </>;
}
