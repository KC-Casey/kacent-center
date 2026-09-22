import { Fragment, useMemo, useState } from 'react';
import { ArrowLeft, ArrowUpRight, ChevronDown, ChevronRight, CircleAlert, Cpu, ExternalLink, RefreshCw, Search, Shield, ShieldAlert } from 'lucide-react';
import type { DiscoveredProcess, DiscoverySnapshot, PortInfo, ServiceConfig } from '../types';
import { groupDiscoveredProcesses, processIdentityKey, type DiscoveryGroupCategory, type DiscoveryProcessGroup } from '../discoveryGroups';

export type DiscoveryCategory = 'system' | 'nonSystem' | 'unknown';

interface Props {
  snapshot: DiscoverySnapshot | null;
  isDesktop: boolean;
  refreshing: boolean;
  query: string;
  categories: DiscoveryCategory[];
  selected: DiscoveredProcess | null;
  services: ServiceConfig[];
  onQueryChange: (value: string) => void;
  onRefresh: () => void;
  onSelect: (process: DiscoveredProcess) => void;
  onBack: () => void;
  onAttach: (process: DiscoveredProcess) => void;
  onOpenManaged: (serviceId: string) => void;
}

const categoryLabel: Record<DiscoveryCategory, string> = {
  system: '系统',
  nonSystem: '非系统',
  unknown: '待确认',
};

function formatAddressPort(address: string, port: number): string {
  return address.includes(':') && !address.startsWith('[') ? `[${address}]:${port}` : `${address}:${port}`;
}

function hasUsableIdentity(instance: DiscoveredProcess['instance']): boolean {
  return Number.isInteger(instance.identity?.pid) && instance.identity.pid > 0 && Boolean(instance.identity.createdAt);
}

function processMatches(process: DiscoveredProcess, query: string): boolean {
  if (!query) return true;
  const { instance, ports } = process;
  const searchable = [
    instance.name,
    instance.executable ?? '',
    instance.workingDir ?? '',
    String(instance.identity.pid),
    instance.category,
    ...instance.reasons,
    ...ports.flatMap((port) => [port.address, String(port.port), port.protocol, port.pid === null ? '' : String(port.pid)]),
  ].join(' ').toLocaleLowerCase();
  return searchable.includes(query);
}

function sameProcess(a: DiscoveredProcess, b: DiscoveredProcess): boolean {
  return processIdentityKey(a.instance.identity) === processIdentityKey(b.instance.identity);
}

function PortBadges({ ports }: { ports: PortInfo[] }) {
  if (!ports.length) return <span className="discovery-no-ports">尚未关联端口</span>;
  return <div className="discovery-port-list">
    {ports.map((port, index) => <span className={`discovery-port-chip ${port.protocol.toLowerCase()}`} key={`${port.protocol}:${port.address}:${port.port}:${index}`}>
      <small>{port.protocol === 'TCP' ? '监听' : '绑定'} {port.protocol}</small>
      <code>{formatAddressPort(port.address, port.port)}</code>
      <small>{port.pid === null ? 'PID 未知' : `PID ${port.pid}`}</small>
    </span>)}
  </div>;
}

function GroupPortBadges({ ports }: { ports: DiscoveryProcessGroup['ports'] }) {
  if (!ports.length) return <span className="discovery-no-ports">尚未关联端口</span>;
  return <div className="discovery-port-list discovery-group-port-list">
    {ports.map((port) => <span className={`discovery-port-chip ${port.protocol.toLowerCase()}`} key={`${port.protocol}:${port.port}`}>
      <small>{port.protocol === 'TCP' ? '监听' : '绑定'} {port.protocol}</small>
      <code>{port.addresses.map((address) => formatAddressPort(address, port.port)).join('、')}</code>
      <small>所属 PID {port.ownerPids.join('、')}</small>
    </span>)}
  </div>;
}

function classificationLabel(category: DiscoveryGroupCategory): string {
  return category === 'mixed' ? '混合分类' : categoryLabel[category];
}

function groupReasons(group: DiscoveryProcessGroup): string[] {
  return [...new Set(group.members.flatMap((member) => member.instance.reasons))].slice(0, 3);
}

function groupDomId(key: string): string {
  return `discovery-members-${encodeURIComponent(key)}`;
}

