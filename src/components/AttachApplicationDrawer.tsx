import { useEffect, useMemo, useRef, useState } from 'react';
import { AppWindow, Check, Folder, X } from 'lucide-react';
import type { AttachApplicationRequest, DiscoveredApplication, ServiceConfig } from '../types';
import { trapDialogFocus } from './focusTrap';

interface Props {
  application: DiscoveredApplication;
  existingServices: ServiceConfig[];
  onClose: () => void;
  onAttach: (request: AttachApplicationRequest) => Promise<void>;
  onDirtyChange: (dirty: boolean) => void;
  chooseDirectory: () => Promise<string | null>;
}

function normalizedPath(value: string): string {
  const normalized = value.replace(/[\\/]+$/, '').replaceAll('\\', '/');
  return /win/i.test(navigator.platform) ? normalized.toLocaleLowerCase() : normalized;
}

/** 将一个已发现的应用实例明确关联到既有路径记录，或以当前路径新建桌面应用服务。 */
export default function AttachApplicationDrawer({ application, existingServices, onClose, onAttach, onDirtyChange, chooseDirectory }: Props) {
  const appPath = application.path;
  const matchingService = existingServices.find((service) => service.kind === 'desktopApp'
    && service.launch.type === 'desktopApp' && normalizedPath(service.launch.path) === normalizedPath(appPath));
  const candidateName = application.instance.name || '桌面应用';
  const candidateWorkingDir = application.instance.workingDir ?? '';
  const [existingServiceId, setExistingServiceId] = useState(matchingService?.id ?? '');
  const [name, setName] = useState(matchingService?.name ?? candidateName);
  const [workingDir, setWorkingDir] = useState(matchingService?.workingDir ?? candidateWorkingDir);
  const [notes, setNotes] = useState(matchingService?.notes ?? '');
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState('');
  const drawerRef = useRef<HTMLElement>(null);
  const eligibleServices = useMemo(() => existingServices.filter((service) => service.kind === 'desktopApp'
    && service.launch.type === 'desktopApp'
    && normalizedPath(service.launch.path) === normalizedPath(appPath)), [existingServices, appPath]);
  const baseline = useRef(JSON.stringify({ existingServiceId: matchingService?.id ?? '', name: matchingService?.name ?? candidateName, workingDir: matchingService?.workingDir ?? candidateWorkingDir, notes: matchingService?.notes ?? '' }));
  const dirty = baseline.current !== JSON.stringify({ existingServiceId, name, workingDir, notes });

  useEffect(() => trapDialogFocus(drawerRef), []);
  useEffect(() => { onDirtyChange(dirty); return () => onDirtyChange(false); }, [dirty, onDirtyChange]);

  const selectDirectory = async () => {
    try {
      const selected = await chooseDirectory();
      if (selected) setWorkingDir(selected);
    } catch (reason) {
      setError(`无法选择工作目录：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const submit = async () => {
    if (!name.trim()) { setError('请填写服务名称。'); return; }
    setBusy(true);
    setError('');
    try {
      await onAttach({
        identity: application.instance.identity,
        path: application.path,
        name: name.trim(),
        notes,
        workingDir: workingDir.trim(),
        existingServiceId: existingServiceId || null,
      });
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setBusy(false);
    }
  };

  return <div className="drawer-scrim" onMouseDown={(event) => event.target === event.currentTarget && !busy && onClose()}>
    <aside ref={drawerRef} className="service-drawer attach-application-drawer" role="dialog" aria-modal="true" aria-labelledby="attach-application-title">
      <header className="drawer-header"><div><h2 id="attach-application-title">接入运行中的应用</h2><p>只关联已发现的应用实例，不会改变它当前的运行状态。</p></div><button className="icon-button" type="button" aria-label="关闭应用接入" title="关闭" onClick={onClose}><X size={18} /></button></header>
      <div className="drawer-form">
        <section className="form-section"><div className="section-heading"><h3>已核验的应用实例</h3></div><div className="application-identity-card"><div className="application-icon"><AppWindow size={18} /></div><div><strong>{application.instance.name || name}</strong><code title={application.path}>{application.path}</code><small>PID {application.instance.identity.pid} · 创建于 {application.instance.identity.createdAt}</small></div></div><div className="application-member-summary"><strong>{application.windowCount} 个窗口 · {application.members.length} 个关联进程</strong><div>{application.members.map((member) => <span key={`${member.pid}:${member.createdAt}`}>PID {member.pid}</span>)}</div></div></section>
        <section className="form-section"><div className="section-heading"><h3>管理记录</h3></div>
          <label className="field"><span>关联目标</span><select value={existingServiceId} onChange={(event) => {
            const nextId = event.target.value;
            const selectedService = eligibleServices.find((service) => service.id === nextId);
            setExistingServiceId(nextId);
            // 切换关联目标时载入已有记录的用户配置；切回新建则恢复发现快照的候选默认值。
            setName(selectedService?.name ?? candidateName);
            setWorkingDir(selectedService?.workingDir ?? candidateWorkingDir);
            setNotes(selectedService?.notes ?? '');
            setError('');
          }}><option value="">新建桌面应用服务</option>{eligibleServices.map((service) => <option key={service.id} value={service.id}>关联到“{service.name}”</option>)}</select><small>只有路径相同的桌面应用记录可以重新关联。</small></label>
          <label className="field"><span>服务名称 <b aria-hidden="true">*</b></span><input value={name} onChange={(event) => setName(event.target.value)} placeholder="例如：代码编辑器" /></label>
          <label className="field"><span>工作目录</span><div className="path-input-row"><input value={workingDir} onChange={(event) => setWorkingDir(event.target.value)} placeholder="可选：用于独立终端和 Agent" /><button className="icon-button" type="button" aria-label="选择工作目录" title="选择目录" onClick={() => void selectDirectory()}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectDirectory()}>选择</button></div><small>运行中应用快照提供的目录仅作为参考，可在这里调整。</small></label>
          <label className="field"><span>备注</span><textarea rows={4} value={notes} onChange={(event) => setNotes(event.target.value)} placeholder="记录应用用途或接入来源。" /></label>
        </section>
        {error && <div className="form-error" role="alert">{error}</div>}
      </div>
      <footer className="drawer-footer"><span>接入不会启动或停止应用</span><div><button className="button secondary" type="button" onClick={onClose} disabled={busy}>取消</button><button className="button primary" type="button" onClick={() => void submit()} disabled={busy}>{busy ? <><span className="spinner" />处理中…</> : <><Check size={16} />{existingServiceId ? '重新关联' : '接入应用'}</>}</button></div></footer>
    </aside>
  </div>;
}
