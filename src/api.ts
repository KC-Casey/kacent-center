import { agentPresets, expandedPresetIds, matchesAgentPreset, presetProfile } from './agentPresets';
import { defaultTerminalAppearance, normalizeTerminalAppearance, validateTerminalAppearance } from './terminalAppearance';
import { invoke, isTauri } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { open, save } from '@tauri-apps/plugin-dialog';
import type { AgentAvailability, AgentProfile, ApplicationInspection, ApplicationSnapshot, AttachApplicationRequest, AttachExternalRequest, Bootstrap, DiscoverySnapshot, FrontendProject, Preferences, ServiceConfig, ServiceDocument, ServiceLaunchResult, ServiceRuntime, TerminalSession } from './types';

export const isDesktop = isTauri();
export const isPreview = !isDesktop;

// 浏览器仅用于界面验收：独立命名空间、明确预览标识，不伪造进程执行结果。
// 实际桌面始终经过 Rust/SQLite；演示数据只在显式 ?demo=1 时出现。
const previewKey = 'kacent-interface-preview-v1';
const previewAgents = agentPresets.map(presetProfile);
type PreviewData = { services: ServiceConfig[]; agents: AgentProfile[]; preferences: Preferences; documents: ServiceDocument[]; presetRevision: number };
function previewData(): PreviewData {
  const initial: PreviewData = { services: [], agents: previewAgents, preferences: { theme: 'system', terminal: defaultTerminalAppearance() }, documents: [], presetRevision: 1 };
  try {
    const saved = localStorage.getItem(previewKey);
    if (saved) {
      const parsed = JSON.parse(saved) as Partial<PreviewData>;
      const services = Array.isArray(parsed.services) ? parsed.services.map(normalizeService) : initial.services;
      const upgradedAgents = Array.isArray(parsed.agents) ? [...parsed.agents] : [...initial.agents];
      // 只升级一次：尊重用户改过的路径，也不在重开后复活已删除的预设。
      if (!parsed.presetRevision) {
        for (const preset of agentPresets.filter(item => expandedPresetIds.has(item.id))) {
          if (!upgradedAgents.some(agent => matchesAgentPreset(agent, preset))) upgradedAgents.push(presetProfile(preset));
        }
      }
      const data: PreviewData = {
        ...initial,
        ...parsed,
        services,
        agents: upgradedAgents,
        presetRevision: 1,
        documents: Array.isArray(parsed.documents) ? parsed.documents : initial.documents,
        preferences: normalizePreferences(parsed.preferences),
      };
      if (!parsed.presetRevision) savePreview(data);
      return data;
    }
  } catch { /* 预览存储损坏时返回空工作区，不影响桌面 SQLite 数据。 */ }
  if (new URLSearchParams(location.search).get('demo') === '1') {
    const now = new Date().toISOString();
    initial.services = ['知识库 API', '管理前端', '任务队列', '向量检索', '图像服务'].map((name, index) => ({
      id: `preview-${index}`, kind: index === 1 ? 'frontend' : 'command', launch: index === 1
        ? { type: 'frontend', packageManager: 'pnpm', script: 'dev', args: [], url: 'http://localhost:5173' }
        : { type: 'command' }, source: 'managed', external: null, name, description: index === 0 ? '本地知识库与检索接口' : '本地开发服务',
      workingDir: '/Users/kc/Projects/knowledge-api', environmentType: 'venv', environmentDir: '/Users/kc/Envs/knowledge-api',
      executableDirs: [], env: { APP_ENV: 'development' }, command: 'python -m uvicorn app.main:app --port 8000',
      shell: '', expectedPorts: index === 0 ? [8000] : index === 1 ? [5173] : [],
      notes: '本地开发环境，启动前请确认模型服务可用。', createdAt: now, updatedAt: now,
    }));
  }
  return initial;
}

function normalizePreferences(value: Partial<Preferences> | undefined): Preferences {
  const theme = value?.theme;
  return { theme: theme === 'dark' || theme === 'light' ? theme : 'system', terminal: normalizeTerminalAppearance(value?.terminal) };
}

/**
 * 本地预览缓存和升级前的 SQLite 都可能没有 kind/launch。前端在展示边界补齐旧结构，
 * 将历史服务明确按命令服务读取，避免旧数据在界面升级后丢失配置或产生 undefined。
 */
