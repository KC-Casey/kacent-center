import { useCallback, useEffect, useRef, useState } from 'react';
import { AppWindow, ArrowUpRight, Bot, Check, Copy, FolderOpen, Gamepad2, Library, Mic, Pencil, Pin, Play, Plus, Power, RefreshCw, Search, Send, Settings, Square, Trash2, X } from 'lucide-react';
import { isDesktop } from '../api';
import { contentApi } from '../contentApi';
import { actionText, contentKindText, contentTargetLabel, newContent, taskStateText } from '../contentTypes';
import type { ActionTask, AgentCapabilities, ContentItem, ContentStatus, PlannedAction } from '../contentTypes';
import type { AgentProfile } from '../types';
import ContentEditor from './ContentEditor';
import Modal from './Modal';
import Select from './Select';
import './content.css';

interface Props { visible: boolean; suspendDialogs?: boolean; agents: AgentProfile[]; onOpenService: (id: string) => void; onSettings: () => void; onDirtyChange?: (dirty: boolean) => void }
const pendingStates = ['awaitingConfirmation', 'awaitingSelection', 'awaitingForm'];
const activeStates = ['understanding', 'ready', 'running', ...pendingStates];
const kindIcons = { folder: FolderOpen, application: AppWindow, game: Gamepad2 };
const stateText: Record<string, string> = { available: '可用', unavailable: '目标不可用', running: '运行中', stopped: '未运行', unknown: '待确认', selectionRequired: '选择实例' };
function resultText(result: unknown): string {
  if (!result || typeof result !== 'object') return '';
  const value = result as Record<string, unknown>;
  if (typeof value.message === 'string') return value.message;
  if (typeof value.state === 'string') return stateText[value.state] || value.state;
  if (Array.isArray(result)) return result.map(i => i?.name || i?.id || '').join('、') || '没有匹配收藏';
  if (typeof value.name === 'string') return `已保存：${value.name}`;
  return '操作已完成';
}

