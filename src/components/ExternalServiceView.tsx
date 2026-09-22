import { Copy, ExternalLink, FolderOpen, Info, Pencil, Shield, ShieldAlert, TerminalSquare } from 'lucide-react';
import type { ServiceConfig, ServiceRuntime } from '../types';

interface Props {
  service: ServiceConfig;
  runtime: ServiceRuntime;
  isDesktop: boolean;
  onEdit: () => void;
  onCopy: (value: string, label: string) => void;
  onOpenDirectory: () => void;
  onUnlock: () => void;
  onConvert: () => void;
}

const categoryText = { system: '系统进程', nonSystem: '非系统进程', unknown: '待确认' } as const;

function formatAddressPort(address: string, port: number): string {
  return address.includes(':') && !address.startsWith('[') ? `[${address}]:${port}` : `${address}:${port}`;
}

/** 外部进程没有本应用启动的控制台输出；本视图始终把来源、权限和端口状态放在一起。 */
export default function ExternalServiceView({ service, runtime, isDesktop, onEdit, onCopy, onOpenDirectory, onUnlock, onConvert }: Props) {
  const external = service.external;
  const category = external?.category ?? 'unknown';
  // 外部扫描值只是建议；如果服务配置被用户清空，不能借外部元数据假装目录仍有效。
  const workingDir = service.workingDir;
  const suggestedWorkingDir = external?.workingDir && external.workingDir !== workingDir ? external.workingDir : '';
  const commandReady = Boolean(workingDir.trim() && service.command.trim());
  const canConvert = isDesktop && runtime.canConvert && commandReady && !external?.sharedHost;
  const canUnlock = isDesktop && runtime.canUnlock && !runtime.protectionUnlocked && !external?.sharedHost;

  return <div className="external-service-view">
    <div className="external-overview-grid">
      <section className="surface-card external-info-card">
        <div className="card-heading"><div><h2>外部进程</h2><p>由其他程序启动，接入只保存关联与配置。</p></div><span className="source-badge external-source"><ExternalLink size={13} />外部来源</span></div>
        <dl className="discovery-meta-grid external-service-meta">
          <div><dt>PID</dt><dd><code>{external?.identity.pid ?? runtime.pid ?? '未知'}</code></dd></div>
          <div><dt>分类</dt><dd><span className={`classification-badge ${category}`}><span />{categoryText[category]}</span></dd></div>
          <div className="full"><dt>可执行文件</dt><dd><code title={external?.executable ?? '未知'}>{external?.executable || '未知'}</code><button className="icon-button small" type="button" disabled={!external?.executable} aria-label="复制可执行文件路径" title="复制路径" onClick={() => external?.executable && onCopy(external.executable, '可执行文件路径')}><Copy size={14} /></button></dd></div>
          <div className="full"><dt>工作目录</dt><dd><code title={workingDir || '未设置'}>{workingDir || '未设置'}</code><span className="external-path-actions"><button className="icon-button small" type="button" disabled={!workingDir} aria-label="复制工作目录" title="复制路径" onClick={() => workingDir && onCopy(workingDir, '工作目录')}><Copy size={14} /></button><button className="icon-button small" type="button" disabled={!workingDir || !isDesktop} aria-label="打开工作目录" title={isDesktop ? '在系统中打开' : '请在桌面应用中使用'} onClick={onOpenDirectory}><FolderOpen size={15} /></button></span></dd></div>
          <div className="full"><dt>启动命令</dt><dd><code title={service.command || '未设置'}>{service.command || '未设置'}</code><button className="icon-button small" type="button" disabled={!service.command} aria-label="复制启动命令" title="复制命令" onClick={() => service.command && onCopy(service.command, '启动命令')}><Copy size={14} /></button></dd></div>
        </dl>
        {suggestedWorkingDir && <div className="external-suggested-path"><Info size={14} /><span>扫描器报告的建议目录：<code>{suggestedWorkingDir}</code>。此路径未写入服务配置。</span></div>}
        <div className="external-info-actions"><button className="text-button" type="button" onClick={onEdit}><Pencil size={14} />编辑名称、备注、目录或命令</button></div>
      </section>

      <section className="surface-card external-port-card">
        <div className="card-heading"><div><h2>实际端口</h2><p>端口由当前 PID 持有，可能随进程变化。</p></div><span>{runtime.ports.length} 项</span></div>
        {runtime.stale && <div className="external-runtime-note stale"><Info size={14} /><span>运行态快照已过期，保留最近一次成功结果。</span></div>}
        {runtime.portError && <div className="external-runtime-note"><Info size={14} /><span>端口探测失败：{runtime.portError}</span></div>}
        {runtime.error && <div className="external-runtime-note"><ShieldAlert size={14} /><span>{runtime.error}</span></div>}
        <div className="external-expected-ports"><span>转换预期端口</span><strong>{service.expectedPorts.length ? service.expectedPorts.join('、') : '未设置'}</strong><small>转换前会按这些端口检查占用；下方显示当前 PID 的实际端口。</small></div>
        <div className="discovery-detail-port-table" role="table" aria-label="外部服务实际端口">
          <div role="row" className="discovery-detail-port-head"><span role="columnheader">状态</span><span role="columnheader">地址</span><span role="columnheader">协议 / PID</span></div>
          {runtime.ports.map((port, index) => <div role="row" className="discovery-detail-port-row" key={`${port.protocol}:${port.address}:${port.port}:${index}`}>
            <span role="cell">{port.protocol === 'TCP' ? '监听' : '绑定'}</span>
            <code role="cell">{formatAddressPort(port.address, port.port)}</code>
            <span role="cell"><small className={`protocol-badge ${port.protocol.toLowerCase()}`}>{port.protocol}</small><small>{port.pid === null ? 'PID 未知' : `PID ${port.pid}`}</small></span>
          </div>)}
          {runtime.ports.length === 0 && <div className="discovery-empty-table">当前快照没有可显示的实际端口。</div>}
        </div>
        <div className="discovery-no-output"><Info size={15} /><span>此进程不是由应用启动的，应用没有它启动前的原始控制台输出。</span></div>
      </section>
    </div>

    <section className="surface-card external-control-card">
      <div className="card-heading"><div><h2>控制权限</h2><p>分类理由取自接入时快照；操作权限会按当前进程实例实时核验。</p></div><span className={`protection-state ${runtime.canStop ? 'unlocked' : ''}`}><Shield size={13} />{external?.sharedHost ? '共享宿主只读' : runtime.protectionUnlocked && runtime.canStop ? '本次运行已临时解锁' : runtime.canStop ? '当前实例允许停止' : runtime.canUnlock ? '默认受保护' : '只读'}</span></div>
      <div className="external-control-summary">
        <div><strong>分类依据</strong>{external?.reasons.length ? <ul>{external.reasons.map((reason, index) => <li key={`${reason}-${index}`}>{reason}</li>)}</ul> : <p>当前没有分类说明。</p>}</div>
        <div><strong>控制限制</strong><p>{external?.sharedHost ? '共享宿主始终只读。' : runtime.controlReason || (runtime.canStop ? '当前实例允许停止。' : '当前实例保持只读。')}</p>{external?.sharedHost && <p>共享宿主不提供解锁、停止或转换操作。</p>}</div>
      </div>
      <div className="external-control-actions">
        {canUnlock && <button className="button secondary" type="button" onClick={onUnlock}><Shield size={15} />临时解锁此实例</button>}
        {runtime.protectionUnlocked && <span className="external-unlocked-hint"><Shield size={14} />仅对当前进程实例有效；进程身份变化后需要重新确认。</span>}
        <button className="button primary" type="button" disabled={!canConvert} title={!isDesktop ? '请在桌面应用中转换' : !commandReady ? '先补齐工作目录和启动命令' : external?.sharedHost ? '共享宿主始终只读' : runtime.controlReason || '转换为平台托管并启动'} onClick={onConvert}><TerminalSquare size={15} />转换为平台托管并启动</button>
      </div>
      {!workingDir.trim() || !service.command.trim() ? <div className="external-missing-config"><Info size={14} /><span>转换前需要补齐工作目录和启动命令。编辑外部服务可填写缺少的配置。</span><button className="text-button" type="button" onClick={onEdit}>补充配置</button></div> : null}
      {external?.sharedHost && <div className="discovery-readonly-note"><ShieldAlert size={15} /><span>共享宿主始终只读；此限制不会因临时解锁而改变。</span></div>}
    </section>
  </div>;
}
