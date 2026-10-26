import { useEffect, useRef, useState } from 'react';
import { Check, FolderOpen, X } from 'lucide-react';
import { api, isDesktop } from '../api';
import type { ContentItem, ContentStatus } from '../contentTypes';
import { contentKindText } from '../contentTypes';
import { contentApi } from '../contentApi';
import { trapDialogFocus } from './focusTrap';
import Modal from './Modal';
import Select from './Select';

interface Props { item: ContentItem; status?: ContentStatus; onSave?: (item: ContentItem) => Promise<ContentItem>; onSaved: (item: ContentItem) => Promise<void>; onClose: () => void; onDirtyChange?: (dirty: boolean) => void }
/** 保存表单只接受用户看到的结构化目标；Agent 草稿进入此表单后仍需用户明确保存。 */
export default function ContentEditor({ item, status, onSave, onSaved, onClose, onDirtyChange }: Props) {
  const [draft, setDraft] = useState(() => structuredClone(item));
  const [error, setError] = useState(''); const [busy, setBusy] = useState(false); const [discard, setDiscard] = useState(false);
  const initial = useRef(JSON.stringify(item)); const initialStamp = useRef(status?.confirmationStamp || null); const ref = useRef<HTMLElement>(null);
  const dirty = JSON.stringify(draft) !== initial.current;
  const targetLocked = item.target.type === 'serviceLink' || (item.target.type === 'localApp' && (status?.running || status?.state === 'unknown'));
  useEffect(() => trapDialogFocus(ref), []);
  useEffect(() => { onDirtyChange?.(dirty || busy); return () => onDirtyChange?.(false); }, [dirty, busy, onDirtyChange]);
  const close = () => { if (!busy) { if (dirty) setDiscard(true); else onClose(); } };
  const pick = async () => { try { const value = draft.target.type === 'folder' ? await api.chooseDirectory() : await api.chooseApplicationPath(/win/i.test(navigator.platform) ? 'windows' : 'macos'); if (value && (draft.target.type === 'folder' || draft.target.type === 'localApp')) { const target = { ...draft.target, path: value }; setDraft({ ...draft, target, name: draft.name || value.split(/[\\/]/).filter(Boolean).pop()?.replace(/\.(app|exe)$/i, '') || '' }); } } catch (e) { setError(String(e)); } };
  const save = async () => { setError(''); setBusy(true); try { const cleaned = { ...draft, tags: draft.tags.filter(Boolean), aliases: draft.aliases.filter(Boolean) }; const saved = onSave ? await onSave(cleaned) : await contentApi.save(cleaned, initialStamp.current); await onSaved(saved); onClose(); } catch (e) { setError(String(e)); } finally { setBusy(false); } };
  return <>
    <div className="drawer-scrim" onMouseDown={e => e.target === e.currentTarget && close()}>
      <aside className="settings-drawer content-editor" ref={ref} role="dialog" aria-modal="true" aria-labelledby="content-editor-title" onKeyDown={e => { if (e.key === 'Escape' && !discard) { e.stopPropagation(); close(); } }}>
        <header className="drawer-header"><div><h2 id="content-editor-title">{item.id ? '编辑收藏' : '添加到内容库'}</h2><p>保存启动入口、标签和备注</p></div><button className="icon-button" aria-label="关闭收藏编辑" disabled={busy} onClick={close}><X size={18} /></button></header>
        <div className="settings-body"><fieldset className="settings-fields" disabled={busy}>
          <div className="content-kind-picker">{(['folder', 'application', 'game'] as const).map(kind => <button type="button" key={kind} disabled={targetLocked} className={draft.kind === kind ? 'selected' : ''} onClick={() => setDraft({ ...draft, kind, target: kind === 'folder' ? { type: 'folder', path: '' } : { type: 'localApp', path: '', args: [], startDir: '' } })}>{contentKindText[kind]}</button>)}</div>
          <label className="field"><span>名称 <b>*</b></span><input autoComplete="off" maxLength={160} value={draft.name} onChange={e => setDraft({ ...draft, name: e.target.value })} placeholder="例如：设计资料、浏览器、我的游戏" /></label>
          {draft.kind === 'game' && <label className="field"><span>启动入口</span><Select value={draft.target.type} disabled={targetLocked} onChange={next => setDraft({ ...draft, target: next === 'steam' ? { type: 'steam', appId: '' } : { type: 'localApp', path: '', args: [], startDir: '' } })} options={[{ value: 'localApp', label: '本地应用（.exe / .app）' }, { value: 'steam', label: 'Steam 游戏' }, ...(draft.target.type === 'serviceLink' ? [{ value: 'serviceLink', label: '已有桌面应用' }] : [])]} /></label>}
          {(draft.target.type === 'folder' || draft.target.type === 'localApp') && <label className="field"><span>{draft.target.type === 'folder' ? '文件夹' : '应用路径'} <b>*</b></span><div className="content-path-input"><input disabled={targetLocked} value={draft.target.path} onChange={e => { const path = e.target.value; setDraft(d => d.target.type === 'folder' || d.target.type === 'localApp' ? { ...d, target: { ...d.target, path } } : d); }} /><button type="button" className="button secondary compact" disabled={targetLocked || !isDesktop} onClick={() => void pick()}><FolderOpen size={15} />选择</button></div></label>}
          {draft.target.type === 'steam' && <label className="field"><span>Steam 游戏 ID 或启动链接 <b>*</b></span><input value={draft.target.appId} onChange={e => setDraft({ ...draft, target: { type: 'steam', appId: e.target.value } })} placeholder="570 或 steam://rungameid/570" /><small>只使用固定 Steam 启动入口；发送请求不代表游戏已经运行。</small></label>}
          {draft.target.type === 'localApp' && <><label className="field"><span>启动参数（可选，每行一个）</span><textarea disabled={targetLocked} value={draft.target.args.join('\n')} onChange={e => { const args = e.target.value.split('\n'); setDraft(d => d.target.type === 'localApp' ? { ...d, target: { ...d.target, args: args.length === 1 && !args[0] ? [] : args } } : d); }} placeholder="参数按行传递，路径中的空格无需引号" /></label><label className="field"><span>启动目录（Windows，可选）</span><input disabled={targetLocked} value={draft.target.startDir} onChange={e => { const startDir = e.target.value; setDraft(d => d.target.type === 'localApp' ? { ...d, target: { ...d.target, startDir } } : d); }} /><small>macOS 应用由系统启动，不承诺改变应用的工作目录。</small></label></>}
          {targetLocked && <p className="field-hint">{item.target.type === 'serviceLink' ? '此收藏关联原桌面应用。启动配置和文档继续在原应用详情中维护。' : '当前实例活动或身份待确认，暂时不能更改启动目标。'}</p>}
          <label className="field"><span>别名（用逗号分隔）</span><input value={draft.aliases.join('，')} onChange={e => setDraft({ ...draft, aliases: e.target.value.split(/[,，]/).map(s => s.trim()) })} placeholder="用于文字或语音查找" /></label>
          <div className="content-editor-pair"><label className="field"><span>分组</span><input value={draft.group} onChange={e => setDraft({ ...draft, group: e.target.value })} /></label><label className="field"><span>标签（用逗号分隔）</span><input value={draft.tags.join('，')} onChange={e => setDraft({ ...draft, tags: e.target.value.split(/[,，]/).map(s => s.trim()) })} /></label></div>
          <label className="field"><span>备注</span><textarea rows={5} value={draft.notes} onChange={e => setDraft({ ...draft, notes: e.target.value })} /></label>
          <label className="content-check"><input type="checkbox" checked={draft.pinned} onChange={e => setDraft({ ...draft, pinned: e.target.checked })} />置顶收藏</label>
        </fieldset>{error && <p className="form-error" role="alert">{error}</p>}</div>
        <footer className="drawer-footer"><span>只保存收藏，不改动实际文件</span><div><button className="button secondary" disabled={busy} onClick={close}>取消</button><button className="button primary" disabled={busy || !draft.name.trim()} onClick={() => void save()}><Check size={16} />{busy ? '正在保存…' : '保存收藏'}</button></div></footer>
      </aside>
    </div>
    {discard && <Modal title="放弃收藏修改？" onCancel={() => setDiscard(false)} actions={[{ label: '继续编辑', onClick: () => setDiscard(false), autoFocus: true }, { label: '放弃修改', onClick: onClose, variant: 'danger' }]}><p>当前尚未保存的修改会丢失。</p></Modal>}
  </>;
}
