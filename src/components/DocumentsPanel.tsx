import { useEffect, useMemo, useState } from 'react';
import { Clipboard, Download, Eye, FilePlus2, FileText, Pencil, Save, Trash2 } from 'lucide-react';
import ReactMarkdown from 'react-markdown';
import remarkGfm from 'remark-gfm';
import { api } from '../api';
import type { ServiceDocument } from '../types';
import Modal from './Modal';

export interface DocumentDraft {
  /** 跨服务稳定的界面键；新文档获得后端 id 后仍保留此键，以保住焦点与草稿。 */
  key: string;
  document: ServiceDocument;
  isNew: boolean;
  baseline: { title: string; content: string } | null;
  saveError?: string;
}

export function isDocumentDirty(draft: DocumentDraft): boolean {
  return draft.isNew || !draft.baseline
    || draft.document.title !== draft.baseline.title
    || draft.document.content !== draft.baseline.content;
}

interface DocumentsPanelProps {
  serviceId: string;
  serviceName: string;
  drafts: Record<string, DocumentDraft>;
  setDrafts: React.Dispatch<React.SetStateAction<Record<string, DocumentDraft>>>;
  activeKey: string;
  setActiveKey: (key: string) => void;
  onSave: (draft: DocumentDraft) => Promise<ServiceDocument>;
  onDelete: (draft: DocumentDraft) => Promise<void>;
  chooseExport: (title: string) => Promise<string | null>;
  exportDocument: (id: string, path: string) => Promise<void>;
}

