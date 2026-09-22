import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { ChevronDown, Expand, GripHorizontal, Plus, TerminalSquare, X, Minimize2 } from 'lucide-react';
import type { TerminalSession } from '../types';
import { activateTerminal, getTerminal, resizeTerminal, updateTerminalThemes } from '../terminals';
import { isDesktop } from '../api';
import './terminal.css';

interface Props { serviceId: string; sessions: TerminalSession[]; theme: 'dark' | 'light'; onCreate: () => void; onClose: (sessionId: string) => void; createDisabled?: boolean; createDisabledReason?: string; initiallyCollapsed?: boolean }
const selectedSessions = new Map<string, string>();
export default function TerminalPanel({ serviceId, sessions, theme, onCreate, onClose, createDisabled = false, createDisabledReason, initiallyCollapsed = false }: Props) {
  const [selected, setSelected] = useState(selectedSessions.get(serviceId) ?? '');
  const [height, setHeight] = useState(() => Math.min(340, Math.max(180, window.innerHeight - 500)));
  const [expanded, setExpanded] = useState(false);
  const [collapsed, setCollapsed] = useState(initiallyCollapsed);
  const host = useRef<HTMLDivElement>(null);
  const seen = useRef(new Set<string>());
  const active = sessions.find(session => session.id === selected) ?? sessions.at(-1);
  useEffect(() => {
    // 窗口缩小时先让出正文空间；用户仍可拖动分隔线或展开终端来主动扩大会话区域。
    const resized = () => setHeight(value => Math.min(value, Math.max(180, window.innerHeight - 500)));
    window.addEventListener('resize', resized);
    return () => window.removeEventListener('resize', resized);
  }, []);
  useEffect(() => { setSelected(selectedSessions.get(serviceId) ?? ''); setCollapsed(initiallyCollapsed); setExpanded(false); seen.current = new Set(); }, [serviceId, initiallyCollapsed]);
  useEffect(() => {
    const added = sessions.filter(session => !seen.current.has(session.id));
    if (added.length && seen.current.size) { const id = added.at(-1)!.id; setSelected(id); selectedSessions.set(serviceId, id); setCollapsed(false); }
    seen.current = new Set(sessions.map(session => session.id));
  }, [sessions, serviceId]);
  useEffect(() => updateTerminalThemes(theme), [theme]);
  useEffect(() => { if (active) getTerminal(active, theme); }, [active, theme]);
  useLayoutEffect(() => {
    if (!active || !host.current) return;
    const entry = getTerminal(active, theme); const target = host.current;
    activateTerminal(entry);
    target.replaceChildren(entry.host);
    const fit = () => resizeTerminal(entry);
    const observer = new ResizeObserver(fit); observer.observe(target);
    fit(); entry.terminal.focus();
    return () => { observer.disconnect(); entry.host.remove(); };
  }, [active?.id, serviceId]);

  return <section className={`kc-terminal-panel ${collapsed ? 'is-collapsed' : ''}`} style={{ height: collapsed ? 43 : expanded ? 'calc(100vh - 215px)' : height, minHeight: collapsed ? 0 : undefined }} aria-label="服务终端">
    {!collapsed && <div className="kc-terminal-resizer" role="separator" aria-label="调整终端高度" aria-orientation="horizontal" aria-valuenow={height} aria-valuemin={160} aria-valuemax={Math.max(200, window.innerHeight - 260)} tabIndex={0}
      onKeyDown={event => { if (event.key === 'ArrowUp' || event.key === 'ArrowDown') { event.preventDefault(); setHeight(value => Math.max(160, Math.min(window.innerHeight - 260, value + (event.key === 'ArrowUp' ? 20 : -20)))); } }}
      onPointerDown={event => {
        event.preventDefault(); setExpanded(false);
        const start = event.clientY; const initial = height; const target = event.currentTarget; target.setPointerCapture(event.pointerId);
        const move = (moveEvent: PointerEvent) => setHeight(Math.max(160, Math.min(window.innerHeight - 260, initial + start - moveEvent.clientY)));
        const finish = () => { target.removeEventListener('pointermove', move); target.removeEventListener('pointerup', finish); target.removeEventListener('pointercancel', finish); };
        target.addEventListener('pointermove', move); target.addEventListener('pointerup', finish); target.addEventListener('pointercancel', finish);
      }}><GripHorizontal size={18} /></div>}
    <div className="kc-terminal-heading"><div><strong>终端</strong><span>{sessions.length} 个会话</span></div><div>
      <button onClick={onCreate} disabled={createDisabled} title={createDisabledReason}><Plus size={15} />新建终端</button>
      {!collapsed && <button className="kc-terminal-icon" onClick={() => setExpanded(value => !value)} aria-label={expanded ? '还原终端高度' : '展开终端'}>{expanded ? <Minimize2 size={16} /> : <Expand size={16} />}</button>}
      <button className="kc-terminal-icon" onClick={() => setCollapsed(value => !value)} aria-label={collapsed ? '展开终端面板' : '收起终端面板'}>{collapsed ? <Expand size={16} /> : <ChevronDown size={16} />}</button>
    </div></div>
    {!collapsed && !!sessions.length && <div className="kc-terminal-tabs" role="tablist" aria-label="终端会话">{sessions.map(session => <div className={`kc-terminal-tab ${active?.id === session.id ? 'selected' : ''}`} key={session.id}>
      <button role="tab" aria-selected={active?.id === session.id} onClick={() => { setSelected(session.id); selectedSessions.set(serviceId, session.id); if (active?.id === session.id) activateTerminal(getTerminal(session, theme)); }}>{session.title}{session.state === 'exited' && <small>已结束</small>}</button>
      {session.kind !== 'service' && <button className="kc-terminal-close" aria-label={`关闭 ${session.title}`} onClick={() => onClose(session.id)}><X size={13} /></button>}
    </div>)}</div>}
    {!collapsed && (active ? <><div className="kc-terminal-context">{active.kind === 'service' ? '服务输出' : active.kind === 'agent' ? '独立 Agent 会话' : '独立终端'} / {active.title}<span>{active.pid ? `PID ${active.pid}` : ''}</span></div><div className="kc-terminal-host" ref={host} /></>
      : <div className="kc-terminal-empty"><TerminalSquare size={30} /><strong>为这个服务打开终端</strong><p>{createDisabledReason || (isDesktop ? '运行服务、打开独立终端，或从上方启动 AI Agent。' : '界面预览不连接本机进程，请在桌面应用中使用终端。')}</p><button onClick={onCreate} disabled={createDisabled} title={createDisabledReason}><Plus size={15} />新建终端</button></div>)}
  </section>;
}
