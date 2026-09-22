import { useMemo, useState } from 'react';
import { AppWindow, ArrowUpRight, CircleAlert, Cpu, ExternalLink, RefreshCw, Search, Shield, ShieldAlert } from 'lucide-react';
import type { ApplicationSnapshot, DiscoveredApplication, ServiceConfig } from '../types';
import type { DiscoveryCategory } from './DiscoveryView';

interface Props {
  snapshot: ApplicationSnapshot | null;
  isDesktop: boolean;
  refreshing: boolean;
  query: string;
  categories: DiscoveryCategory[];
  services: ServiceConfig[];
  onQueryChange: (value: string) => void;
  onRefresh: () => void;
  onAttach: (application: DiscoveredApplication) => void;
  onOpenManaged: (serviceId: string) => void;
}

const categoryText: Record<DiscoveryCategory, string> = { system: '系统', nonSystem: '非系统', unknown: '待确认' };

function matches(application: DiscoveredApplication, query: string): boolean {
  if (!query) return true;
  const values = [application.instance.name, application.path, application.bundleId ?? '', application.instance.executable ?? '', application.instance.workingDir ?? '', String(application.instance.identity.pid), ...application.instance.reasons, ...application.members.map((member) => String(member.pid))];
  return values.join(' ').toLocaleLowerCase().includes(query);
}

function categoryFor(application: DiscoveredApplication): DiscoveryCategory {
  const category = application.instance.category;
  return category === 'system' ? 'system' : category === 'nonSystem' ? 'nonSystem' : 'unknown';
}

function hasUsableIdentity(application: DiscoveredApplication): boolean {
  return Number.isInteger(application.instance.identity?.pid) && application.instance.identity.pid > 0 && Boolean(application.instance.identity.createdAt);
}