/** 文档编辑、预览与持久化集中在此处；草稿存放于父页面，所以切换服务或标签页不会丢失。 */
export default function DocumentsPanel({
  serviceId, serviceName, drafts, setDrafts, activeKey, setActiveKey, onSave, onDelete,
  chooseExport, exportDocument,
}: DocumentsPanelProps) {
  const [preview, setPreview] = useState(false);
  const [saving, setSaving] = useState(false);
  const [exporting, setExporting] = useState(false);
  const [error, setError] = useState('');
  const [deletePrompt, setDeletePrompt] = useState(false);
  const [discardPrompt, setDiscardPrompt] = useState(false);
  const serviceDrafts = useMemo(() => Object.values(drafts).filter((item) => item.document.serviceId === serviceId), [drafts, serviceId]);
  const activeDraft = serviceDrafts.find((item) => item.key === activeKey) ?? serviceDrafts[0];
  const activeDirty = activeDraft ? isDocumentDirty(activeDraft) : false;

  useEffect(() => {
    if (activeDraft && activeDraft.key !== activeKey) setActiveKey(activeDraft.key);
    // activeDraft 的回退行为故意只在服务或文档键变化后校正，不干扰正在编辑的内容。
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [serviceId, activeKey]);

  const createDocument = () => {
    const key = `local-${Date.now()}-${Math.random().toString(36).slice(2, 7)}`;
    const draft: DocumentDraft = {
      key,
      document: { id: '', serviceId, title: '未命名文档', content: '', updatedAt: '' },
      isNew: true,
      baseline: null,
    };
    setDrafts((previous) => ({ ...previous, [key]: draft }));
    setActiveKey(key);
    setPreview(false);
    setError('');
  };

  const updateDocument = (patch: Partial<ServiceDocument>) => {
    if (!activeDraft) return;
    setDrafts((previous) => ({
      ...previous,
      [activeDraft.key]: {
        ...activeDraft,
        document: { ...activeDraft.document, ...patch, serviceId },
        saveError: undefined,
      },
    }));
    setError('');
  };

  const saveCurrent = async (): Promise<ServiceDocument | null> => {
    if (!activeDraft) return null;
    const submitted = activeDraft;
    setSaving(true);
    setError('');
    try {
      const saved = await onSave(submitted);
      setDrafts((previous) => ({
        ...previous,
        [submitted.key]: (() => {
          const latest = previous[submitted.key] ?? submitted;
          const changedWhileSaving = latest.document.title !== submitted.document.title
            || latest.document.content !== submitted.document.content;
          return {
          ...latest,
          document: changedWhileSaving
            ? { ...latest.document, id: saved.id, serviceId: saved.serviceId, updatedAt: saved.updatedAt }
            : saved,
          isNew: false,
          baseline: { title: saved.title, content: saved.content },
          saveError: undefined,
          };
        })(),
      }));
      return saved;
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason);
      setDrafts((previous) => ({
        ...previous,
        [submitted.key]: { ...(previous[submitted.key] ?? submitted), saveError: message },
      }));
      setError(`文档保存失败，编辑内容已保留，请重试。${message ? ` ${message}` : ''}`);
      return null;
    } finally {
      setSaving(false);
    }
  };

  const doExport = async () => {
    if (!activeDraft) return;
    setExporting(true);
    setError('');
    try {
      // 导出内容以已保存文档为准；有修改时先持久化，避免导出的文件与应用内文档不同步。
      const saved = activeDirty ? await saveCurrent() : activeDraft.document;
      if (!saved?.id) return;
      const path = await chooseExport(saved.title);
      if (path) await exportDocument(saved.id, path);
    } catch (reason) {
      setError(`导出失败：${reason instanceof Error ? reason.message : String(reason)}`);
    } finally {
      setExporting(false);
    }
  };

  const removeCurrent = async () => {
    if (!activeDraft) return;
    try {
      await onDelete(activeDraft);
      setDrafts((previous) => {
        const next = { ...previous };
        delete next[activeDraft.key];
        return next;
      });
      const next = serviceDrafts.find((item) => item.key !== activeDraft.key);
      setActiveKey(next?.key ?? '');
      setDeletePrompt(false);
      setError('');
    } catch (reason) {
      setDeletePrompt(false);
      setError(`文档删除失败：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const discardCurrent = () => {
    if (!activeDraft) return;
    if (activeDraft.isNew) {
      setDrafts((previous) => {
        const next = { ...previous };
        delete next[activeDraft.key];
        return next;
      });
      const next = serviceDrafts.find((item) => item.key !== activeDraft.key);
      setActiveKey(next?.key ?? '');
    } else if (activeDraft.baseline) {
      setDrafts((previous) => ({
        ...previous,
        [activeDraft.key]: {
          ...activeDraft,
          document: { ...activeDraft.document, ...activeDraft.baseline },
          saveError: undefined,
        },
      }));
    }
    setDiscardPrompt(false);
    setError('');
  };

  const copyDocument = async () => {
    if (!activeDraft) return;
    try {
      await navigator.clipboard.writeText(activeDraft.document.content);
      setError('文档内容已复制。');
    } catch {
      setError('无法访问剪贴板，请手动选择并复制编辑区内容。');
    }
  };

  const saveOrCopyText = activeDraft?.saveError ? '保存失败' : saving ? '保存中…' : activeDirty ? '有未保存的修改' : '已保存';

  return (
    <div className="documents-workspace">
      <aside className="document-list-pane" aria-label="服务文档列表">
        <div className="document-list-heading"><div><h3>文档</h3><span>{serviceDrafts.length} 篇</span></div><button className="icon-button" type="button" aria-label="新建文档" title="新建文档" onClick={createDocument}><FilePlus2 size={17} /></button></div>
        {serviceDrafts.length === 0 ? (
          <div className="document-list-empty"><FileText size={20} /><strong>还没有文档</strong><span>在这里记录服务说明、排查步骤和本地备注。</span><button className="button secondary" type="button" onClick={createDocument}>新建文档</button></div>
        ) : (
          <div className="document-list-items">
            {serviceDrafts.map((item) => (
              <button key={item.key} type="button" className={`document-list-item ${activeDraft?.key === item.key ? 'selected' : ''}`} onClick={() => { setActiveKey(item.key); setError(''); }}>
                <FileText size={16} />
                <span className="document-list-item-copy"><strong>{item.document.title.trim() || '未命名文档'}</strong><small>{isDocumentDirty(item) ? '有未保存的修改' : item.document.updatedAt ? new Date(item.document.updatedAt).toLocaleDateString() : '新文档'}</small></span>
                {isDocumentDirty(item) && <span className="dirty-dot" aria-label="未保存" />}
              </button>
            ))}
          </div>
        )}
      </aside>

      <section className="document-editor-pane" aria-label={`${serviceName}文档编辑器`}>
        {!activeDraft ? (
          <div className="document-editor-empty"><FileText size={24} /><h3>选择或新建一篇文档</h3><p>文档保存在本机，可预览和导出为 Markdown。</p><button className="button primary" type="button" onClick={createDocument}><FilePlus2 size={16} /> 新建文档</button></div>
        ) : <>
          <div className="document-toolbar">
            <div className="document-title-wrap"><input aria-label="文档标题" value={activeDraft.document.title} onChange={(event) => updateDocument({ title: event.target.value })} placeholder="文档标题" /><span className={`save-state ${activeDraft.saveError ? 'failed' : activeDirty ? 'dirty' : ''}`}>{activeDraft.saveError && <span className="status-dot" />}{saveOrCopyText}</span></div>
            <div className="document-actions">
              <div className="segmented-control" role="group" aria-label="文档编辑模式">
                <button type="button" className={!preview ? 'active' : ''} onClick={() => setPreview(false)}><Pencil size={14} /> 编辑</button>
                <button type="button" className={preview ? 'active' : ''} onClick={() => setPreview(true)}><Eye size={14} /> 预览</button>
              </div>
              <button className="button secondary compact" type="button" onClick={() => void doExport()} disabled={exporting || saving}>{exporting ? <span className="spinner" /> : <Download size={15} />} 导出</button>
              <button className="icon-button danger-quiet" type="button" aria-label="删除文档" title="删除文档" disabled={saving || exporting} onClick={() => setDeletePrompt(true)}><Trash2 size={16} /></button>
              {activeDirty && <button className="button secondary compact" type="button" disabled={saving || exporting} onClick={() => setDiscardPrompt(true)}>放弃修改</button>}
              <button className="button primary compact" type="button" onClick={() => void saveCurrent()} disabled={!activeDirty || saving}><Save size={15} />{saving ? '保存中…' : '保存'}</button>
            </div>
          </div>
          {error && <div className={`document-message ${error.startsWith('文档内容已复制') ? 'success' : 'error'}`} role="status">{error}{activeDraft.saveError && <button className="text-button" type="button" onClick={() => void copyDocument()}><Clipboard size={14} /> 复制内容</button>}</div>}
          <div className={`document-content ${preview ? 'preview-mode' : ''}`}>
            {preview ? (
              <article className="markdown-preview" aria-label="Markdown 预览">
                {activeDraft.document.content.trim() ? <ReactMarkdown remarkPlugins={[remarkGfm]} skipHtml components={{
                  a: ({ href, title, children }) => <a href={href} title={title} onClick={(event) => {
                    // 阻止 WebView 自身导航；只有明确点击链接时才交给系统浏览器打开。
                    event.preventDefault();
                    if (!href) { setError('链接地址无效。'); return; }
                    void api.openExternalUrl(href).then(() => setError('')).catch((reason) => {
                      setError(`无法打开链接：${reason instanceof Error ? reason.message : String(reason)}`);
                    });
                  }}>{children}</a>,
                  // 图片只显示替代文字，避免预览 Markdown 时自动请求远程图片。
                  img: ({ alt }) => <span>{alt ? `[图片：${alt}]` : '[图片未加载]'}</span>,
                }}>{activeDraft.document.content}</ReactMarkdown> : <p className="preview-placeholder">暂无内容，切换到编辑模式开始编写。</p>}
              </article>
            ) : <textarea className="document-markdown-input" aria-label="Markdown 文档内容" value={activeDraft.document.content} onChange={(event) => updateDocument({ content: event.target.value })} placeholder={'使用 Markdown 编写文档…\n\n支持标题、列表、表格、链接和代码块。原始 HTML 不会执行。'} spellCheck={false} />}
          </div>
          <div className="document-footer"><span>Markdown</span><span>{activeDraft.document.content.length.toLocaleString()} 字符</span><span>保存至本机</span></div>
        </>}
      </section>

      {deletePrompt && activeDraft && <Modal title="删除这篇文档？" tone="danger" onCancel={() => !saving && !exporting && setDeletePrompt(false)} actions={[
        { label: '取消', onClick: () => setDeletePrompt(false), variant: 'secondary', autoFocus: true, disabled: saving || exporting },
        { label: '删除文档', onClick: () => void removeCurrent(), variant: 'danger', disabled: saving || exporting },
      ]}><p>“{activeDraft.document.title || '未命名文档'}”将从此服务的本地文档中删除。</p></Modal>}
      {discardPrompt && activeDraft && <Modal title="放弃这篇文档的修改？" onCancel={() => !saving && !exporting && setDiscardPrompt(false)} actions={[
        { label: '继续编辑', onClick: () => setDiscardPrompt(false), variant: 'secondary', autoFocus: true, disabled: saving || exporting },
        { label: '放弃修改', onClick: discardCurrent, variant: 'danger', disabled: saving || exporting },
      ]}><p>{activeDraft.isNew ? '这篇新文档尚未保存，放弃后会从列表移除。' : '文档将恢复到上次保存的内容。'}</p></Modal>}
    </div>
  );
}

export async function saveAllDocumentDrafts(
  drafts: Record<string, DocumentDraft>,
  onSave: (draft: DocumentDraft) => Promise<ServiceDocument>,
  updateDrafts: React.Dispatch<React.SetStateAction<Record<string, DocumentDraft>>>,
): Promise<void> {
  // 退出前按顺序保存所有服务的脏文档；遇到失败立即停止退出，并保留失败文档及其原始内容。
  for (const draft of Object.values(drafts)) {
    if (!isDocumentDirty(draft)) continue;
    try {
      const saved = await onSave(draft);
      updateDrafts((previous) => ({
        ...previous,
        [draft.key]: (() => {
          const latest = previous[draft.key] ?? draft;
          const changedWhileSaving = latest.document.title !== draft.document.title
            || latest.document.content !== draft.document.content;
          return {
            ...latest,
            document: changedWhileSaving
              ? { ...latest.document, id: saved.id, serviceId: saved.serviceId, updatedAt: saved.updatedAt }
              : saved,
            isNew: false,
            baseline: { title: saved.title, content: saved.content },
            saveError: undefined,
          };
        })(),
      }));
    } catch (reason) {
      const message = reason instanceof Error ? reason.message : String(reason);
      updateDrafts((previous) => ({
        ...previous,
        [draft.key]: { ...(previous[draft.key] ?? draft), saveError: message },
      }));
      throw new Error(`“${draft.document.title || '未命名文档'}”保存失败，退出已取消。${message ? ` ${message}` : ''}`);
    }
  }
}
