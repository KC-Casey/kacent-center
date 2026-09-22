import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { ArrowDownToLine, ChevronDown, Expand, GripHorizontal, Plus, Settings2, TerminalSquare, X, Minimize2 } from 'lucide-react';
import type { TerminalAppearance, TerminalSession } from '../types';
import { resolveTerminalAppearance } from '../terminalAppearance';
import { activateTerminal, getTerminal, resizeTerminal, updateTerminalAppearance } from '../terminals';
import { isDesktop } from '../api';
import './terminal.css';

interface Props {
  serviceId: string;
  sessions: TerminalSession[];
  appearance: TerminalAppearance;
  systemTheme: 'dark' | 'light';
  onOpenAppearance?: () => void;
  onCreate: () => void;
  onClose: (sessionId: string) => void;
  createDisabled?: boolean;
  createDisabledReason?: string;
  initiallyCollapsed?: boolean;
}
const selectedSessions = new Map<string, string>();
export default function TerminalPanel({ serviceId, sessions, appearance, systemTheme, onOpenAppearance, onCreate, onClose, createDisabled = false, createDisabledReason, initiallyCollapsed = false }: Props) {
  const [selected, setSelected] = useState(selectedSessions.get(serviceId) ?? '');
  const [height, setHeight] = useState(() => Math.min(340, Math.max(180, window.innerHeight - 500)));
  const [expanded, setExpanded] = useState(false);
  const [collapsed, setCollapsed] = useState(initiallyCollapsed);
  const [maxHeight, setMaxHeight] = useState(340);
  const [readingHistory, setReadingHistory] = useState(false);
  const panel = useRef<HTMLElement>(null);
  const host = useRef<HTMLDivElement>(null);
  const seen = useRef(new Set<string>());
  const active = sessions.find(session => session.id === selected) ?? sessions.at(-1);
  useLayoutEffect(() => {
    const workspace = panel.current?.parentElement;
    if (!workspace) return;
    // 可用高度来自工作区的内容盒，而不是整窗减固定常量。标题、错误提示、缩放
    // 都会改变工作区大小；原先展开后的终端会挤出窗口，被 app-root 裁掉末行。
    // 为上方详情保留其 CSS 最小高度，并扣除终端自身的间距。
    const observer = new ResizeObserver(([entry]) => {
      const detail = panel.current?.previousElementSibling;
      const reserved = detail ? parseFloat(getComputedStyle(detail).minHeight) || 0 : 0;
      const margin = panel.current ? parseFloat(getComputedStyle(panel.current).marginTop) || 0 : 0;
      setMaxHeight(Math.max(0, Math.floor(entry.contentRect.height - reserved - margin)));
    });
    observer.observe(workspace);
    return () => observer.disconnect();
  }, []);
  const minHeight = Math.min(160, maxHeight);
  const visibleHeight = Math.min(maxHeight, Math.max(minHeight, height));
  useEffect(() => { setSelected(selectedSessions.get(serviceId) ?? ''); setCollapsed(initiallyCollapsed); setExpanded(false); seen.current = new Set(); }, [serviceId, initiallyCollapsed]);
  useEffect(() => {
    const added = sessions.filter(session => !seen.current.has(session.id));
    if (added.length && seen.current.size) { const id = added.at(-1)!.id; setSelected(id); selectedSessions.set(serviceId, id); setCollapsed(false); }
    seen.current = new Set(sessions.map(session => session.id));
  }, [sessions, serviceId]);
  const resolvedAppearance = resolveTerminalAppearance(appearance, systemTheme);
  // 外观变更只改已有实例的 options；订阅和回滚区由 getTerminal 保持，不因草稿保存而重建。
  useEffect(() => updateTerminalAppearance(appearance, systemTheme), [appearance, systemTheme]);
  useEffect(() => { if (active) getTerminal(active, appearance, systemTheme); }, [active, appearance, systemTheme]);
  // collapsed 必须参与依赖：折叠时 host 会从 DOM 移除，展开后需重新挂载并 fit，否则会出现黑屏。
  useLayoutEffect(() => {
    if (collapsed || !active || !host.current) return;
    const entry = getTerminal(active, appearance, systemTheme); const target = host.current;
    activateTerminal(entry);
    target.replaceChildren(entry.host);
    const fit = () => resizeTerminal(entry);
    const observer = new ResizeObserver(fit); observer.observe(target);
    // 滚动状态以会话自己的缓冲区为准，切换页签不丢失历史阅读位置。
    // xterm 本身会在底部跟随输出；用户上翻后不强拉到底部，只提供恢复入口。
    let scrollFrame = 0;
    const updateScrollState = () => {
      cancelAnimationFrame(scrollFrame);
      scrollFrame = requestAnimationFrame(() => {
        const buffer = entry.terminal.buffer.active;
        setReadingHistory(buffer.type === 'normal' && buffer.viewportY < buffer.baseY);
      });
    };
    const scrolled = entry.terminal.onScroll(updateScrollState);
    const parsed = entry.terminal.onWriteParsed(updateScrollState);
    fit(); entry.terminal.focus();
    updateScrollState();
    return () => { observer.disconnect(); scrolled.dispose(); parsed.dispose(); cancelAnimationFrame(scrollFrame); entry.host.remove(); };
  }, [active?.id, serviceId, collapsed]);

  return <section ref={panel} className={`kc-terminal-panel ${collapsed ? 'is-collapsed' : ''}`} style={{ height: collapsed ? 43 : expanded ? maxHeight : visibleHeight, minHeight: collapsed ? 0 : minHeight }} aria-label="服务终端">
    {!collapsed && <div className="kc-terminal-resizer" role="separator" aria-label="调整终端高度" aria-orientation="horizontal" aria-valuenow={expanded ? maxHeight : visibleHeight} aria-valuemin={minHeight} aria-valuemax={maxHeight} tabIndex={0}
      onKeyDown={event => { if (event.key === 'ArrowUp' || event.key === 'ArrowDown') { event.preventDefault(); setExpanded(false); setHeight(Math.max(minHeight, Math.min(maxHeight, (expanded ? maxHeight : visibleHeight) + (event.key === 'ArrowUp' ? 20 : -20)))); } }}
      onPointerDown={event => {
        event.preventDefault(); setExpanded(false);
        const start = event.clientY; const initial = expanded ? maxHeight : visibleHeight; const target = event.currentTarget; target.setPointerCapture(event.pointerId);
        setHeight(initial);
        const move = (moveEvent: PointerEvent) => setHeight(Math.max(minHeight, Math.min(maxHeight, initial + start - moveEvent.clientY)));
        const finish = () => { target.removeEventListener('pointermove', move); target.removeEventListener('pointerup', finish); target.removeEventListener('pointercancel', finish); };
        target.addEventListener('pointermove', move); target.addEventListener('pointerup', finish); target.addEventListener('pointercancel', finish);
      }}><GripHorizontal size={18} /></div>}
    <div className="kc-terminal-heading"><div><strong>终端</strong><span>{sessions.length} 个会话</span></div><div>
      <button onClick={onCreate} disabled={createDisabled} title={createDisabledReason}><Plus size={15} />新建终端</button>
      {onOpenAppearance && <button className="kc-terminal-icon" onClick={onOpenAppearance} aria-label="终端外观设置" title="终端外观设置"><Settings2 size={16} /></button>}
      {!collapsed && <button className="kc-terminal-icon" onClick={() => setExpanded(value => !value)} aria-label={expanded ? '还原终端高度' : '展开终端'}>{expanded ? <Minimize2 size={16} /> : <Expand size={16} />}</button>}
      <button className="kc-terminal-icon" onClick={() => setCollapsed(value => !value)} aria-label={collapsed ? '展开终端面板' : '收起终端面板'}>{collapsed ? <Expand size={16} /> : <ChevronDown size={16} />}</button>
    </div></div>
    {!collapsed && !!sessions.length && <div className="kc-terminal-tabs" role="tablist" aria-label="终端会话">{sessions.map(session => <div className={`kc-terminal-tab ${active?.id === session.id ? 'selected' : ''}`} key={session.id}>
      <button role="tab" aria-selected={active?.id === session.id} onClick={() => { setSelected(session.id); selectedSessions.set(serviceId, session.id); if (active?.id === session.id) activateTerminal(getTerminal(session, appearance, systemTheme)); }}>{session.title}{session.state === 'exited' && <small>已结束</small>}</button>
      {session.kind !== 'service' && <button className="kc-terminal-close" aria-label={`关闭 ${session.title}`} onClick={() => onClose(session.id)}><X size={13} /></button>}
    </div>)}</div>}
    {!collapsed && (active ? <><div className="kc-terminal-context"><span className="kc-terminal-session-label">{active.kind === 'service' ? '服务输出' : active.kind === 'agent' ? '独立 Agent 会话' : '独立终端'} / {active.title}</span><div>{readingHistory && <button className="kc-terminal-latest" onClick={() => { const entry = getTerminal(active, appearance, systemTheme); entry.terminal.scrollToBottom(); entry.terminal.focus(); }}><ArrowDownToLine size={12} />回到最新输出</button>}<span>{active.pid ? `PID ${active.pid}` : ''}</span></div></div><div className="kc-terminal-host" ref={host} style={{ backgroundColor: resolvedAppearance.colors.background }} /></>
      : <div className="kc-terminal-empty" style={{ backgroundColor: resolvedAppearance.colors.background, color: resolvedAppearance.colors.foreground }}><TerminalSquare size={30} /><strong>为这个服务打开终端</strong><p>{createDisabledReason || (isDesktop ? '运行服务、打开独立终端，或从上方启动 AI Agent。' : '界面预览不连接本机进程，请在桌面应用中使用终端。')}</p><button onClick={onCreate} disabled={createDisabled} title={createDisabledReason}><Plus size={15} />新建终端</button></div>)}
  </section>;
}
