import { invoke } from '@tauri-apps/api/core';
import { isDesktop } from './api';
import type { ActionPlan, ActionTask, AgentCapabilities, ContentItem, ContentStatus, ControlPreferences, SpeechStatus } from './contentTypes';
import type { ProcessIdentity } from './types';
const previewKey = 'kacent-content-preview-v1';
function readPreview(): ContentItem[] { try { return JSON.parse(localStorage.getItem(previewKey) || '[]') as ContentItem[]; } catch { return []; } }
const desktopOnly = (): never => { throw new Error('此操作需要在桌面应用内使用；浏览器仅预览收藏界面。'); };
export const contentApi = {
  async snapshot(): Promise<{ items: ContentItem[]; statuses: ContentStatus[] }> { return isDesktop ? invoke('content_snapshot') : { items: readPreview(), statuses: [] }; },
  async save(item: ContentItem, stamp: string | null = null): Promise<ContentItem> {
    if (isDesktop) return invoke('content_save', { item, stamp });
    if (!item.name.trim()) throw new Error('请填写内容名称');
    const now = new Date().toISOString(); const saved = { ...item, id: item.id || crypto.randomUUID(), createdAt: item.createdAt || now, updatedAt: now, revision: item.revision + 1 };
    localStorage.setItem(previewKey, JSON.stringify([...readPreview().filter(i => i.id !== saved.id), saved])); return saved;
  },
  async submit(plan: ActionPlan): Promise<ActionTask> { return isDesktop ? invoke('content_submit', { plan, requestId: crypto.randomUUID() }) : desktopOnly(); },
  async plan(input: string, agentId: string, requestId: string): Promise<ActionTask> { return isDesktop ? invoke('content_plan', { input, agentId, requestId }) : desktopOnly(); },
  async tasks(): Promise<ActionTask[]> { return isDesktop ? invoke('content_tasks') : []; },
  async confirm(taskId: string, index: number): Promise<ActionTask> { return invoke('content_confirm', { taskId, index }); },
  async choose(taskId: string, index: number, contentId: string): Promise<ActionTask> { return invoke('content_choose', { taskId, index, contentId }); },
  async cancel(taskId: string): Promise<ActionTask> { return invoke('content_cancel', { taskId }); },
  async saveForm(taskId: string, index: number, item: ContentItem): Promise<ContentItem> { return invoke('content_save_form', { taskId, index, item }); },
  async associate(contentId: string, identity: ProcessIdentity): Promise<unknown> { return invoke('content_associate', { contentId, identity }); },
  async capabilities(): Promise<AgentCapabilities[]> { return isDesktop ? invoke('content_agent_capabilities') : []; },
  async preferences(): Promise<ControlPreferences> { return isDesktop ? invoke('content_preferences') : { agentId: '', voiceEnabled: false }; },
  async savePreferences(preferences: ControlPreferences): Promise<ControlPreferences> { return isDesktop ? invoke('save_content_preferences', { preferences }) : preferences; },
  async mcpConfig(agentId: string): Promise<unknown> { return isDesktop ? invoke('content_mcp_config', { agentId }) : desktopOnly(); },
  async checkMcp(): Promise<unknown> { return isDesktop ? invoke('content_mcp_check') : desktopOnly(); },
  async speechStatus(): Promise<SpeechStatus> { return isDesktop ? invoke('speech_status') : { state: 'idle', modelReady: false, progress: 0, error: null }; },
  async speechStart(): Promise<void> { return invoke('speech_start'); },
  async speechCancel(): Promise<void> { return invoke('speech_cancel'); },
  async speechFinish(): Promise<string> { return invoke('speech_finish'); },
  async speechDownload(): Promise<void> { return invoke('speech_download'); },
  async speechImport(path: string): Promise<void> { return invoke('speech_import', { path }); },
};