function normalizeService(input: ServiceConfig): ServiceConfig {
  const value = input as ServiceConfig & { kind?: ServiceConfig['kind']; launch?: ServiceConfig['launch'] };
  const kind = value.kind ?? 'command';
  const launch = value.launch ?? (kind === 'frontend'
    ? { type: 'frontend' as const, packageManager: '', script: '', args: [], url: '' }
    : kind === 'staticSite'
      ? { type: 'staticSite' as const, root: value.workingDir ?? '', port: 0, spaFallback: true }
      : kind === 'desktopApp'
        ? { type: 'desktopApp' as const, path: value.command ?? '', args: [], startDir: '' }
        : { type: 'command' as const });
  return {
    ...value,
    kind,
    launch,
    source: value.source ?? 'managed',
    external: value.external ?? null,
    description: value.description ?? '',
    workingDir: value.workingDir ?? '',
    environmentType: value.environmentType ?? 'system',
    environmentDir: value.environmentDir ?? '',
    executableDirs: value.executableDirs ?? [],
    env: value.env ?? {},
    command: value.command ?? '',
    shell: value.shell ?? '',
    expectedPorts: value.expectedPorts ?? [],
    notes: value.notes ?? '',
    createdAt: value.createdAt ?? '',
    updatedAt: value.updatedAt ?? '',
  };
}
function savePreview(data: PreviewData) { localStorage.setItem(previewKey, JSON.stringify(data)); }
function previewOnly(): never { throw new Error('当前是界面预览，请打开桌面应用后操作服务和终端。'); }
function stopped(serviceId: string): ServiceRuntime {
  return {
    serviceId, runId: null, status: 'stopped', pid: null, ports: [], conflicts: [], portError: null,
    exitCode: null, error: null, startedAt: null, updatedAt: null, stale: false, canStop: false,
    canUnlock: false, canConvert: false, protectionUnlocked: false, controlReason: null,
    canStart: false, canRestart: false, canFocus: false, canRequestQuit: false, canForceQuit: false,
    actionReasons: {}, webUrls: [], messages: [],
  };
}
function savePreviewItem<T extends { id: string }>(list: T[], item: T): T[] { return [...list.filter(value => value.id !== item.id), item]; }