function ManagedGroupState({ group, serviceById }: { group: DiscoveryProcessGroup; serviceById: Map<string, ServiceConfig> }) {
  const managedMembers = group.members.filter((member) => Boolean(member.managedServiceId));
  if (!managedMembers.length) return <span className="external-state"><ExternalLink size={12} />未接入</span>;

  const serviceNames = [...new Set(managedMembers.map((member) => {
    const id = member.managedServiceId!;
    return serviceById.get(id)?.name ?? id;
  }))];
  const count = `${managedMembers.length}/${group.members.length} 个进程`;
  if (managedMembers.length < group.members.length) {
    return <span className="partial-managed-state"><strong>部分已管理</strong><small>{count}</small></span>;
  }
  return <span className="managed-state"><strong>已管理</strong><span title={serviceNames.join('、')}>{serviceNames.length === 1 ? serviceNames[0] : `${serviceNames.length} 条服务记录`}</span></span>;
}

function memberCategory(member: DiscoveredProcess) {
  const category = member.instance.category;
  return <span className={`classification-badge ${category}`}><span />{categoryLabel[category]}</span>;
}

/**
 * 发现页将后端确认属于同一服务组的进程合并显示，并折叠重复的协议与端口。
 * 组只用于理解端口关系；详情、接入和服务记录操作都继续针对具体的原始进程。
 */