/** 内容与任务轮询共用一条串行请求链。切换到服务页仍接收 MCP 确认，不重复执行动作。 */
export default function ContentWorkspace({ visible, suspendDialogs, agents, onOpenService, onSettings, onDirtyChange }: Props) {
  const [items, setItems] = useState<ContentItem[]>([]); const [statuses, setStatuses] = useState<Record<string, ContentStatus>>({});
  const [tasks, setTasks] = useState<ActionTask[]>([]); const [selected, setSelected] = useState('');
  const [kind, setKind] = useState<'all' | ContentItem['kind']>('all'); const [search, setSearch] = useState(''); const [tag, setTag] = useState(''); const [sort, setSort] = useState('pinned');
  const [editor, setEditor] = useState<{ item: ContentItem; taskId?: string; index?: number } | null>(null);
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false); const [loading, setLoading] = useState(true);
  const [input, setInput] = useState(''); const [agentId, setAgentId] = useState(''); const [caps, setCaps] = useState<AgentCapabilities[]>([]);
  const [voiceEnabled, setVoiceEnabled] = useState(false); const [modelReady, setModelReady] = useState(false);
  const [speechPhase, setSpeechPhase] = useState('idle'); const [seconds, setSeconds] = useState(0); const [showHistory, setShowHistory] = useState(false);
  const inflight = useRef(false); const mounted = useRef(true); const voiceToken = useRef(0); const recording = useRef(false); const starting = useRef(false); const released = useRef(false); const activeAgent = useRef(agentId); activeAgent.current = agentId;
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const refresh = useCallback(async () => {
    if (inflight.current) return; inflight.current = true;
    try { const [snapshot, nextTasks] = await Promise.all([visible ? contentApi.snapshot() : Promise.resolve(null), contentApi.tasks()]); if (!mounted.current) return; if (snapshot) { setItems(snapshot.items); setStatuses(Object.fromEntries(snapshot.statuses.map(s => [s.contentId, s]))); } setTasks(nextTasks); }
    catch (e) { if (mounted.current) setError(String(e)); } finally { inflight.current = false; if (mounted.current) setLoading(false); }
  }, [visible]);
  useEffect(() => { mounted.current = true; void refresh(); const timer = window.setInterval(() => void refresh(), 2000); return () => { mounted.current = false; clearInterval(timer); }; }, [refresh]);
  useEffect(() => {
    let active = true; if (!visible) return;
    const load = () => void Promise.all([contentApi.capabilities(), contentApi.preferences(), contentApi.speechStatus()]).then(([nextCaps, prefs, status]) => {
      if (!active) return; setCaps(nextCaps);
      setAgentId(previous => nextCaps.some(c => c.agentId === previous && c.centerAvailable) ? previous : nextCaps.some(c => c.agentId === prefs.agentId && c.centerAvailable) ? prefs.agentId : '');
      setVoiceEnabled(prefs.voiceEnabled); setModelReady(status.modelReady);
    }).catch(e => { if (active) setError(String(e)); });
    load(); window.addEventListener('content-preferences-changed', load);
    return () => { active = false; window.removeEventListener('content-preferences-changed', load); };
  }, [visible, agents]);
  useEffect(() => { if (tasks[0]?.state === 'failed') setShowHistory(true); }, [tasks[0]?.id, tasks[0]?.state]);

  const upsertTask = (task: ActionTask) => setTasks(previous => [task, ...previous.filter(t => t.id !== task.id)]);
  const work = async (callback: () => Promise<unknown>) => { setBusy(true); setError(''); try { await callback(); await refresh(); } catch (e) { setError(String(e)); } finally { setBusy(false); } };
  const submit = async (action: PlannedAction['action'], item: ContentItem, changes: Record<string, unknown> | null = null) => { await work(async () => { const task = await contentApi.submit({ summary: `${actionText[action]}「${item.name}」`, actions: [{ action, contentId: item.id, query: null, changes }] }); upsertTask(task); }); };
  const submitText = async (text: string) => {
    if (!text.trim()) return;
    const chosen = activeAgent.current; if (!chosen) { setError('请先选择支持中心指令的 Agent。'); return; }
    await work(async () => { const task = await contentApi.plan(text.trim(), chosen, crypto.randomUUID()); upsertTask(task); setInput(''); });
  };
  const cancelVoice = useCallback(() => { voiceToken.current++; recording.current = false; starting.current = false; released.current = false; setSpeechPhase('idle'); setSeconds(0); if (isDesktop) void contentApi.speechCancel().catch(() => {}); }, []);
  const finishVoice = async () => {
    if (starting.current) { released.current = true; return; } if (!recording.current) return;
    recording.current = false; const token = voiceToken.current; setSpeechPhase('transcribing');
    try { const text = await contentApi.speechFinish(); if (token !== voiceToken.current) return; setSpeechPhase('idle'); setInput(text); if (text.trim()) await submitText(text); }
    catch (e) { if (token === voiceToken.current) { setSpeechPhase('idle'); setError(String(e)); } }
  };
  const startVoice = async () => {
    if (starting.current || recording.current || speechPhase !== 'idle') return;
    setError(''); const token = ++voiceToken.current; starting.current = true; released.current = false; setSpeechPhase('starting');
    try { await contentApi.speechStart(); starting.current = false; if (token !== voiceToken.current) { await contentApi.speechCancel(); return; } recording.current = true; setSeconds(0); setSpeechPhase('recording'); if (released.current) await finishVoice(); }
    catch (e) { starting.current = false; if (token === voiceToken.current) { setSpeechPhase('idle'); setError(String(e)); } }
  };
  useEffect(() => { if (speechPhase !== 'recording') return; const timer = window.setInterval(() => setSeconds(value => value + 1), 1000); return () => clearInterval(timer); }, [speechPhase]);
  useEffect(() => { if (seconds >= 60 && recording.current) void finishVoice(); }, [seconds]);
  useEffect(() => { const cancel = () => { if (recording.current || starting.current || speechPhase === 'transcribing') cancelVoice(); }; const key = (e: KeyboardEvent) => { if (e.key === 'Escape' && speechPhase !== 'idle') { e.preventDefault(); cancelVoice(); } }; window.addEventListener('blur', cancel); window.addEventListener('keydown', key); return () => { window.removeEventListener('blur', cancel); window.removeEventListener('keydown', key); }; }, [cancelVoice, speechPhase]);
  useEffect(() => { if (!visible && (recording.current || starting.current)) cancelVoice(); }, [visible, cancelVoice]);
  useEffect(() => () => { if (recording.current || starting.current) cancelVoice(); }, [cancelVoice]);
  const current = items.find(item => item.id === selected) || null; const status = current ? statuses[current.id] : undefined;
  const currentTarget = status?.targetUri || (current ? contentTargetLabel(current) : '');
  const tags = [...new Set(items.flatMap(item => item.tags))].sort();
  const query = search.trim().toLocaleLowerCase();
  const filtered = items.filter(item => (kind === 'all' || item.kind === kind) && (!tag || item.tags.includes(tag)) && (!query || [item.name, ...item.aliases, item.group, ...item.tags, item.notes, contentTargetLabel(item)].some(s => s.toLocaleLowerCase().includes(query)))).sort((a, b) => (sort === 'pinned' ? Number(b.pinned) - Number(a.pinned) : 0) || (sort !== 'name' ? (b.lastUsedAt || '').localeCompare(a.lastUsedAt || '') : 0) || a.name.localeCompare(b.name, 'zh-CN'));
  useEffect(() => { setSelected(''); }, [kind, search, tag]);
  const pending = tasks.find(task => pendingStates.includes(task.state));
  const pendingIndex = pending?.steps.findIndex(step => pendingStates.includes(step.state)) ?? -1;
  const step = pending && pendingIndex >= 0 ? pending.steps[pendingIndex] : null;
  const latestTask = tasks[0];
  const actOnTask = (fn: () => Promise<ActionTask>) => void work(async () => upsertTask(await fn()));
  const openTaskForm = () => { if (!pending || !step) return; void work(async () => {
    // 外部 MCP 可在服务页请求编辑；此时内容库没有持续刷新，必须先取当前记录再生成表单。
    const snapshot = await contentApi.snapshot(); setItems(snapshot.items); setStatuses(Object.fromEntries(snapshot.statuses.map(s => [s.contentId, s])));
    const existing = snapshot.items.find(item => item.id === step.contentId);
    if (step.action.action === 'edit' && !existing) throw new Error('目标收藏已不存在，请取消此任务后重新操作。');
    const changes = step.action.changes || {}; const base = existing || newContent();
    setEditor({ item: { ...base, ...changes } as ContentItem, taskId: pending.id, index: pendingIndex });
  }); };
  const editorClose = () => setEditor(null);
  return <>
    <section className="content-workspace" hidden={!visible} aria-label="内容库">
      <header className="content-header"><div><span className="content-eyebrow">本机收藏 · 统一入口</span><h1><Library size={25} />内容库</h1><p>文件夹、应用与游戏，随时找到并打开。</p></div><div className="content-inline-actions"><button className="button secondary" disabled={busy} onClick={() => void refresh()} title="刷新内容状态"><RefreshCw size={16} /></button><button className="button primary" onClick={() => setEditor({ item: newContent() })}><Plus size={16} />添加内容</button></div></header>
      {!isDesktop && <div className="content-notice">浏览器界面预览：收藏记录仅保存在预览缓存中，不代表本机应用状态。</div>}
      <div className="content-toolbar"><div className="content-kind-picker">{(['all', 'folder', 'application', 'game'] as const).map(value => <button type="button" key={value} className={value === kind ? 'selected' : ''} onClick={() => setKind(value)}>{value === 'all' ? '全部' : contentKindText[value]}<small>{items.filter(i => value === 'all' || i.kind === value).length}</small></button>)}</div><label className="content-search"><Search size={16} /><input aria-label="搜索内容库" placeholder="名称、别名、标签或路径" value={search} onChange={e => setSearch(e.target.value)} /></label><Select aria-label="筛选内容标签" value={tag} onChange={setTag} options={[{ value: '', label: '全部标签' }, ...tags.map(t => ({ value: t, label: t }))]} /><Select aria-label="内容排序" value={sort} onChange={setSort} options={[{ value: 'pinned', label: '置顶优先' }, { value: 'recent', label: '最近打开' }, { value: 'name', label: '名称排序' }]} /></div>
      {error && <div className="content-error" role="alert"><span>{error}</span><button className="icon-button" aria-label="关闭内容错误" onClick={() => setError('')}><X size={15} /></button></div>}
      <div className={`content-body ${current ? 'has-detail' : ''}`}>
        <div className="content-items">{loading ? <div className="content-empty"><span className="spinner" />正在读取收藏…</div> : filtered.length ? <div className="content-grid">{filtered.map(item => { const Icon = kindIcons[item.kind]; const s = statuses[item.id]; return <button type="button" className={`content-card ${current?.id === item.id ? 'selected' : ''}`} key={item.id} onClick={() => setSelected(item.id)}><div className="content-card-top"><span className={`content-item-icon ${item.kind}`}><Icon size={21} /></span>{item.pinned && <Pin size={14} />}<span className={`content-state ${s?.running ? 'running' : ''}`}>{item.target.type === 'steam' ? 'Steam' : s ? stateText[s.state] : '待探测'}</span></div><strong>{item.name}</strong><span className="content-card-path" title={s?.targetUri || contentTargetLabel(item)}>{s?.targetUri || contentTargetLabel(item)}</span><div className="content-card-tags"><span>{item.group || contentKindText[item.kind]}</span>{item.tags.slice(0, 3).map(t => <span key={t}>{t}</span>)}</div>{item.lastUsedAt && <small className="content-last-used">最近打开 {new Date(item.lastUsedAt).toLocaleDateString()}</small>}</button>; })}</div> : <div className="content-empty"><Library size={38} /><h2>{items.length ? '没有匹配的内容' : '把常用内容放在一起'}</h2><p>{items.length ? '试试其他名称，或清除筛选。' : '添加一个文件夹、应用或游戏，然后用名称、别名或语音调用。'}</p><button className="button primary" onClick={() => items.length ? (setSearch(''), setKind('all'), setTag('')) : setEditor({ item: newContent() })}>{items.length ? '清除筛选' : '添加第一个收藏'}</button></div>}</div>
        {current && <aside className="content-detail"><div className="content-detail-heading"><span className="content-eyebrow">{contentKindText[current.kind]}</span><button className="icon-button small" aria-label="关闭内容详情" onClick={() => setSelected('')}><X size={16} /></button></div><h2>{current.name}</h2><p className="content-detail-path">{currentTarget}</p><div className="content-inline-actions"><button className="button primary" disabled={busy || !isDesktop || !status?.canOpen} onClick={() => void submit('open', current)}>{current.kind === 'folder' ? <FolderOpen size={16} /> : <Play size={16} />}{current.kind === 'folder' ? '打开文件夹' : '打开'}</button>{current.target.type !== 'steam' && current.target.type !== 'folder' && <button className="button secondary" disabled={busy || !status?.canFocus} onClick={() => void submit('focus', current)}><ArrowUpRight size={15} />显示</button>}<button className="icon-button" title="编辑收藏" aria-label="编辑收藏" onClick={() => setEditor({ item: current })}><Pencil size={16} /></button></div>
          <div className="content-detail-status"><strong>{status ? stateText[status.state] : '状态尚未获取'}</strong>{status?.pid && <span>PID {status.pid}</span>}{status?.message && <p>{status.message}</p>}{current.target.type === 'steam' && <p>只发送启动请求，不据此推断游戏运行状态。</p>}{current.target.type === 'folder' && <p>文件夹仅检查路径和权限，不显示进程状态。</p>}</div>
          {!!status?.candidates.length && (status.state === 'selectionRequired' || status.state === 'unknown') && <div className="content-instance-picker"><strong>选择运行实例</strong>{status.candidates.map(candidate => <button className="button secondary" key={`${candidate.identity.pid}:${candidate.identity.createdAt}`} disabled={busy} onClick={() => void work(() => contentApi.associate(current.id, candidate.identity))}>{candidate.name} · PID {candidate.identity.pid}</button>)}</div>}
          {current.target.type === 'serviceLink' && <button className="text-button" onClick={() => current.target.type === 'serviceLink' && onOpenService(current.target.serviceId)}>打开原应用配置与文档 <ArrowUpRight size={14} /></button>}
          <dl><dt>别名</dt><dd>{current.aliases.filter(Boolean).join('、') || '未设置'}</dd><dt>分组与标签</dt><dd>{[current.group, ...current.tags].filter(Boolean).join(' · ') || '未设置'}</dd><dt>备注</dt><dd className="content-notes">{current.notes || '暂无备注'}</dd></dl>
          <div className="content-recent-results"><dt>最近操作</dt>{tasks.flatMap(task => task.steps.filter(s => s.contentId === current.id).map(s => ({ step: s, id: task.id }))).slice(0, 3).map(({ step: recent, id }, index) => <p key={`${id}:${index}`}>{actionText[recent.action.action]} · {taskStateText[recent.state] || recent.state}{recent.error ? `：${recent.error}` : recent.result ? `：${resultText(recent.result)}` : ''}</p>)}</div><div className="content-detail-actions"><button className="button secondary" onClick={() => void work(() => navigator.clipboard.writeText(currentTarget))}><Copy size={14} />复制位置</button><button className="button secondary" disabled={busy || !isDesktop} onClick={() => void submit('edit', current, { pinned: !current.pinned })}><Pin size={14} />{current.pinned ? '取消置顶' : '置顶'}</button>{status?.canRequestQuit && <button className="button secondary" disabled={busy} onClick={() => void submit('requestQuit', current)}><Power size={14} />请求退出</button>}{status?.canForceQuit && <button className="text-button danger" disabled={busy} onClick={() => void submit('forceQuit', current)}>强制结束…</button>}<button className="text-button danger" disabled={busy || !isDesktop} onClick={() => void submit('remove', current)}><Trash2 size={14} />移除收藏…</button></div>
          <p className="field-hint">移除收藏或退出中心会保留实际文件及运行中的应用。</p>
        </aside>}
      </div>
      <div className="content-command-area">
        {latestTask && <div className="content-task-summary"><button type="button" onClick={() => setShowHistory(v => !v)}><span className={`status-dot ${latestTask.state === 'failed' ? 'failed' : activeStates.includes(latestTask.state) ? 'busy' : 'running'}`} />{taskStateText[latestTask.state] || latestTask.state}<span>{latestTask.summary || latestTask.input}</span></button>{activeStates.includes(latestTask.state) && <button className="text-button" onClick={() => actOnTask(() => contentApi.cancel(latestTask.id))}>取消任务</button>}</div>}
        {showHistory && <div className="content-task-history">{tasks.slice(0, 12).map(task => <article key={task.id}><strong>{task.summary || task.input}</strong><span>{taskStateText[task.state]}</span>{task.error && <p className="content-danger">{task.error}</p>}{task.steps.map((s, i) => <p key={i}><b>{actionText[s.action.action]} {s.targetName || s.action.query || ''}</b> · {taskStateText[s.state] || s.state}{s.error ? `：${s.error}` : s.result ? `：${resultText(s.result)}` : ''}</p>)}</article>)}</div>}
        <div className="content-command"><Bot size={20} /><textarea ref={inputRef} aria-label="内容指令" placeholder="试试：打开设计资料，或查找我的游戏…" value={input} rows={1} maxLength={4000} onChange={e => setInput(e.target.value)} onKeyDown={e => { if (e.key === 'Enter' && !e.shiftKey && !e.nativeEvent.isComposing) { e.preventDefault(); if (!busy && agentId) void submitText(input); } }} /><Select aria-label="指令 Agent" value={agentId} disabled={busy} onChange={next => { setAgentId(next); void contentApi.savePreferences({ agentId: next, voiceEnabled }).catch(e => setError(String(e))); }} options={[{ value: '', label: '选择 Agent' }, ...caps.map(c => ({ value: c.agentId, label: `${c.name}${c.centerAvailable ? '' : '（暂不可用）'}`, disabled: !c.centerAvailable }))]} /><button className={`icon-button content-mic ${speechPhase === 'recording' ? 'recording' : ''}`} aria-label="按住说话" title={!voiceEnabled ? '请先在设置中启用语音' : !modelReady ? '请先在设置中准备语音模型' : '按住说话，松开识别；Esc 取消'} disabled={!isDesktop || !voiceEnabled || !modelReady || !agentId || busy || speechPhase === 'transcribing'} onPointerDown={e => { if (e.button !== 0) return; e.preventDefault(); e.currentTarget.setPointerCapture(e.pointerId); void startVoice(); }} onPointerUp={() => void finishVoice()} onPointerCancel={cancelVoice} onKeyDown={e => { if ((e.key === ' ' || e.key === 'Enter') && !e.repeat) { e.preventDefault(); void startVoice(); } }} onKeyUp={e => { if (e.key === ' ' || e.key === 'Enter') { e.preventDefault(); void finishVoice(); } }}><Mic size={18} /></button><button className="button primary content-send" aria-label="提交内容指令" disabled={!isDesktop || !agentId || !input.trim() || busy || speechPhase !== 'idle'} onClick={() => void submitText(input)}><Send size={17} /></button></div>
        <div className="content-command-hint"><span>{speechPhase === 'recording' ? `正在录音 ${seconds} / 60 秒 · 松开后识别` : speechPhase === 'transcribing' ? '正在本地转写…' : speechPhase === 'starting' ? '正在准备麦克风…' : '指令仅操作内容库 · 打开直接执行，退出和移除需确认'}</span>{speechPhase !== 'idle' ? <button className="text-button" onClick={cancelVoice}><Square size={12} />取消</button> : <button className="text-button" onClick={onSettings}><Settings size={13} />Agent 与语音设置</button>}</div>
      </div>
    </section>
    {pending && step && !editor && !suspendDialogs && <Modal title={pending.state === 'awaitingSelection' ? '选择要操作的收藏' : pending.state === 'awaitingForm' ? '确认收藏信息' : `${actionText[step.action.action]}「${step.targetName || '所选内容'}」？`} tone={step.action.action === 'forceQuit' ? 'danger' : 'normal'} onCancel={() => actOnTask(() => contentApi.cancel(pending.id))} actions={[
      { label: '取消任务', disabled: busy, autoFocus: true, onClick: () => actOnTask(() => contentApi.cancel(pending.id)) },
      ...(pending.state === 'awaitingConfirmation' ? [{ label: step.action.action === 'forceQuit' ? '确认强制结束' : '确认操作', disabled: busy, variant: 'danger' as const, onClick: () => actOnTask(() => contentApi.confirm(pending.id, pendingIndex)) }] : []),
      ...(pending.state === 'awaitingForm' ? [{ label: '填写并确认', disabled: busy, variant: 'primary' as const, onClick: openTaskForm }] : []),
    ]}>
      <p>{pending.source === 'mcp' ? '外部 Agent 请求' : '内容操作'}：{pending.summary}</p>{step.error && <p className="content-danger">{step.error}</p>}
      {pending.state === 'awaitingSelection' && <div className="content-candidate-list">{step.candidates.map(item => <button key={item.id} className="button secondary" disabled={busy} onClick={() => actOnTask(() => contentApi.choose(pending.id, pendingIndex, item.id))}><span><strong>{item.name}</strong><small>{contentTargetLabel(item)}</small></span><Check size={15} /></button>)}</div>}
      {pending.state === 'awaitingConfirmation' && <><p>{step.action.action === 'remove' ? '只移除收藏记录。实际文件和运行中的应用会保留。' : step.action.action === 'forceQuit' ? '强制结束可能丢失该应用未保存的内容，只作用于当前关联主进程。' : step.action.action === 'requestQuit' ? '向当前关联实例请求正常退出。应用拒绝退出时仍保留跟踪，不自动强制结束。' : '请核对本次修改目标。'}</p><div className="content-confirm-target"><strong>{step.targetName}</strong><p>{step.targetLabel || '收藏目标'}</p>{!!step.result && typeof step.result === 'object' && <><p>{resultText(step.result)}</p>{(step.result as ContentStatus).pid && <p>关联进程 PID {(step.result as ContentStatus).pid}</p>}</>}</div></>}
      {pending.state === 'awaitingForm' && <p>请在表单中检查名称与启动目标。保存前不会使用 Agent 提出的新路径启动程序。</p>}
    </Modal>}
    {editor && <ContentEditor key={`${editor.item.id}:${editor.taskId || 'manual'}`} item={editor.item} status={statuses[editor.item.id]} onDirtyChange={onDirtyChange} onClose={editorClose} onSave={editor.taskId && editor.index !== undefined ? item => contentApi.saveForm(editor.taskId!, editor.index!, item) : undefined} onSaved={async item => { setSelected(item.id); await refresh(); }} />}
  </>;
}