export const api = {
  async bootstrap(): Promise<Bootstrap> {
    if (isDesktop) return invoke('bootstrap');
    const data = previewData();
    return { ...data, services: data.services.map(normalizeService), runtimes: data.services.map(service => stopped(service.id)), sessions: [] };
  },
  /**
   * 读取本机发现快照。浏览器预览只返回明确的桌面限制说明，避免把示例数据
   * 伪装成真实机器上的进程扫描结果。
   */
  async discoverySnapshot(force: boolean): Promise<DiscoverySnapshot> {
    if (isDesktop) return invoke('discovery_snapshot', { force });
    if (new URLSearchParams(location.search).get('demo') === '1') {
      // 只有用户显式启用 ?demo=1 时才展示合成记录，而且页面会持续标明它们不是本机扫描结果。
      return {
        revision: 1,
        updatedAt: null,
        stale: false,
        error: '静态演示数据：这些进程与端口是合成示例，不代表本机扫描结果；接入和控制在浏览器预览中不可用。',
        processes: [
          {
            instance: { identity: { pid: 32145, createdAt: 'demo-instance-32145' }, name: 'postgres', executable: '/usr/local/bin/postgres', workingDir: '/Users/demo/data/postgres', category: 'nonSystem', reasons: ['静态示例：数据库主进程。'], sharedHost: false, controllable: true, controlReason: null },
            ports: [
              { address: '127.0.0.1', port: 5432, protocol: 'TCP', pid: 32145 },
              { address: '::1', port: 5432, protocol: 'TCP', pid: 32145 },
              { address: '::1', port: 62369, protocol: 'UDP', pid: 32145 },
            ],
            managedServiceId: null,
            familyIdentity: { pid: 32145, createdAt: 'demo-instance-32145' },
          },
          ...[32146, 32147].map((pid): DiscoverySnapshot['processes'][number] => ({
            instance: { identity: { pid, createdAt: `demo-instance-${pid}` }, name: 'postgres', executable: '/usr/local/bin/postgres', workingDir: '/Users/demo/data/postgres', category: 'nonSystem', reasons: ['静态示例：继承主进程端口的数据库工作进程。'], sharedHost: false, controllable: true, controlReason: null },
            ports: [{ address: '::1', port: 62369, protocol: 'UDP', pid }],
            managedServiceId: null,
            familyIdentity: { pid: 32145, createdAt: 'demo-instance-32145' },
          })),
          {
            instance: { identity: { pid: 32150, createdAt: 'demo-instance-32150' }, name: 'postgres', executable: '/usr/local/bin/postgres', workingDir: '/Users/demo/data/another-database', category: 'nonSystem', reasons: ['静态示例：另一个独立数据库实例，不应仅因同名而合并。'], sharedHost: false, controllable: true, controlReason: null },
            ports: [{ address: '127.0.0.1', port: 5433, protocol: 'TCP', pid: 32150 }],
            managedServiceId: null,
            familyIdentity: null,
          },
          {
            instance: { identity: { pid: 412, createdAt: 'demo-instance-412' }, name: '系统解析服务', executable: '/usr/libexec/mDNSResponder', workingDir: null, category: 'system', reasons: ['静态示例：系统进程。'], sharedHost: true, controllable: false, controlReason: '共享宿主始终只读。' },
            ports: [{ address: '0.0.0.0', port: 5353, protocol: 'UDP', pid: 412 }],
            managedServiceId: null,
          },
          {
            instance: { identity: { pid: 29018, createdAt: 'demo-instance-29018' }, name: '未知进程', executable: null, workingDir: null, category: 'unknown', reasons: ['静态示例：扫描信息不足，无法分类。'], sharedHost: false, controllable: false, controlReason: '当前进程身份无法安全确认。' },
            ports: [{ address: '::1', port: 9000, protocol: 'TCP', pid: 29018 }],
            managedServiceId: null,
          },
        ],
        unattributedPorts: [{ address: '0.0.0.0', port: 9999, protocol: 'TCP', pid: null }],
      };
    }
    return {
      revision: 0,
      updatedAt: null,
      stale: false,
      error: '进程发现仅在桌面应用中可用；浏览器预览不会扫描或模拟本机进程。',
      processes: [],
      unattributedPorts: [],
    };
  },
  /** 接入时只传进程身份与用户填写的元数据，不允许前端单靠 PID 猜测当前实例。 */
  async attachExternal(request: AttachExternalRequest): Promise<ServiceConfig> {
    if (isDesktop) return invoke<ServiceConfig>('attach_external', { request });
    return previewOnly();
  },
  /** 读取 package.json 中的名称、脚本和包管理器识别结果；只读，不安装依赖。 */
  async inspectFrontendProject(path: string): Promise<FrontendProject> {
    if (isDesktop) return invoke<FrontendProject>('inspect_frontend_project', { path });
    return previewOnly();
  },
  /** 检查当前平台可启动的桌面应用包，并读取显示名称和可用平台信息。 */
  async inspectApplication(path: string): Promise<ApplicationInspection> {
    if (isDesktop) return invoke<ApplicationInspection>('inspect_application', { path });
    return previewOnly();
  },
  /** 定时复用后端应用实例快照，运行中应用不要求必须监听端口。 */
  async applicationSnapshot(force: boolean): Promise<ApplicationSnapshot> {
    if (isDesktop) return invoke<ApplicationSnapshot>('application_snapshot', { force });
    return { updatedAt: null, stale: false, error: '运行中应用发现只在桌面应用中可用；浏览器预览不会扫描或模拟本机应用。', applications: [] };
  },
  /** 将已核验的应用实例关联到已有服务，或按实例信息创建新的桌面应用记录。 */
  async attachApplication(request: AttachApplicationRequest): Promise<ServiceConfig> {
    if (isDesktop) return invoke<ServiceConfig>('attach_application', { request });
    return previewOnly();
  },
  async focusApplication(serviceId: string): Promise<void> {
    if (isDesktop) return invoke('focus_application', { serviceId });
    return previewOnly();
  },
  async requestQuitApplication(serviceId: string): Promise<ServiceRuntime> {
    if (isDesktop) return invoke<ServiceRuntime>('request_quit_application', { serviceId });
    return previewOnly();
  },
  async forceQuitApplication(serviceId: string): Promise<ServiceRuntime> {
    if (isDesktop) return invoke<ServiceRuntime>('force_quit_application', { serviceId });
    return previewOnly();
  },
  async unlockApplication(serviceId: string): Promise<ServiceRuntime> {
    if (isDesktop) return invoke<ServiceRuntime>('unlock_application', { serviceId });
    return previewOnly();
  },
  /** 只临时解除当前进程实例的保护；是否允许由后端基于 identity 再次校验。 */
  async unlockExternal(serviceId: string): Promise<ServiceRuntime> {
    if (isDesktop) return invoke('unlock_external', { serviceId });
    return previewOnly();
  },
  /** 转换完成后后端会创建托管终端会话，前端用返回值接入既有会话列表。 */
  async convertExternal(serviceId: string): Promise<TerminalSession> {
    if (isDesktop) return invoke('convert_external', { serviceId });
    return previewOnly();
  },
  async saveService(service: ServiceConfig): Promise<ServiceConfig> {
    if (isDesktop) return invoke<ServiceConfig>('save_service', { service });
    if (!service.name.trim()) throw new Error('请填写服务名称。');
    if (service.kind === 'command' && (!service.workingDir.trim() || !service.command.trim())) throw new Error('请填写工作目录和启动命令。');
    if (service.kind === 'frontend' && (service.launch.type !== 'frontend' || !service.workingDir.trim() || !service.launch.packageManager || !service.launch.script)) throw new Error('前端项目需要工作目录、包管理器和启动脚本。');
    if (service.kind === 'staticSite' && (service.launch.type !== 'staticSite' || !service.launch.root.trim())) throw new Error('请选择静态站点目录。');
    if (service.kind === 'desktopApp' && (service.launch.type !== 'desktopApp' || !service.launch.path.trim())) throw new Error('请选择桌面应用文件。');
    const data = previewData();
    const now = new Date().toISOString();
    const value = normalizeService({ ...service, id: service.id || crypto.randomUUID(), createdAt: service.createdAt || now, updatedAt: now });
    data.services = savePreviewItem(data.services, value); savePreview(data); return value;
  },
  async deleteService(serviceId: string): Promise<void> {
    if (isDesktop) return invoke('delete_service', { serviceId });
    const data = previewData(); data.services = data.services.filter(value => value.id !== serviceId);
    data.documents = data.documents.filter(value => value.serviceId !== serviceId); savePreview(data);
  },
  async startService(serviceId: string): Promise<ServiceLaunchResult> { return isDesktop ? invoke<ServiceLaunchResult>('start_service', { serviceId }) : previewOnly(); },
  async stopService(serviceId: string): Promise<void> { return isDesktop ? invoke('stop_service', { serviceId }) : previewOnly(); },
  async restartService(serviceId: string): Promise<ServiceLaunchResult> { return isDesktop ? invoke<ServiceLaunchResult>('restart_service', { serviceId }) : previewOnly(); },
  async createTerminal(serviceId: string, agentId?: string): Promise<TerminalSession> { return isDesktop ? invoke('create_terminal', { serviceId, agentId: agentId ?? null }) : previewOnly(); },
  async closeTerminal(sessionId: string): Promise<void> { return isDesktop ? invoke('close_terminal', { sessionId }) : previewOnly(); },
  async saveAgent(agent: AgentProfile): Promise<AgentProfile> {
    if (isDesktop) return invoke('save_agent', { agent });
    if (!agent.name.trim() || !agent.executable.trim()) throw new Error('请填写工具名称和可执行程序。');
    const data = previewData(); const value = { ...agent, id: agent.id || crypto.randomUUID() };
    data.agents = savePreviewItem(data.agents, value); savePreview(data); return value;
  },
  async deleteAgent(agentId: string): Promise<void> {
    if (isDesktop) return invoke('delete_agent', { agentId });
    const data = previewData(); data.agents = data.agents.filter(value => value.id !== agentId); savePreview(data);
  },
  async checkAgent(agentId: string, serviceId?: string): Promise<AgentAvailability> {
    return isDesktop ? invoke('check_agent', { agentId, serviceId: serviceId ?? null }) : { available: false, resolvedPath: null, error: '请在桌面应用中检测本地 CLI。' };
  },
  async savePreferences(preferences: Preferences): Promise<Preferences> {
    const error = validateTerminalAppearance(preferences.terminal);
    if (error) throw new Error(error);
    if (isDesktop) return invoke('save_preferences', { preferences });
    const data = previewData(); data.preferences = preferences; savePreview(data); return preferences;
  },
  async listDocuments(serviceId: string): Promise<ServiceDocument[]> {
    return isDesktop ? invoke('list_documents', { serviceId }) : previewData().documents.filter(value => value.serviceId === serviceId);
  },
  async saveDocument(document: ServiceDocument): Promise<ServiceDocument> {
    if (isDesktop) return invoke('save_document', { document });
    if (!document.title.trim()) throw new Error('请填写文档标题。');
    const data = previewData(); const value = { ...document, id: document.id || crypto.randomUUID(), updatedAt: new Date().toISOString() };
    data.documents = savePreviewItem(data.documents, value); savePreview(data); return value;
  },
  async deleteDocument(documentId: string): Promise<void> {
    if (isDesktop) return invoke('delete_document', { documentId });
    const data = previewData(); data.documents = data.documents.filter(value => value.id !== documentId); savePreview(data);
  },
  async exportDocument(documentId: string, path: string): Promise<void> {
    if (isDesktop) return invoke('export_document', { documentId, path });
    const document = previewData().documents.find(value => value.id === documentId);
    if (!document) throw new Error('请先保存文档。');
    const url = URL.createObjectURL(new Blob([document.content], { type: 'text/markdown;charset=utf-8' }));
    const link = window.document.createElement('a'); link.href = url; link.download = path; link.click();
    setTimeout(() => URL.revokeObjectURL(url), 1000);
  },
  async openDirectory(serviceId: string, kind: 'work' | 'environment'): Promise<void> { return isDesktop ? invoke('open_service_directory', { serviceId, kind }) : previewOnly(); },
  async openExternalUrl(value: string): Promise<void> {
    let url: URL;
    try { url = new URL(value); }
    catch { throw new Error('链接格式无效。'); }
    if (!['http:', 'https:'].includes(url.protocol) || !url.hostname) {
      throw new Error('只支持有效的 HTTP 或 HTTPS 链接。');
    }
    if (isDesktop) return invoke('open_external_url', { url: url.href });
    // 预览模式也只在点击回调中调用此函数，并通过 noopener/noreferrer 隔离新标签页。
    window.open(url.href, '_blank', 'noopener,noreferrer');
  },
  async chooseDirectory(): Promise<string | null> {
    if (!isDesktop) throw new Error('浏览器预览无法选择系统目录，请手动填写示例路径。');
    const result = await open({ directory: true, multiple: false, title: '选择服务目录' }); return typeof result === 'string' ? result : null;
  },
  /** NSOpenPanel 将 .app 当作可选择的文件包；纯目录模式反而会禁止选择应用包。 */
  async chooseApplicationPath(platform: 'macos' | 'windows'): Promise<string | null> {
    if (!isDesktop) throw new Error('浏览器预览无法选择系统应用，请在桌面应用中操作。');
    const result = platform === 'macos'
      ? await open({ multiple: false, title: '选择 macOS 应用（.app）', filters: [{ name: 'macOS 应用', extensions: ['app'] }] })
      : await open({ multiple: false, title: '选择 Windows 应用（.exe）', filters: [{ name: 'Windows 应用', extensions: ['exe'] }] });
    return typeof result === 'string' ? result : null;
  },
  async platformInfo(): Promise<'macos' | 'windows' | 'unknown'> {
    if (!isDesktop) return 'unknown';
    const value = await invoke<string>('platform_info');
    return value === 'macos' || value === 'windows' ? value : 'unknown';
  },
  async chooseExport(title: string): Promise<string | null> {
    const name = title.replace(/[<>:"/\\|?*\u0000-\u001f]/g, '_') || '服务文档';
    return isDesktop ? save({ title: '导出 Markdown 文档', defaultPath: `${name}.md`, filters: [{ name: 'Markdown 文档', extensions: ['md'] }] }) : `${name}.md`;
  },
  async quit(): Promise<void> { return isDesktop ? invoke('quit_app') : previewOnly(); },
};

export async function onRuntime(callback: (runtime: ServiceRuntime) => void): Promise<() => void> {
  return isDesktop ? listen<ServiceRuntime>('service-runtime', event => callback(event.payload)) : () => {};
}
export async function onSessions(callback: (sessions: TerminalSession[]) => void): Promise<() => void> {
  return isDesktop ? listen<TerminalSession[]>('terminal-sessions', event => callback(event.payload)) : () => {};
}
let quitHandler: (() => void) | undefined;
let pendingQuit = false;
let quitSubscription: Promise<() => void> | undefined;
let quitDrain: Promise<void> | undefined;
function consumeQuitRequest(): Promise<void> {
  if (quitDrain) return quitDrain;
  quitDrain = (async () => {
    pendingQuit ||= await invoke<boolean>('take_quit_request');
    if (pendingQuit && quitHandler) { pendingQuit = false; quitHandler(); }
  })().finally(() => { quitDrain = undefined; });
  return quitDrain;
}
export async function onQuitRequested(callback: () => void): Promise<() => void> {
  if (!isDesktop) return () => {};
  quitHandler = callback;
  // 订阅由模块持有，React 严格模式重挂载只换回调，不会在消费后丢掉退出请求。
  // 先监听再补查，覆盖页面就绪前及注册期间产生的退出请求。
  quitSubscription ??= listen('quit-requested', () => { void consumeQuitRequest().catch(console.error); });
  await quitSubscription;
  void consumeQuitRequest().catch(console.error);
  return () => { if (quitHandler === callback) quitHandler = undefined; };
}