export default function DiscoveryView({ snapshot, isDesktop, refreshing, query, categories, selected, services, onQueryChange, onRefresh, onSelect, onBack, onAttach, onOpenManaged }: Props) {
  const normalizedQuery = query.trim().toLocaleLowerCase();
  const [expandedGroups, setExpandedGroups] = useState<Record<string, boolean>>({});

  // 先按分类隐藏进程，再组成服务组，防止默认隐藏的系统进程因同组关系出现在结果中。
  const categoryProcesses = useMemo(() => (snapshot?.processes ?? []).filter((process) => categories.includes(process.instance.category)), [snapshot, categories]);
  const categoryGroups = useMemo(() => groupDiscoveredProcesses(categoryProcesses), [categoryProcesses]);
  const visibleGroups = useMemo(() => categoryGroups.filter((group) => {
    // 搜索任一成员命中后显示该组所有分类可见成员及其完整端口汇总。
    return group.members.some((process) => processMatches(process, normalizedQuery));
  }), [categoryGroups, normalizedQuery]);
  const visibleUnattributedPorts = useMemo(() => (snapshot?.unattributedPorts ?? []).filter((port) => {
    // 无法关联到进程的端口归入“待确认”；隐藏该分类时也一并隐藏此组结果。
    if (!categories.includes('unknown')) return false;
    if (!normalizedQuery) return true;
    return `${port.address} ${port.port} ${port.protocol} ${port.pid ?? ''}`.toLocaleLowerCase().includes(normalizedQuery);
  }), [snapshot, normalizedQuery, categories]);
  const serviceById = useMemo(() => new Map(services.map((service) => [service.id, service])), [services]);
  const selectedService = selected?.managedServiceId ? serviceById.get(selected.managedServiceId) : undefined;
  const selectedIsVisible = Boolean(selected && snapshot?.processes.some((process) => sameProcess(process, selected)));
  const selectedGroup = useMemo(() => selected
    ? categoryGroups.find((group) => group.members.some((member) => sameProcess(member, selected)))
    : undefined, [categoryGroups, selected]);
  const canAttachSelected = Boolean(selected && !selected.managedServiceId && hasUsableIdentity(selected.instance));
  const visibleProcessCount = visibleGroups.reduce((count, group) => count + group.members.length, 0);
  const visibleUniquePortCount = visibleGroups.reduce((count, group) => count + group.ports.length, 0);
  const isStaticDemo = Boolean(snapshot?.error?.startsWith('静态演示数据：'));

  if (selected) {
    const { instance } = selected;
    return <section className="discovery-page discovery-detail-page">
      <header className="discovery-page-header">
        <div className="discovery-title-group">
          <button className="icon-button discovery-back-button" type="button" aria-label="返回发现列表" title="返回列表" onClick={onBack}><ArrowLeft size={17} /></button>
          <div><span className="eyebrow">发现服务 / 进程详情</span><h1>{instance.name || '未知进程'}</h1><p>{isStaticDemo ? '静态演示进程详情，不代表本机正在运行的进程' : selectedIsVisible ? '来自当前本机进程与端口快照' : '此进程已不在最新扫描结果中，以下资料来自上次快照'}</p></div>
        </div>
        <div className="discovery-header-actions"><button className="button secondary" type="button" onClick={onRefresh} disabled={!isDesktop || refreshing}><RefreshCw size={15} className={refreshing ? 'spin-icon' : ''} />{refreshing ? '刷新中…' : '刷新'}</button></div>
      </header>
      {snapshot?.stale && <div className="discovery-alert stale"><CircleAlert size={16} /><span>当前显示的是上次成功快照，刷新可能暂时失败。</span></div>}
      {snapshot?.error && <div className="discovery-alert"><CircleAlert size={16} /><span>{snapshot.error}</span></div>}
      {!isDesktop && <div className="discovery-alert preview"><Cpu size={16} /><span>真实进程发现只在桌面应用中开放；浏览器预览不会扫描或模拟本机进程。</span></div>}

      <div className="discovery-detail-grid">
        <section className="surface-card discovery-detail-card">
          <div className="card-heading"><div><h2>进程来源</h2><p>{isStaticDemo ? '静态示例记录，不代表本机正在运行的进程' : '此记录来自桌面应用读取的进程快照'}</p></div><span className="source-badge external-source"><ExternalLink size={13} />{isStaticDemo ? '演示进程' : '外部进程'}</span></div>
          <dl className="discovery-meta-grid">
            <div><dt>PID</dt><dd><code>{instance.identity.pid}</code></dd></div>
            <div><dt>分类</dt><dd>{memberCategory(selected)}</dd></div>
            <div className="full"><dt>可执行文件</dt><dd><code title={instance.executable ?? '未知'}>{instance.executable || '未知'}</code></dd></div>
            <div className="full"><dt>工作目录</dt><dd><code title={instance.workingDir ?? '未提供'}>{instance.workingDir || '未提供'}</code></dd></div>
          </dl>
          <div className="classification-reasons">
            <strong><Shield size={14} />分类依据</strong>
            {instance.reasons.length ? <ul>{instance.reasons.map((reason, index) => <li key={`${reason}-${index}`}>{reason}</li>)}</ul> : <p>当前扫描未提供分类说明。</p>}
          </div>
          {(instance.sharedHost || !instance.controllable) && <div className="discovery-readonly-note"><ShieldAlert size={15} /><span>{instance.sharedHost ? '此进程属于共享宿主，始终保持只读。' : instance.controlReason || '此进程当前不可控制。'}</span></div>}
        </section>

        <section className="surface-card discovery-detail-card discovery-detail-ports">
          <div className="card-heading"><div><h2>此 PID 的实际端口</h2><p>仅显示当前选中进程原始记录中的地址、协议与端口</p></div><span>{selected.ports.length} 项</span></div>
          <div className="discovery-detail-port-table" role="table" aria-label="当前进程实际端口">
            <div role="row" className="discovery-detail-port-head"><span role="columnheader">状态</span><span role="columnheader">地址</span><span role="columnheader">协议 / PID</span></div>
            {selected.ports.map((port, index) => <div role="row" className="discovery-detail-port-row" key={`${port.protocol}:${port.address}:${port.port}:${index}`}>
              <span role="cell">{port.protocol === 'TCP' ? '监听' : '绑定'}</span>
              <code role="cell">{formatAddressPort(port.address, port.port)}</code>
              <span role="cell"><small className={`protocol-badge ${port.protocol.toLowerCase()}`}>{port.protocol}</small><small>{port.pid === null ? 'PID 未知' : `PID ${port.pid}`}</small></span>
            </div>)}
            {selected.ports.length === 0 && <div className="discovery-empty-table">此进程没有关联到监听或绑定端口。</div>}
          </div>
          <div className="discovery-no-output"><CircleAlert size={15} /><span>{isStaticDemo ? '这是合成演示记录，没有对应的实际进程或进程输出。' : '外部进程由其他程序启动，应用没有它启动前的原始控制台输出。'}</span></div>
        </section>
      </div>

      {selectedGroup && selectedGroup.members.length > 1 && <section className="surface-card discovery-related-group">
        <div className="card-heading"><div><h2>同组进程与全部端口</h2><p>这里只汇总当前分类筛选允许显示的成员；查看、接入和服务记录均按单个 PID 操作。</p></div><span>{selectedGroup.members.length} 个进程 · {selectedGroup.ports.length} 项去重端口</span></div>
        <div className="discovery-related-group-grid">
          <div className="discovery-related-members">
            {selectedGroup.members.map((member) => {
              const managedServiceId = member.managedServiceId;
              const memberService = managedServiceId ? serviceById.get(managedServiceId) : undefined;
              return <article className={`discovery-related-member ${sameProcess(member, selected) ? 'selected' : ''}`} key={processIdentityKey(member.instance.identity)}>
                <div className="discovery-related-member-heading">
                  <button type="button" aria-pressed={sameProcess(member, selected)} onClick={() => onSelect(member)}><strong>{member.instance.name || '未知进程'}</strong><code>PID {member.instance.identity.pid}</code></button>
                  {memberCategory(member)}
                </div>
                <PortBadges ports={member.ports} />
                <div className="discovery-related-member-footer">
                  {managedServiceId ? <><span className="managed-state"><strong>已管理</strong><span>{memberService?.name ?? managedServiceId}</span></span><button className="text-button" type="button" onClick={() => onOpenManaged(managedServiceId)}>打开记录<ArrowUpRight size={13} /></button></> : <><span className="external-state"><ExternalLink size={12} />未接入</span><button className="text-button" type="button" disabled={!isDesktop || !hasUsableIdentity(member.instance)} title={!isDesktop ? '请在桌面应用接入外部进程' : '只接入此 PID'} onClick={() => onAttach(member)}>接入此 PID</button></>}
                </div>
              </article>;
            })}
          </div>
          <div className="discovery-related-all-ports"><strong>按协议与端口去重后的汇总</strong><GroupPortBadges ports={selectedGroup.ports} /></div>
        </div>
      </section>}

      <footer className="discovery-detail-actions">
        {selected.managedServiceId ? <>
          <div className="discovery-managed-copy"><strong>此进程已关联到应用服务</strong><span>{selectedService?.name ?? selected.managedServiceId}</span></div>
          <button className="button primary" type="button" onClick={() => onOpenManaged(selected.managedServiceId!)}>打开服务<ArrowUpRight size={15} /></button>
        </> : <>
          <div className="discovery-managed-copy"><strong>外部进程不会自动接管</strong><span>此操作只接入 PID {selected.instance.identity.pid}；接入后可以补充备注、目录，并明确选择允许的控制动作。</span></div>
          <button className="button primary" type="button" disabled={!canAttachSelected || !isDesktop} onClick={() => onAttach(selected)} title={!isDesktop ? '请在桌面应用接入外部进程' : !canAttachSelected ? '缺少可靠的 PID 与进程创建标识，无法安全接入' : '接入当前进程'}>接入此进程</button>
        </>}
      </footer>
    </section>;
  }

  return <section className="discovery-page">
    <header className="discovery-page-header">
      <div><span className="eyebrow">{isStaticDemo ? '静态演示结果' : isDesktop ? '本机运行状态' : '桌面进程发现'}</span><h1>发现服务</h1><p>按已确认的进程关系查看服务组与 TCP / UDP 端口，仍可逐个进程查看和接入。</p></div>
      <div className="discovery-header-actions">
        {snapshot?.updatedAt && <span className="discovery-updated">更新于 {new Date(snapshot.updatedAt).toLocaleTimeString()}</span>}
        <button className="button secondary" type="button" onClick={onRefresh} disabled={!isDesktop || refreshing}><RefreshCw size={15} className={refreshing ? 'spin-icon' : ''} />{refreshing ? '刷新中…' : '刷新'}</button>
      </div>
    </header>

    {snapshot?.stale && <div className="discovery-alert stale"><CircleAlert size={16} /><span>扫描结果已过期；保留显示上次成功发现的进程与端口。</span></div>}
    {snapshot?.error && <div className={`discovery-alert ${isDesktop ? '' : 'preview'}`}><CircleAlert size={16} /><span>{snapshot.error}</span></div>}
    {!isDesktop && !snapshot?.error && <div className="discovery-alert preview"><Cpu size={16} /><span>真实进程发现只在桌面应用中开放；浏览器预览不会扫描或模拟本机进程。</span></div>}
    {isDesktop && refreshing && !snapshot?.updatedAt && <div className="discovery-loading"><span className="spinner" /><span>正在扫描本机进程与端口…</span></div>}

    <div className="discovery-summary-row">
      <div><strong>{visibleGroups.length}</strong><span>个服务组</span></div>
      <div><strong>{visibleProcessCount}</strong><span>个进程</span></div>
      <div><strong>{visibleUniquePortCount}</strong><span>个去重端口</span></div>
      <div><strong>{visibleUnattributedPorts.length}</strong><span>个未归属端口</span></div>
      <label className="discovery-inline-search"><Search size={15} /><input aria-label="搜索端口、名称、PID 或路径" placeholder="搜索端口、名称、PID 或路径…" value={query} onChange={(event) => onQueryChange(event.target.value)} /></label>
    </div>

    <section className="discovery-table-section" aria-label="服务组与端口">
      <div className="discovery-section-heading"><div><h2>服务组与端口</h2><p>已核验的相关进程合并显示；主进程优先作为代表，同组重复端口会折叠。</p></div><span>{visibleGroups.length} 组</span></div>
      <div className="discovery-table-scroll"><table className="discovery-table"><thead><tr><th scope="col">服务组</th><th scope="col">代表 PID</th><th scope="col">分类与理由</th><th scope="col">TCP 监听 / UDP 绑定</th><th scope="col">管理情况</th><th scope="col">操作</th></tr></thead>
        <tbody>{visibleGroups.map((group) => {
          const alreadyExpanded = Boolean(expandedGroups[group.key]);
          const primaryIsManaged = Boolean(group.primary.managedServiceId);
          const primaryIsKnownRoot = processIdentityKey(group.primary.instance.identity) === group.key;
          const primaryService = group.primary.managedServiceId ? serviceById.get(group.primary.managedServiceId) : undefined;
          const reasons = groupReasons(group);
          // 可展开区的公开 DOM 标识使用可见代表 PID，避免把已筛除成员的根标识带入页面。
          const panelId = groupDomId(processIdentityKey(group.primary.instance.identity));
          return <Fragment key={group.key}>
            <tr>
              <td>
                <div className="discovery-group-name">
                  <button className="discovery-process-link" type="button" onClick={() => onSelect(group.primary)}><strong>{group.primary.instance.name || '未知进程'}</strong><code title={group.primary.instance.executable ?? ''}>{group.primary.instance.executable || '可执行文件未知'}</code><small title={group.primary.instance.workingDir ?? ''}>{group.primary.instance.workingDir || '工作目录未知'}</small></button>
                  {group.members.length > 1 && <button className="discovery-members-toggle" type="button" aria-expanded={alreadyExpanded} aria-controls={panelId} onClick={() => setExpandedGroups((current) => ({ ...current, [group.key]: !current[group.key] }))}>{alreadyExpanded ? <ChevronDown size={13} /> : <ChevronRight size={13} />}{alreadyExpanded ? '收起成员' : `查看 ${group.members.length} 个进程`}</button>}
                </div>
              </td>
              <td><code>{group.primary.instance.identity.pid}</code>{group.members.length > 1 && <small className="discovery-primary-hint">代表进程</small>}</td>
              <td><div className="discovery-classification-cell"><span className={`classification-badge ${group.category}`}><span />{classificationLabel(group.category)}</span>{reasons.length > 0 && <small title={reasons.join('；')}>{reasons.join('；')}</small>}</div></td>
              <td><GroupPortBadges ports={group.ports} /></td>
              <td><ManagedGroupState group={group} serviceById={serviceById} /></td>
              <td><div className="discovery-row-actions"><button className="text-button" type="button" onClick={() => onSelect(group.primary)}>详情</button>{primaryIsManaged ? <button className="icon-button small" type="button" aria-label={`打开服务记录 ${primaryService?.name ?? group.primary.managedServiceId}`} title={group.members.length === 1 ? '打开服务记录' : primaryIsKnownRoot ? '打开主进程服务记录' : '打开代表进程服务记录'} onClick={() => onOpenManaged(group.primary.managedServiceId!)}><ArrowUpRight size={15} /></button> : <button className="text-button" type="button" disabled={!isDesktop || !hasUsableIdentity(group.primary.instance)} title={!isDesktop ? '请在桌面应用接入外部进程' : '只接入此代表进程'} onClick={() => onAttach(group.primary)}>{group.members.length === 1 ? '接入服务' : primaryIsKnownRoot ? '接入主进程' : '接入代表进程'}</button>}</div></td>
            </tr>
            {alreadyExpanded && group.members.length > 1 && <tr className="discovery-expanded-row"><td colSpan={6}><div className="discovery-members-panel" id={panelId}>
              <div className="discovery-members-panel-heading"><strong>组内进程</strong><span>每个 PID 单独查看、接入或打开服务记录</span></div>
              {group.members.map((member) => {
                const managedServiceId = member.managedServiceId;
                const memberService = managedServiceId ? serviceById.get(managedServiceId) : undefined;
                return <div className="discovery-member-row" key={processIdentityKey(member.instance.identity)}>
                  <div className="discovery-member-identity"><button type="button" onClick={() => onSelect(member)}><strong>{member.instance.name || '未知进程'}</strong><code>PID {member.instance.identity.pid}</code></button>{memberCategory(member)}</div>
                  <div className="discovery-member-ports"><PortBadges ports={member.ports} /></div>
                  <div className="discovery-member-state">{managedServiceId ? <span className="managed-state"><strong>已管理</strong><span>{memberService?.name ?? managedServiceId}</span></span> : <span className="external-state"><ExternalLink size={12} />未接入</span>}</div>
                  <div className="discovery-member-actions"><button className="text-button" type="button" onClick={() => onSelect(member)}>详情</button>{managedServiceId ? <button className="text-button" type="button" onClick={() => onOpenManaged(managedServiceId)}>打开记录</button> : <button className="text-button" type="button" disabled={!isDesktop || !hasUsableIdentity(member.instance)} title={!isDesktop ? '请在桌面应用接入外部进程' : '只接入此 PID'} onClick={() => onAttach(member)}>接入此 PID</button>}</div>
                </div>;
              })}
            </div></td></tr>}
          </Fragment>;
        })}
        {visibleGroups.length === 0 && <tr><td colSpan={6}><div className="discovery-empty-state"><Cpu size={23} /><strong>{isDesktop ? '没有符合筛选条件的进程' : '发现功能仅限桌面应用'}</strong><span>{isDesktop ? '尝试调整分类或搜索条件；系统与待确认进程也可在筛选器中查看。' : '桌面应用会读取真实本机进程；浏览器预览不扫描也不生成演示进程。'}</span></div></td></tr>}
        </tbody></table></div>
    </section>

    {visibleUnattributedPorts.length > 0 && <section className="discovery-table-section unattributed-section" aria-label="未归属端口">
      <div className="discovery-section-heading"><div><h2>未归属端口</h2><p>扫描到端口，但暂时无法可靠关联到进程。</p></div><span>{visibleUnattributedPorts.length} 项</span></div>
      <div className="discovery-unattributed-list">{visibleUnattributedPorts.map((port, index) => <div className="discovery-unattributed-row" key={`${port.protocol}:${port.address}:${port.port}:${index}`}><span>{port.protocol === 'TCP' ? '监听' : '绑定'}</span><small className={`protocol-badge ${port.protocol.toLowerCase()}`}>{port.protocol}</small><code>{formatAddressPort(port.address, port.port)}</code><small>{port.pid === null ? 'PID 待确认' : `PID ${port.pid}`}</small></div>)}</div>
    </section>}

    {isDesktop && snapshot?.processes.length === 0 && visibleUnattributedPorts.length === 0 && !snapshot.error && !refreshing && <div className="discovery-empty-state discovery-no-data"><Cpu size={23} /><strong>暂未发现监听服务</strong><span>扫描结果只包含当前有 TCP 监听或 UDP 绑定的进程。</span></div>}
  </section>;
}