/** 显示桌面系统实际运行的应用实例及其可核验成员；此列表不依赖端口扫描结果。 */
export default function ApplicationDiscoveryView({ snapshot, isDesktop, refreshing, query, categories, services, onQueryChange, onRefresh, onAttach, onOpenManaged }: Props) {
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const [expanded, setExpanded] = useState<Record<string, boolean>>({});
  const serviceById = useMemo(() => new Map(services.map((service) => [service.id, service])), [services]);
  const visible = useMemo(() => (snapshot?.applications ?? []).filter((application) => categories.includes(categoryFor(application)) && matches(application, normalizedQuery)), [snapshot, categories, normalizedQuery]);

  return <section className="application-discovery-page">
    <header className="discovery-page-header"><div><span className="eyebrow">应用实例发现</span><h1>运行中的应用</h1><p>按系统报告的应用实例和可核验成员查看；没有监听端口的应用也会显示。</p></div><div className="discovery-header-actions">{snapshot?.updatedAt && <span className="discovery-updated">更新于 {new Date(snapshot.updatedAt).toLocaleTimeString()}</span>}<button className="button secondary" type="button" onClick={onRefresh} disabled={!isDesktop || refreshing}><RefreshCw size={15} className={refreshing ? 'spin-icon' : ''} />{refreshing ? '刷新中…' : '刷新'}</button></div></header>

    {snapshot?.stale && <div className="discovery-alert stale"><CircleAlert size={16} /><span>应用快照已过期；以下保留最近一次成功扫描结果。</span></div>}
    {snapshot?.error && <div className={`discovery-alert ${isDesktop ? '' : 'preview'}`}><CircleAlert size={16} /><span>{snapshot.error}</span></div>}
    {!isDesktop && !snapshot?.error && <div className="discovery-alert preview"><Cpu size={16} /><span>运行中应用发现只在桌面应用开放；浏览器预览不会扫描或模拟本机应用。</span></div>}
    {isDesktop && refreshing && !snapshot?.updatedAt && <div className="discovery-loading"><span className="spinner" /><span>正在读取运行中应用…</span></div>}

    <div className="discovery-summary-row application-summary-row"><div><strong>{visible.length}</strong><span>个运行中应用</span></div><label className="discovery-inline-search"><Search size={15} /><input aria-label="搜索应用名称、路径或 PID" placeholder="搜索应用名称、路径或 PID…" value={query} onChange={(event) => onQueryChange(event.target.value)} /></label></div>

    <section className="application-discovery-list" aria-label="运行中应用列表">
      {visible.map((application) => {
        const service = application.managedServiceId ? serviceById.get(application.managedServiceId) : undefined;
        const category = categoryFor(application);
        const key = `${application.instance.identity.pid}:${application.instance.identity.createdAt}`;
        const isExpanded = Boolean(expanded[key]);
        const canAttach = hasUsableIdentity(application);
        return <article className="surface-card application-discovery-card" key={key}>
          <div className="application-discovery-heading"><div className="application-icon"><AppWindow size={19} /></div><div className="application-discovery-name"><div><h2>{application.instance.name || '未知应用'}</h2><span className={`classification-badge ${category}`}><span />{categoryText[category]}</span></div><code title={application.path}>{application.path}</code></div><div className="application-discovery-metrics"><span><strong>PID {application.instance.identity.pid}</strong><small>应用实例</small></span><span><strong>{application.windowCount}</strong><small>个窗口</small></span><span><strong>{application.members.length}</strong><small>个核验进程</small></span></div></div>
          <div className="application-discovery-meta"><span><small>Bundle ID</small><code>{application.bundleId || '未提供'}</code></span><span><small>工作目录</small><code title={application.instance.workingDir ?? ''}>{application.instance.workingDir || '未提供'}</code></span>{application.instance.reasons.length > 0 && <span className="application-reasons"><Shield size={13} />{application.instance.reasons[0]}</span>}</div>
          {(application.instance.sharedHost || !application.instance.controllable) && <div className="discovery-readonly-note"><ShieldAlert size={15} /><span>{application.instance.sharedHost ? '共享宿主保持只读。' : application.instance.controlReason || '当前实例不可控制。'}</span></div>}
          <div className="application-discovery-footer"><button className="text-button" type="button" aria-expanded={isExpanded} onClick={() => setExpanded((previous) => ({ ...previous, [key]: !previous[key] }))}>{isExpanded ? '收起成员进程' : `查看 ${application.members.length} 个成员进程`}</button>{service ? <><span className="managed-state"><strong>已管理</strong><span>{service.name}</span></span><button className="button secondary compact" type="button" onClick={() => onOpenManaged(service.id)}>打开记录<ArrowUpRight size={14} /></button></> : <><span className="external-state"><ExternalLink size={12} />未接入</span><button className="button primary compact" type="button" disabled={!isDesktop || !canAttach} title={!isDesktop ? '请在桌面应用中接入应用' : !canAttach ? '缺少可靠的进程创建标识，无法安全关联' : '保存为桌面应用记录'} onClick={() => onAttach(application)}>接入应用</button></>}</div>
          {isExpanded && <div className="application-member-list"><strong>已核验成员</strong>{application.members.length ? application.members.map((member) => <span key={`${member.pid}:${member.createdAt}`}>PID {member.pid}<small>创建标识 {member.createdAt}</small></span>) : <small>系统未报告额外成员进程。</small>}</div>}
        </article>;
      })}
      {visible.length === 0 && <div className="application-discovery-empty"><AppWindow size={22} /><strong>{snapshot?.applications.length ? '没有符合当前筛选的应用' : '没有发现运行中的应用'}</strong><p>{snapshot?.applications.length ? '检查搜索内容或在筛选菜单中恢复分类。' : isDesktop ? '打开应用后刷新扫描；服务中心不会扫描完整的已安装应用目录。' : '请在桌面应用中查看真实运行实例。'}</p></div>}
    </section>
  </section>;
}
