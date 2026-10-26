import { useEffect, useLayoutEffect, useRef, useState } from 'react';
import { Activity, Check, Monitor, X } from 'lucide-react';
import type { Preferences } from '../types';
import { validateTerminalAppearance } from '../terminalAppearance';
import TerminalAppearanceEditor from './TerminalAppearanceEditor';
import Modal from './Modal';
import ContentControlSettings from './ContentControlSettings';
import { trapDialogFocus } from './focusTrap';

interface Props {
  initialTab?: 'appearance' | 'content';
  preferences: Preferences;
  systemTheme: 'dark' | 'light';
  onSave: (preferences: Preferences) => Promise<Preferences>;
  onDirtyChange: (dirty: boolean) => void;
  onClose: () => void;
}

/** 外观草稿只影响预览，保存成功后才更新运行中的终端；失败与取消都不覆盖原偏好。 */
export default function SettingsDrawer({ initialTab = 'appearance', preferences, systemTheme, onSave, onDirtyChange, onClose }: Props) {
  const [tab, setTab] = useState(initialTab);
  const [draft, setDraft] = useState<Preferences>(() => structuredClone(preferences));
  const [saved, setSaved] = useState(() => JSON.stringify(preferences));
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [discard, setDiscard] = useState(false);
  const ref = useRef<HTMLElement>(null);
  const dirty = JSON.stringify(draft) !== saved;
  useEffect(() => trapDialogFocus(ref), []);
  useLayoutEffect(() => {
    onDirtyChange(dirty || saving);
    return () => onDirtyChange(false);
  }, [dirty, saving, onDirtyChange]);
  const close = () => {
    if (saving) return;
    if (dirty) setDiscard(true);
    else onClose();
  };
  const save = async () => {
    const validation = validateTerminalAppearance(draft.terminal);
    if (validation) { setError(validation); return; }
    setSaving(true);
    setError('');
    try {
      const result = await onSave(draft);
      setDraft(structuredClone(result));
      setSaved(JSON.stringify(result));
    } catch (reason) {
      setError(`外观设置保存失败：${reason instanceof Error ? reason.message : String(reason)}`);
    } finally { setSaving(false); }
  };
  const choices: Array<{ value: Preferences['theme']; title: string; detail: string; icon: typeof Monitor }> = [
    { value: 'system', title: '跟随系统', detail: `系统当前为${systemTheme === 'dark' ? '深色' : '浅色'}`, icon: Monitor },
    { value: 'dark', title: '深色主题', detail: '适合低亮度环境', icon: Activity },
    { value: 'light', title: '浅色主题', detail: '明亮清晰的工作台', icon: Activity },
  ];
  return <>
    <div className="drawer-scrim" onMouseDown={event => event.target === event.currentTarget && close()}>
      <aside ref={ref} className="settings-drawer" role="dialog" aria-modal="true" aria-labelledby="settings-title" onKeyDown={event => {
        if (event.key === 'Escape' && !discard) { event.stopPropagation(); close(); }
      }}>
        <header className="drawer-header"><div><h2 id="settings-title">设置</h2><p>外观、内容控制与本地语音</p></div><button className="icon-button" type="button" aria-label="关闭设置" onClick={close} disabled={saving}><X size={18} /></button></header>
        <div className="settings-tab-row" role="tablist" aria-label="设置类别"><button type="button" role="tab" aria-selected={tab === 'appearance'} onClick={() => setTab('appearance')}>外观与终端</button><button type="button" role="tab" aria-selected={tab === 'content'} onClick={() => setTab('content')}>内容控制与语音</button></div>
        <div className="settings-body">
          {tab === 'content' && <ContentControlSettings />}
          <fieldset className="settings-fields" disabled={saving} hidden={tab !== 'appearance'}>
            <section><div className="section-heading"><h3>工作台主题</h3><p>用于侧栏、配置和文档；终端外观可以独立设置。</p></div>
              <div className="theme-choice-list">{choices.map(({ value, title, detail, icon: Icon }) => <button type="button" className={`theme-choice ${draft.theme === value ? 'selected' : ''}`} key={value} aria-pressed={draft.theme === value} onClick={() => setDraft(previous => ({ ...previous, theme: value }))}><span className="theme-choice-icon"><Icon size={17} /></span><span className="theme-choice-copy"><strong>{title}</strong><small>{detail}</small></span>{draft.theme === value && <Check size={16} className="theme-choice-check" />}</button>)}</div>
            </section>
            <TerminalAppearanceEditor value={draft.terminal} onChange={terminal => setDraft(previous => ({ ...previous, terminal }))} systemTheme={systemTheme} />
          </fieldset>
          {error && <div className="form-error" role="alert">{error}</div>}
          <div className="settings-note" hidden={tab !== 'appearance'}><Monitor size={15} /><span>保存在本机，保存后应用到已有终端，会话与输出记录保留。</span></div>
        </div>
        <footer className="drawer-footer"><span role="status">{saving ? '正在保存…' : dirty ? '有未保存的外观修改' : tab === 'content' ? '内容控制偏好即时保存' : '外观设置已保存'}</span><div><button className="button secondary" type="button" onClick={close} disabled={saving}>关闭</button><button className="button primary" type="button" onClick={() => void save()} disabled={saving || !dirty}><Check size={16} />{saving ? '保存中…' : '保存外观'}</button></div></footer>
      </aside>
    </div>
    {discard && <Modal title="放弃外观设置修改？" onCancel={() => setDiscard(false)} actions={[
      { label: '继续编辑', onClick: () => setDiscard(false), variant: 'secondary', autoFocus: true },
      { label: '放弃修改', onClick: onClose, variant: 'danger' },
    ]}><p>尚未保存的外观修改会丢失，原有终端样式保持不变。</p></Modal>}
  </>;
}
