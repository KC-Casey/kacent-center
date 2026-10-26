import { useCallback, useEffect, useMemo, useRef, useState } from 'react';
import {
  Activity, AppWindow, Bot, Check, ChevronDown, CircleAlert, Copy, Cpu, Filter,
  ExternalLink, Folder, FolderOpen, Library, LayoutGrid, Monitor, MoreHorizontal, Pencil, Plus, Power, RefreshCw,
  Search, Settings, Shield, Square, TerminalSquare, Trash2, Wrench, X,
} from 'lucide-react';
import { api, isDesktop, isPreview, onQuitRequested, onRuntime, onSessions } from './api';
import AgentDrawer from './components/AgentDrawer';
import ApplicationDiscoveryView from './components/ApplicationDiscoveryView';
import AttachApplicationDrawer from './components/AttachApplicationDrawer';
import AttachExternalDrawer from './components/AttachExternalDrawer';
import DocumentsPanel, { isDocumentDirty, saveAllDocumentDrafts, type DocumentDraft } from './components/DocumentsPanel';
import DiscoveryView, { type DiscoveryCategory } from './components/DiscoveryView';
import ExternalServiceDrawer from './components/ExternalServiceDrawer';
import ExternalServiceView from './components/ExternalServiceView';
import Modal from './components/Modal';
import ServiceDrawer from './components/ServiceDrawer';
import TerminalPanel from './components/TerminalPanel';
import SettingsDrawer from './components/SettingsDrawer';
import ContentWorkspace from './components/ContentWorkspace';
import { defaultTerminalAppearance } from './terminalAppearance';
import type { AgentAvailability, AgentProfile, ApplicationSnapshot, AttachApplicationRequest, AttachExternalRequest, Bootstrap, DiscoveredApplication, DiscoveredProcess, DiscoverySnapshot, Preferences, ServiceConfig, ServiceDocument, ServiceRuntime, TerminalSession } from './types';
import './styles.css';

type MainTab = 'overview' | 'environment' | 'documents';
type ServiceOperation = 'start' | 'stop' | 'restart';
type Drawer = { kind: 'service'; mode: 'create' | 'edit' } | { kind: 'external-edit' } | { kind: 'attach-external'; process: DiscoveredProcess } | { kind: 'attach-application'; application: DiscoveredApplication } | { kind: 'agents' } | { kind: 'settings'; tab?: 'appearance' | 'content' } | null;
type ServiceKindFilter = 'all' | ServiceConfig['kind'];

const statusText: Record<ServiceRuntime['status'], string> = {
  stopped: '已停止', starting: '启动中', running: '运行中', stopping: '停止中', failed: '异常退出', unknown: '待确认',
};

const serviceKindText: Record<ServiceConfig['kind'], string> = {
  command: '命令', frontend: '前端', staticSite: '静态站点', desktopApp: '桌面应用',
};

function stoppedRuntime(serviceId: string): ServiceRuntime {
  return {
    serviceId, runId: null, status: 'stopped', pid: null, ports: [], conflicts: [], portError: null,
    exitCode: null, error: null, startedAt: null, updatedAt: null, stale: false, canStop: false,
    canUnlock: false, canConvert: false, protectionUnlocked: false, controlReason: null,
    canStart: false, canRestart: false, canFocus: false, canRequestQuit: false, canForceQuit: false,
    actionReasons: {}, webUrls: [], messages: [],
  };
}

function runtimeForService(service: ServiceConfig): ServiceRuntime {
  // 桌面应用即使来自外部关联，路径仍是明确配置的；unknown 快照下允许后端复核并给出唯一/多实例选择。
  if (service.kind === 'desktopApp' && service.source === 'external') return {
    ...stoppedRuntime(service.id),
    status: 'unknown',
    pid: service.external?.identity.pid ?? null,
    stale: true,
    controlReason: '等待读取外部应用实例状态。',
    canStart: serviceConfigurationReady(service),
  };
  if (service.source === 'managed') return { ...stoppedRuntime(service.id), canStart: serviceConfigurationReady(service) };
  return {
    ...stoppedRuntime(service.id),
    status: 'unknown',
    pid: service.external?.identity.pid ?? null,
    stale: true,
    controlReason: '等待读取外部进程状态。',
  };
}

function runtimeClass(status: ServiceRuntime['status']): string {
  return status === 'running' ? 'running' : status === 'failed' ? 'failed' : status === 'starting' || status === 'stopping' ? 'busy' : status === 'unknown' ? 'unknown' : 'stopped';
}

function isActiveRuntime(status: ServiceRuntime['status']): boolean {
  return status === 'running' || status === 'starting' || status === 'stopping';
}

function serviceConfigurationReady(service: ServiceConfig | null | undefined): boolean {
  if (!service) return false;
  if (service.kind === 'command') return Boolean(service.workingDir.trim() && service.command.trim());
  if (service.kind === 'frontend') return service.launch.type === 'frontend'
    && Boolean(service.workingDir.trim() && service.launch.packageManager && service.launch.script.trim());
  if (service.kind === 'staticSite') return service.launch.type === 'staticSite' && Boolean(service.launch.root.trim());
  return service.launch.type === 'desktopApp' && Boolean(service.launch.path.trim());
}

function errorText(reason: unknown): string {
  return reason instanceof Error ? reason.message : String(reason);
}

function envEntries(env: Record<string, string>): Array<[string, string]> {
  return Object.entries(env).sort(([a], [b]) => a.localeCompare(b));
}

function prettyDate(value: string | null): string {
  if (!value) return '';
  const date = new Date(value);
  return Number.isNaN(date.valueOf()) ? '' : date.toLocaleString();
}

function formatAddressPort(address: string, port: number): string {
  return address.includes(':') && !address.startsWith('[') ? `[${address}]:${port}` : `${address}:${port}`;
}

function sameApplicationPath(a: string, b: string): boolean {
  const normalize = (value: string) => value.replace(/[\\/]+$/, '').replaceAll('\\', '/');
  const left = normalize(a);
  const right = normalize(b);
  return /win/i.test(navigator.platform) ? left.toLocaleLowerCase() === right.toLocaleLowerCase() : left === right;
}

/**
 * 服务状态中心：配置保存在服务列表中，运行态只由后端事件推进。启动、重启和删除
 * 都在一个边界明确的异步操作里处理，避免按钮状态、PID 与运行实例相互覆盖。
 */
export default function App() {
  const [services, setServicesState] = useState<ServiceConfig[]>([]);
  const servicesRef = useRef<ServiceConfig[]>([]);
  const [agents, setAgents] = useState<AgentProfile[]>([]);
  const [preferences, setPreferences] = useState<Preferences>(() => ({ theme: 'system', terminal: defaultTerminalAppearance() }));
  const [runtimes, setRuntimesState] = useState<Record<string, ServiceRuntime>>({});
  const runtimeRef = useRef<Record<string, ServiceRuntime>>({});
  const runIdGuard = useRef<Record<string, string>>({});
  const [sessions, setSessionsState] = useState<TerminalSession[]>([]);
  const sessionsRef = useRef<TerminalSession[]>([]);
  const [documentDrafts, setDocumentDraftsState] = useState<Record<string, DocumentDraft>>({});
  const documentDraftsRef = useRef<Record<string, DocumentDraft>>({});
  const documentSavePromises = useRef(new Map<string, Promise<ServiceDocument>>());
  const [activeDocumentByService, setActiveDocumentByService] = useState<Record<string, string>>({});
  const [documentsLoaded, setDocumentsLoaded] = useState<Record<string, boolean>>({});
  const [documentLoadErrors, setDocumentLoadErrors] = useState<Record<string, string>>({});
  const [documentsReloadToken, setDocumentsReloadToken] = useState(0);

  const [activeServiceId, setActiveServiceId] = useState('');
  const [activePage, setActivePage] = useState<'services' | 'discovery' | 'content'>('services');
  const [activeTab, setActiveTab] = useState<MainTab>('overview');
  const [activeDiscoveryMode, setActiveDiscoveryMode] = useState<'ports' | 'applications'>('ports');
  const [discoverySnapshot, setDiscoverySnapshot] = useState<DiscoverySnapshot | null>(null);
  const [applicationSnapshot, setApplicationSnapshot] = useState<ApplicationSnapshot | null>(null);
  const [discoveryRefreshing, setDiscoveryRefreshing] = useState(false);
  const discoveryRequestInFlight = useRef(false);
  const applicationRequestInFlight = useRef(false);
  const [discoverySearch, setDiscoverySearch] = useState('');
  const [discoveryCategories, setDiscoveryCategories] = useState<DiscoveryCategory[]>(['nonSystem', 'unknown']);
  const [selectedDiscoveryProcess, setSelectedDiscoveryProcess] = useState<DiscoveredProcess | null>(null);
  const [drawer, setDrawer] = useState<Drawer>(null);
  const [bootstrapLoading, setBootstrapLoading] = useState(true);
  const [bootstrapError, setBootstrapError] = useState('');
  const [globalError, setGlobalError] = useState('');
  const [toast, setToast] = useState('');
  const [operationByService, setOperationByService] = useState<Record<string, ServiceOperation | undefined>>({});
  const [desktopBusyByService, setDesktopBusyByService] = useState<Record<string, string | undefined>>({});
  const [desktopPrompt, setDesktopPrompt] = useState<{ kind: 'force' | 'restart'; serviceId: string; afterRequest?: boolean } | null>(null);
  const [launchCandidates, setLaunchCandidates] = useState<{ serviceId: string; candidates: DiscoveredApplication[] } | null>(null);
  const [selectedLaunchCandidate, setSelectedLaunchCandidate] = useState('');
  const [candidateBusy, setCandidateBusy] = useState(false);
  const [serviceErrors, setServiceErrors] = useState<Record<string, string>>({});
  const [invalidPathByService, setInvalidPathByService] = useState<Record<string, boolean>>({});
  const [filterStatus, setFilterStatus] = useState<'all' | 'running' | 'stopped' | 'failed'>('all');
  const [filterServiceKind, setFilterServiceKind] = useState<ServiceKindFilter>('all');
  const [search, setSearch] = useState('');
  const [filterOpen, setFilterOpen] = useState(false);
  const [agentMenuOpen, setAgentMenuOpen] = useState(false);
  const [moreMenuOpen, setMoreMenuOpen] = useState(false);
  const [selectedAgentByService, setSelectedAgentByService] = useState<Record<string, string>>({});
  const [agentFailure, setAgentFailure] = useState<{ serviceId: string; agentId: string; message: string } | null>(null);
  const [checkingAgentId, setCheckingAgentId] = useState('');
  const [deleteTargetId, setDeleteTargetId] = useState('');
  const [deleteBusy, setDeleteBusy] = useState(false);
  const [deleteError, setDeleteError] = useState('');
  const [externalAction, setExternalAction] = useState<{ kind: 'stop' | 'unlock' | 'convert'; serviceId: string } | null>(null);
  const [externalActionBusy, setExternalActionBusy] = useState(false);
  const [quitPrompt, setQuitPrompt] = useState(false);
  const [quitBusy, setQuitBusy] = useState(false);
  const [serviceDrawerDirty, setServiceDrawerDirty] = useState(false);
  const [agentDrawerDirty, setAgentDrawerDirty] = useState(false);
  const [settingsDrawerDirty, setSettingsDrawerDirty] = useState(false);
  const configDraftsRef = useRef({ service: false, agent: false, settings: false, content: false });
  const reportContentDirty = useCallback((dirty: boolean) => { configDraftsRef.current.content = dirty; }, []);
  const [systemTheme, setSystemTheme] = useState<'dark' | 'light'>(() => window.matchMedia('(prefers-color-scheme: dark)').matches ? 'dark' : 'light');

  const activeService = services.find((service) => service.id === activeServiceId) ?? null;
  const activeRuntime = activeService ? runtimes[activeService.id] ?? runtimeForService(activeService) : null;
  const activeSessions = activeService ? sessions.filter((session) => session.serviceId === activeService.id) : [];
  const resolvedTheme = preferences.theme === 'system' ? systemTheme : preferences.theme;
  const dirtyDocuments = useMemo(() => Object.values(documentDrafts).filter(isDocumentDirty), [documentDrafts]);

  const updateServices = useCallback((action: React.SetStateAction<ServiceConfig[]>) => {
    if (typeof action !== 'function') {
      servicesRef.current = action;
      setServicesState(action);
      return;
    }
    setServicesState((previous) => {
      const next = action(previous);
      servicesRef.current = next;
      return next;
    });
  }, []);

  const updateSessions = useCallback((action: React.SetStateAction<TerminalSession[]>) => {
    setSessionsState((previous) => {
      const next = typeof action === 'function' ? action(previous) : action;
      sessionsRef.current = next;
      return next;
    });
  }, []);

  const updateDocumentDrafts = useCallback((action: React.SetStateAction<Record<string, DocumentDraft>>) => {
    setDocumentDraftsState((previous) => {
      const next = typeof action === 'function' ? action(previous) : action;
      documentDraftsRef.current = next;
      return next;
    });
  }, []);

  const reportServiceDrawerDirty = useCallback((dirty: boolean) => {
    configDraftsRef.current = { ...configDraftsRef.current, service: dirty };
    setServiceDrawerDirty(dirty);
  }, []);

  const reportSettingsDrawerDirty = useCallback((dirty: boolean) => {
    configDraftsRef.current = { ...configDraftsRef.current, settings: dirty };
    setSettingsDrawerDirty(dirty);
  }, []);

  const reportAgentDrawerDirty = useCallback((dirty: boolean) => {
    configDraftsRef.current = { ...configDraftsRef.current, agent: dirty };
    setAgentDrawerDirty(dirty);
  }, []);

  const setRuntimeMap = useCallback((next: Record<string, ServiceRuntime>) => {
    runtimeRef.current = next;
    setRuntimesState(next);
  }, []);

  /**
   * 发现扫描只在发现页激活期间发起。失败响应会标记快照过期并保留上次结果，
   * 避免瞬时扫描错误把用户正在查看的进程与端口清空。
   */
  const refreshDiscovery = useCallback(async (force: boolean) => {
    if (discoveryRequestInFlight.current) return;
    discoveryRequestInFlight.current = true;
    setDiscoveryRefreshing(true);
    try {
      const next = await api.discoverySnapshot(force);
      setDiscoverySnapshot((previous) => {
        if (next.error && next.processes.length === 0 && previous) {
          return { ...next, stale: true, processes: previous.processes, unattributedPorts: previous.unattributedPorts };
        }
        return next;
      });
      setSelectedDiscoveryProcess((selected) => {
        if (!selected) return selected;
        return next.processes.find((process) => process.instance.identity.pid === selected.instance.identity.pid
          && process.instance.identity.createdAt === selected.instance.identity.createdAt) ?? selected;
      });
    } catch (reason) {
      const message = errorText(reason);
      setDiscoverySnapshot((previous) => previous
        ? { ...previous, stale: true, error: message }
        : { revision: 0, updatedAt: null, stale: true, error: message, processes: [], unattributedPorts: [] });
    } finally {
      discoveryRequestInFlight.current = false;
      setDiscoveryRefreshing(false);
    }
  }, []);

  const refreshApplications = useCallback(async (force: boolean) => {
    if (applicationRequestInFlight.current) return;
    applicationRequestInFlight.current = true;
    setDiscoveryRefreshing(true);
    try {
      const next = await api.applicationSnapshot(force);
      setApplicationSnapshot((previous) => next.error && next.applications.length === 0 && previous
        ? { ...next, stale: true, applications: previous.applications }
        : next);
    } catch (reason) {
      const message = errorText(reason);
      setApplicationSnapshot((previous) => previous
        ? { ...previous, stale: true, error: message }
        : { updatedAt: null, stale: true, error: message, applications: [] });
    } finally {
      applicationRequestInFlight.current = false;
      setDiscoveryRefreshing(false);
    }
  }, []);

  useEffect(() => {
    if (!isDesktop || activePage !== 'services' || activeService?.kind !== 'desktopApp') return;
    // 桌面应用的运行态不携带原生创建时间；详情页复用两秒缓存快照，补齐真实实例身份、
    // 窗口数量与监听端口。离开详情后停止轮询，发现页仍使用自己的同一快照轮询入口。
    const refresh = () => void refreshApplications(false);
    refresh();
    const timer = window.setInterval(refresh, 2000);
    return () => window.clearInterval(timer);
  }, [activePage, activeService?.id, activeService?.kind, refreshApplications]);

  const acceptRuntime = useCallback((incoming: ServiceRuntime) => {
    // 删除后的进程退出事件可能迟到；服务不再存在时忽略，防止幽灵状态重新进入工作区。
    if (!servicesRef.current.some((service) => service.id === incoming.serviceId)) return;
    const previous = runtimeRef.current[incoming.serviceId];
    const guardedRunId = runIdGuard.current[incoming.serviceId] ?? previous?.runId ?? null;
    const terminalStatus = (status?: ServiceRuntime['status']) => status === 'stopped' || status === 'failed';
    const activeStatus = (status: ServiceRuntime['status']) => status === 'starting' || status === 'running' || status === 'stopping';

    // 运行事件绑定到 runId：旧实例迟到的“运行中/失败”事件不能覆盖新实例，
    // 同一 runId 已结束后到达的旧活动事件也不能把服务状态回滚。
    if (previous && guardedRunId && incoming.runId && incoming.runId !== guardedRunId && incoming.status !== 'starting') return;
    if (previous && !incoming.runId && previous.runId && terminalStatus(incoming.status) && previous.status !== 'stopping') return;
    if (previous && terminalStatus(previous.status) && incoming.runId === guardedRunId && activeStatus(incoming.status)) return;
    if (incoming.status === 'starting' && incoming.runId) runIdGuard.current[incoming.serviceId] = incoming.runId;
    else if (!guardedRunId && incoming.runId) runIdGuard.current[incoming.serviceId] = incoming.runId;

    setRuntimeMap({ ...runtimeRef.current, [incoming.serviceId]: incoming });
  }, [setRuntimeMap]);

  useEffect(() => {
    document.documentElement.dataset.theme = resolvedTheme;
  }, [resolvedTheme]);

  useEffect(() => {
    const media = window.matchMedia('(prefers-color-scheme: dark)');
    const update = () => setSystemTheme(media.matches ? 'dark' : 'light');
    update();
    media.addEventListener('change', update);
    return () => media.removeEventListener('change', update);
  }, []);

  useEffect(() => {
    if (!toast) return;
    const timer = window.setTimeout(() => setToast(''), 3600);
    return () => window.clearTimeout(timer);
  }, [toast]);

  useEffect(() => {
    if (activePage !== 'discovery') return;
    const refresh = activeDiscoveryMode === 'applications' ? refreshApplications : refreshDiscovery;
    void refresh(true);
    if (!isDesktop) return;
    const timer = window.setInterval(() => void refresh(false), 2000);
    return () => window.clearInterval(timer);
  }, [activePage, activeDiscoveryMode, refreshApplications, refreshDiscovery]);

  useEffect(() => {
    let mounted = true;
    let ready = false;
    let runtimeUnsubscribe: (() => void) | null = null;
    let sessionsUnsubscribe: (() => void) | null = null;
    const queuedRuntimes: ServiceRuntime[] = [];
    let queuedSessions: TerminalSession[][] = [];

    // 先等两个事件监听都注册完成，再读取 bootstrap；初始化期间抵达的事件先排队，
    // 这样较新的事件会在快照之后应用，不会被初始化快照覆盖。
    const initialize = async () => {
      const subscriptions = await Promise.allSettled([
        onRuntime((runtime) => ready ? acceptRuntime(runtime) : queuedRuntimes.push(runtime)),
        onSessions((nextSessions) => {
          if (ready) updateSessions(nextSessions);
          else queuedSessions.push(nextSessions);
        }),
      ]);
      if (!mounted) {
        for (const result of subscriptions) if (result.status === 'fulfilled') result.value();
        return;
      }
      if (subscriptions[0].status === 'fulfilled') runtimeUnsubscribe = subscriptions[0].value;
      if (subscriptions[1].status === 'fulfilled') sessionsUnsubscribe = subscriptions[1].value;

      try {
        const data: Bootstrap = await api.bootstrap();
        if (!mounted) return;
        updateServices(data.services);
        setAgents(data.agents);
        setPreferences(data.preferences);
        const nextRuntimeMap = Object.fromEntries(data.runtimes.map((runtime) => [runtime.serviceId, runtime]));
        for (const runtime of data.runtimes) if (runtime.runId) runIdGuard.current[runtime.serviceId] = runtime.runId;
        setInvalidPathByService(Object.fromEntries(data.runtimes.filter((runtime) => /目录|路径|pyvenv/i.test(runtime.error ?? '')).map((runtime) => [runtime.serviceId, true])));
        setRuntimeMap(nextRuntimeMap);
        updateSessions(data.sessions);
        setActiveServiceId((current) => data.services.some((service) => service.id === current) ? current : data.services[0]?.id ?? '');
        ready = true;
        for (const runtime of queuedRuntimes) acceptRuntime(runtime);
        for (const nextSessions of queuedSessions) updateSessions(nextSessions);
        queuedSessions = [];
        const subscriptionFailure = subscriptions.find((result) => result.status === 'rejected');
        setBootstrapError(subscriptionFailure?.status === 'rejected' ? `部分运行事件监听不可用：${errorText(subscriptionFailure.reason)}` : '');
      } catch (reason) {
        if (!mounted) return;
        ready = true;
        for (const runtime of queuedRuntimes) acceptRuntime(runtime);
        for (const nextSessions of queuedSessions) updateSessions(nextSessions);
        queuedSessions = [];
        setBootstrapError(errorText(reason));
      } finally {
        if (mounted) setBootstrapLoading(false);
      }
    };
    void initialize();

    return () => {
      mounted = false;
      runtimeUnsubscribe?.();
      sessionsUnsubscribe?.();
    };
  }, [acceptRuntime, setRuntimeMap, updateServices, updateSessions]);

  useEffect(() => {
    if (!activeServiceId) return;
    let mounted = true;
    setDocumentsLoaded((previous) => ({ ...previous, [activeServiceId]: false }));
    setDocumentLoadErrors((previous) => ({ ...previous, [activeServiceId]: '' }));
    void api.listDocuments(activeServiceId).then((documents) => {
      if (!mounted) return;
      updateDocumentDrafts((previous) => {
        const next = { ...previous };
        for (const document of documents) {
          const alreadyLoaded = Object.values(next).some((draft) => draft.document.id === document.id);
          if (!alreadyLoaded) next[document.id] = {
            key: document.id,
            document,
            isNew: false,
            baseline: { title: document.title, content: document.content },
          };
        }
        return next;
      });
      setDocumentsLoaded((previous) => ({ ...previous, [activeServiceId]: true }));
    }).catch((reason: unknown) => {
      if (!mounted) return;
      setDocumentLoadErrors((previous) => ({ ...previous, [activeServiceId]: errorText(reason) }));
      setDocumentsLoaded((previous) => ({ ...previous, [activeServiceId]: true }));
    });
    return () => { mounted = false; };
  }, [activeServiceId, documentsReloadToken, updateDocumentDrafts]);

  const saveDocument = useCallback((draft: DocumentDraft) => {
    const existing = documentSavePromises.current.get(draft.key);
    if (existing) return existing;
    const request = api.saveDocument({
      ...draft.document,
      id: draft.isNew ? '' : draft.document.id,
      title: draft.document.title.trim() || '未命名文档',
    }).finally(() => {
      if (documentSavePromises.current.get(draft.key) === request) documentSavePromises.current.delete(draft.key);
    });
    documentSavePromises.current.set(draft.key, request);
    return request;
  }, []);

  const handleQuitRequest = useCallback(() => {
    if (Object.values(documentDraftsRef.current).some(isDocumentDirty)
      || configDraftsRef.current.service || configDraftsRef.current.agent || configDraftsRef.current.settings || configDraftsRef.current.content) {
      setQuitPrompt(true);
    } else {
      void api.quit().catch((reason: unknown) => setGlobalError(`退出应用失败：${errorText(reason)}`));
    }
  }, []);

  useEffect(() => {
    let unsubscribe: (() => void) | null = null;
    void onQuitRequested(handleQuitRequest).then((removeListener) => { unsubscribe = removeListener; });
    return () => unsubscribe?.();
  }, [handleQuitRequest]);

  useEffect(() => {
    const beforeUnload = (event: BeforeUnloadEvent) => {
      // 浏览器退出提示同时覆盖父级文档草稿与抽屉中的配置草稿，避免关闭窗口绕过应用内保存流程。
      if (Object.values(documentDraftsRef.current).some(isDocumentDirty)
        || configDraftsRef.current.service || configDraftsRef.current.agent || configDraftsRef.current.settings || configDraftsRef.current.content) {
        event.preventDefault();
        event.returnValue = '';
      }
    };
    window.addEventListener('beforeunload', beforeUnload);
    return () => window.removeEventListener('beforeunload', beforeUnload);
  }, []);

  const showService = (serviceId: string) => {
    setActivePage('services');
    setActiveServiceId(serviceId);
    setActiveTab('overview');
    setAgentFailure(null);
  };

  const saveService = async (service: ServiceConfig) => {
    const saved = await api.saveService(service);
    updateServices((previous) => previous.some((item) => item.id === saved.id)
      ? previous.map((item) => item.id === saved.id ? saved : item)
      : [...previous, saved]);
    setInvalidPathByService((previous) => ({ ...previous, [saved.id]: false }));
    setServiceErrors((previous) => ({ ...previous, [saved.id]: '' }));
    if (!runtimeRef.current[saved.id]) setRuntimeMap({ ...runtimeRef.current, [saved.id]: runtimeForService(saved) });
    setActivePage('services');
    setActiveServiceId(saved.id);
    setDrawer(null);
    setToast(service.id ? '服务配置已保存。' : '服务已创建。');
  };

  const applyBootstrap = (data: Bootstrap) => {
    updateServices(data.services);
    setAgents(data.agents);
    setPreferences(data.preferences);
    const nextRuntimeMap = Object.fromEntries(data.runtimes.map((runtime) => [runtime.serviceId, runtime]));
    for (const runtime of data.runtimes) {
      if (runtime.runId) runIdGuard.current[runtime.serviceId] = runtime.runId;
      else delete runIdGuard.current[runtime.serviceId];
    }
    setRuntimeMap(nextRuntimeMap);
    updateSessions(data.sessions);
  };

  const reloadBootstrap = async () => {
    const data = await api.bootstrap();
    applyBootstrap(data);
    return data;
  };

  const resetServiceRuntimeGuard = (serviceId: string) => {
    // 外部重新关联或转换可能切换 runId 的命名空间；删除旧快照，避免旧 guard 拦住新来源事件。
    delete runIdGuard.current[serviceId];
    const nextRuntimeMap = { ...runtimeRef.current };
    delete nextRuntimeMap[serviceId];
    setRuntimeMap(nextRuntimeMap);
  };

  const attachExternal = async (request: AttachExternalRequest, command: string) => {
    const attached = await api.attachExternal(request);
    let saved = attached;
    // attach_external 请求只包含可安全确认的 identity 与基础元数据；可选启动配方
    // 仍经 save_service 单独保存，命令为空时也允许先接入为纯观察记录。
    if (command !== attached.command) {
      try {
        saved = await api.saveService({ ...attached, command });
      } catch (reason) {
        setGlobalError(`外部进程已接入，但启动命令未能保存：${errorText(reason)}`);
      }
    }
    updateServices((previous) => previous.some((service) => service.id === saved.id)
      ? previous.map((service) => service.id === saved.id ? saved : service)
      : [...previous, saved]);
    resetServiceRuntimeGuard(saved.id);
    setRuntimeMap({ ...runtimeRef.current, [saved.id]: runtimeForService(saved) });
    setActivePage('services');
    setActiveServiceId(saved.id);
    setActiveTab('overview');
    setDrawer(null);
    setToast(request.existingServiceId ? `“${saved.name}”已重新关联。` : `“${saved.name}”已接入。`);
    try {
      await reloadBootstrap();
      setActiveServiceId(saved.id);
    } catch (reason) {
      setGlobalError(`外部进程已接入，但无法刷新运行状态：${errorText(reason)}`);
    }
  };

  const attachApplication = async (request: AttachApplicationRequest) => {
    const attached = await api.attachApplication(request);
    updateServices((previous) => previous.some((service) => service.id === attached.id)
      ? previous.map((service) => service.id === attached.id ? attached : service)
      : [...previous, attached]);
    resetServiceRuntimeGuard(attached.id);
    setRuntimeMap({ ...runtimeRef.current, [attached.id]: runtimeForService(attached) });
    setActivePage('services');
    setActiveServiceId(attached.id);
    setActiveTab('overview');
    setDrawer(null);
    setToast(request.existingServiceId ? `“${attached.name}”已重新关联。` : `“${attached.name}”已接入。`);
    try {
      await reloadBootstrap();
      setActiveServiceId(attached.id);
    } catch (reason) {
      setGlobalError(`应用已接入，但无法刷新运行状态：${errorText(reason)}`);
    }
  };

  const attachCandidateToService = async (serviceId: string, candidate: DiscoveredApplication) => {
    const service = servicesRef.current.find((item) => item.id === serviceId);
    if (!service || service.kind !== 'desktopApp' || service.launch.type !== 'desktopApp') {
      setLaunchCandidates(null);
      return;
    }
    setCandidateBusy(true);
    try {
      const attached = await api.attachApplication({
        identity: candidate.instance.identity,
        path: candidate.path || service.launch.path,
        name: service.name,
        notes: service.notes,
        workingDir: service.workingDir,
        existingServiceId: service.id,
      });
      updateServices((previous) => previous.map((item) => item.id === attached.id ? attached : item));
      resetServiceRuntimeGuard(attached.id);
      setLaunchCandidates(null);
      await reloadBootstrap();
      setActiveServiceId(attached.id);
      setToast(`已关联“${attached.name}”的运行实例。`);
    } catch (reason) {
      setServiceErrors((previous) => ({ ...previous, [service.id]: `无法关联运行实例：${errorText(reason)}` }));
      setLaunchCandidates(null);
    } finally {
      setCandidateBusy(false);
    }
  };

  const attachLaunchCandidate = async (candidate: DiscoveredApplication) => {
    if (!launchCandidates) return;
    await attachCandidateToService(launchCandidates.serviceId, candidate);
  };

  const savePreferences = async (next: Preferences): Promise<Preferences> => {
    // 整体保存偏好，切换工作台主题时不能丢失终端的独立外观配置。
    const saved = await api.savePreferences(next);
    setPreferences(saved);
    return saved;
  };

  const mergeSession = (session: TerminalSession) => {
    updateSessions((previous) => {
      const current = previous.find((item) => item.id === session.id);
      if (!current) return [...previous, session];
      // API 调用返回值可能晚于终止事件；旧响应不能把同一会话从已结束改回运行中。
      if (current.state === 'exited' && session.state === 'running') return previous;
      return previous.map((item) => item.id === session.id ? session : item);
    });
  };

  const refreshRuntimeAfterOperation = async (serviceId: string, operation: ServiceOperation, returnedRunId?: string | null) => {
    const snapshot = await api.bootstrap();
    const next = snapshot.runtimes.find((runtime) => runtime.serviceId === serviceId);
    if (!next) return;
    const current = runtimeRef.current[serviceId];
    const guardedRunId = runIdGuard.current[serviceId] ?? current?.runId ?? null;
    const isTerminal = (status?: ServiceRuntime['status']) => status === 'failed' || status === 'stopped';
    const isActive = (status: ServiceRuntime['status']) => status === 'starting' || status === 'running' || status === 'stopping';

    // 操作完成后用 Rust 当前快照补齐可能漏掉的事件；同一 runId 的终态优先，
    // 新实例只有在服务启动/重启响应的 runId 与快照一致时才可替换旧实例。
    if (current && isTerminal(current.status) && current.runId === next.runId && isActive(next.status)) return;
    if ((operation === 'start' || operation === 'restart') && returnedRunId && next.runId === returnedRunId) {
      runIdGuard.current[serviceId] = returnedRunId;
      setRuntimeMap({ ...runtimeRef.current, [serviceId]: next });
      return;
    }
    if (operation === 'stop' && !next.runId && (!guardedRunId || current?.runId === guardedRunId)) {
      setRuntimeMap({ ...runtimeRef.current, [serviceId]: next });
      return;
    }
    acceptRuntime(next);
  };

  const performOperation = async (operation: ServiceOperation, targetServiceId?: string) => {
    const serviceId = targetServiceId ?? activeService?.id;
    if (!serviceId) return;
    const service = serviceId ? servicesRef.current.find((item) => item.id === serviceId) : undefined;
    const runtime = serviceId && service ? runtimeRef.current[serviceId] ?? runtimeForService(service) : activeRuntime;
    if (!service || !runtime || !isDesktop) return;
    if (operationByService[serviceId]) return;
    if (operation === 'start' && !runtime.canStart) {
      setServiceErrors((previous) => ({ ...previous, [serviceId]: runtime.actionReasons.start || runtime.controlReason || '当前服务暂不可启动。' }));
      return;
    }
    if (operation === 'restart' && !runtime.canRestart) {
      setServiceErrors((previous) => ({ ...previous, [serviceId]: runtime.actionReasons.restart || runtime.controlReason || '当前服务暂不可重启。' }));
      return;
    }
    if (operation === 'stop' && !runtime.canStop) {
      setServiceErrors((previous) => ({ ...previous, [serviceId]: runtime.actionReasons.stop || runtime.controlReason || '当前服务暂不可停止。' }));
      return;
    }
    setOperationByService((previous) => ({ ...previous, [serviceId]: operation }));
    setServiceErrors((previous) => ({ ...previous, [serviceId]: '' }));
    try {
      let session: TerminalSession | null = null;
      let returnedRunId: string | null = null;
      if (operation === 'start' || operation === 'restart') {
        const result = operation === 'start' ? await api.startService(serviceId) : await api.restartService(serviceId);
        session = result.session;
        returnedRunId = result.runtime.runId;
        acceptRuntime(result.runtime);
        if (result.candidates.length === 1) {
          await attachCandidateToService(serviceId, result.candidates[0]);
        } else if (result.candidates.length > 1) {
          setLaunchCandidates({ serviceId, candidates: result.candidates });
          setSelectedLaunchCandidate(`${result.candidates[0].instance.identity.pid}:${result.candidates[0].instance.identity.createdAt}`);
        }
      } else await api.stopService(serviceId);

      if (session) {
        mergeSession(session);
      }
      await refreshRuntimeAfterOperation(serviceId, operation, returnedRunId ?? session?.runId);
    } catch (reason) {
      const message = errorText(reason);
      if (/目录|路径|pyvenv/i.test(message)) setInvalidPathByService((previous) => ({ ...previous, [serviceId]: true }));
      setServiceErrors((previous) => ({ ...previous, [serviceId]: operation === 'start' ? `服务未能启动：${message}` : operation === 'restart' ? `服务重启失败：${message}` : `停止服务失败：${message}` }));
    } finally {
      setOperationByService((previous) => ({ ...previous, [serviceId]: undefined }));
    }
  };

  const performDesktopAction = async (kind: 'focus' | 'requestQuit' | 'unlock', targetServiceId?: string) => {
    const serviceId = targetServiceId ?? activeService?.id;
    const service = serviceId ? servicesRef.current.find((item) => item.id === serviceId) : undefined;
    if (!serviceId || !service || service.kind !== 'desktopApp' || !isDesktop || desktopBusyByService[serviceId]) return;
    const runtime = runtimeRef.current[serviceId] ?? runtimeForService(service);
    const allowed = kind === 'focus' ? runtime.canFocus : kind === 'requestQuit' ? runtime.canRequestQuit : runtime.canUnlock && !runtime.protectionUnlocked;
    if (!allowed) {
      const actionName = kind === 'focus' ? '显示应用' : kind === 'requestQuit' ? '请求退出' : '解锁控制';
      setServiceErrors((previous) => ({ ...previous, [serviceId]: runtime.actionReasons[kind] || runtime.controlReason || `当前实例暂不可${actionName}。` }));
      return;
    }
    setDesktopBusyByService((previous) => ({ ...previous, [serviceId]: kind }));
    setServiceErrors((previous) => ({ ...previous, [serviceId]: '' }));
    try {
      if (kind === 'focus') {
        await api.focusApplication(serviceId);
        setToast(`已请求显示“${service.name}”。`);
      } else {
        const updated = kind === 'requestQuit'
          ? await api.requestQuitApplication(serviceId)
          : await api.unlockApplication(serviceId);
        acceptRuntime(updated);
        if (kind === 'unlock') setToast(`“${service.name}”的当前实例已临时解锁。`);
        else if (isActiveRuntime(updated.status)) {
          if (updated.canForceQuit) setDesktopPrompt({ kind: 'force', serviceId, afterRequest: true });
          else setToast(updated.actionReasons.forceQuit || '已发送正常退出请求；应用仍运行，可稍后刷新状态。');
        } else setToast(`“${service.name}”已退出。`);
      }
    } catch (reason) {
      setServiceErrors((previous) => ({ ...previous, [serviceId]: `桌面应用操作失败：${errorText(reason)}` }));
    } finally {
      setDesktopBusyByService((previous) => ({ ...previous, [serviceId]: undefined }));
    }
  };

  const confirmDesktopAction = async () => {
    if (!desktopPrompt || candidateBusy) return;
    const { kind, serviceId } = desktopPrompt;
    const service = servicesRef.current.find((item) => item.id === serviceId);
    const runtime = service ? runtimeRef.current[serviceId] ?? runtimeForService(service) : null;
    if (!service || service.kind !== 'desktopApp' || !runtime) { setDesktopPrompt(null); return; }
    if (kind === 'restart') {
      if (!runtime.canRestart) {
        setServiceErrors((previous) => ({ ...previous, [serviceId]: runtime.actionReasons.restart || runtime.controlReason || '当前应用暂不可重启。' }));
        setDesktopPrompt(null);
        return;
      }
      setDesktopPrompt(null);
      await performOperation('restart', serviceId);
      return;
    }
    if (!runtime.canForceQuit) {
      setServiceErrors((previous) => ({ ...previous, [serviceId]: runtime.actionReasons.forceQuit || runtime.controlReason || '当前实例不允许强制结束。' }));
      setDesktopPrompt(null);
      return;
    }
    setDesktopBusyByService((previous) => ({ ...previous, [serviceId]: 'forceQuit' }));
    try {
      const updated = await api.forceQuitApplication(serviceId);
      acceptRuntime(updated);
      setDesktopPrompt(null);
      if (isActiveRuntime(updated.status)) setServiceErrors((previous) => ({ ...previous, [serviceId]: updated.error || '强制结束后应用仍被检测为运行中。' }));
      else setToast(`“${service.name}”已强制结束。`);
    } catch (reason) {
      setServiceErrors((previous) => ({ ...previous, [serviceId]: `强制结束失败：${errorText(reason)}` }));
    } finally {
      setDesktopBusyByService((previous) => ({ ...previous, [serviceId]: undefined }));
    }
  };

  const confirmExternalAction = async () => {
    if (!externalAction || externalActionBusy) return;
    const { kind, serviceId } = externalAction;
    const service = servicesRef.current.find((item) => item.id === serviceId);
    if (!service || service.source !== 'external') {
      setExternalAction(null);
      return;
    }
    const runtime = runtimeRef.current[serviceId] ?? runtimeForService(service);
    if (service.external?.sharedHost) {
      setExternalAction(null);
      setGlobalError('共享宿主始终只读，不能执行停止、解锁或转换。');
      return;
    }
    setExternalActionBusy(true);
    setGlobalError('');
    try {
      if (kind === 'stop') {
        if (!runtime.canStop) throw new Error(runtime.controlReason || '当前实例不允许停止。');
        setExternalAction(null);
        await performOperation('stop', serviceId);
        return;
      }
      if (kind === 'unlock') {
        if (!runtime.canUnlock || runtime.protectionUnlocked) throw new Error(runtime.controlReason || '当前实例不允许解锁。');
        const unlocked = await api.unlockExternal(serviceId);
        acceptRuntime(unlocked);
        setExternalAction(null);
        setToast(`PID ${service.external?.identity.pid ?? unlocked.pid ?? ''} 已对当前实例临时解锁。`);
        return;
      }

      if (!runtime.canConvert || !service.workingDir.trim() || !service.command.trim()) {
        throw new Error(runtime.controlReason || '转换前需要有效的工作目录与启动命令。');
      }
      setExternalAction(null);
      let convertedSession: TerminalSession | null = null;
      let conversionError = '';
      let refreshError = '';
      try {
        // 成功会切换为新的 managed runId；失败可能退回 external runtime，两条路径都在 finally 重读来源。
        convertedSession = await api.convertExternal(serviceId);
      } catch (reason) {
        conversionError = errorText(reason);
      } finally {
        resetServiceRuntimeGuard(serviceId);
        try {
          await reloadBootstrap();
        } catch (reason) {
          refreshError = errorText(reason);
        }
      }
      if (convertedSession) {
        mergeSession(convertedSession);
        showService(serviceId);
        setToast(`“${service.name}”已转换为平台托管并启动。`);
      }
      if (conversionError) setGlobalError(`转换外部进程失败：${conversionError}${refreshError ? `；刷新状态失败：${refreshError}` : ''}`);
      else if (refreshError) setGlobalError(`“${service.name}”已转换，但运行状态刷新失败：${refreshError}`);
    } catch (reason) {
      setGlobalError(errorText(reason));
    } finally {
      setExternalActionBusy(false);
    }
  };

  const openOrdinaryTerminal = async () => {
    if (!activeService) return;
    if (!activeService.workingDir.trim()) {
      setGlobalError('请先为此服务补充工作目录，再创建独立终端。');
      return;
    }
    try {
      const session = await api.createTerminal(activeService.id);
      mergeSession(session);
    } catch (reason) {
      setGlobalError(`无法新建终端：${errorText(reason)}`);
    }
  };

  const launchAgent = async (agent: AgentProfile) => {
    if (!activeService || !isDesktop) return;
    if (!activeService.workingDir.trim()) {
      setAgentFailure({ serviceId: activeService.id, agentId: agent.id, message: '请先为此服务补充工作目录，再启动 Agent。' });
      return;
    }
    setAgentMenuOpen(false);
    setCheckingAgentId(agent.id);
    setAgentFailure(null);
    try {
      const available: AgentAvailability = await api.checkAgent(agent.id, activeService.id);
      if (!available.available) {
        setAgentFailure({
          serviceId: activeService.id,
          agentId: agent.id,
          message: available.error ?? '未找到可执行程序，请安装对应 CLI，或在 AI 工具设置中指定程序路径。',
        });
        return;
      }
      const session = await api.createTerminal(activeService.id, agent.id);
      mergeSession(session);
      setSelectedAgentByService((previous) => ({ ...previous, [activeService.id]: agent.id }));
    } catch (reason) {
      setAgentFailure({ serviceId: activeService.id, agentId: agent.id, message: errorText(reason) });
    } finally {
      setCheckingAgentId('');
    }
  };

  const recheckAgent = async (agentId: string) => {
    if (!activeService) return;
    setCheckingAgentId(agentId);
    try {
      const result = await api.checkAgent(agentId, activeService.id);
      if (result.available) {
        setAgentFailure(null);
        setToast(`已找到 Agent 程序：${result.resolvedPath ?? '检测通过'}。现在可以从“打开 Agent”启动。`);
      } else {
        setAgentFailure({ serviceId: activeService.id, agentId, message: result.error ?? '未找到可执行程序，请检查安装或工具路径。' });
      }
    } catch (reason) {
      setAgentFailure({ serviceId: activeService.id, agentId, message: errorText(reason) });
    } finally {
      setCheckingAgentId('');
    }
  };

  const closeTerminal = async (sessionId: string) => {
    try {
      await api.closeTerminal(sessionId);
    } catch (reason) {
      setGlobalError(`关闭终端失败：${errorText(reason)}`);
    }
  };

  const copyText = async (value: string, label: string) => {
    try {
      await navigator.clipboard.writeText(value);
      setToast(`${label}已复制。`);
    } catch {
      setGlobalError('无法访问剪贴板，请手动选择并复制内容。');
    }
  };

  const openServiceDirectory = async (serviceId: string, kind: 'work' | 'environment') => {
    try {
      await api.openDirectory(serviceId, kind);
    } catch (reason) {
      setGlobalError(`无法打开${kind === 'work' ? '工作' : '环境'}目录：${errorText(reason)}`);
    }
  };

  const removeService = async () => {
    const serviceId = deleteTargetId;
    const service = servicesRef.current.find((item) => item.id === serviceId);
    if (!service) return;
    if (operationByService[serviceId]) {
      setDeleteError('服务操作仍在进行中，请稍候再删除。');
      return;
    }
    setDeleteBusy(true);
    setDeleteError('');
    try {
      // 后端在一个受管操作中停止服务并回收所有终端/Agent；前端只发起一次删除，
      // 失败时保留配置与当前状态，避免重复 stop/close 与后端运行事件竞争。
      await api.deleteService(serviceId);
      const remainingServices = servicesRef.current.filter((item) => item.id !== serviceId);
      servicesRef.current = remainingServices;
      updateServices(remainingServices);
      updateSessions((previous) => previous.filter((session) => session.serviceId !== serviceId));
      updateDocumentDrafts((previous) => Object.fromEntries(Object.entries(previous).filter(([, draft]) => draft.document.serviceId !== serviceId)));
      setDocumentsLoaded((previous) => { const next = { ...previous }; delete next[serviceId]; return next; });
      setActiveDocumentByService((previous) => { const next = { ...previous }; delete next[serviceId]; return next; });
      setActiveServiceId((current) => current === serviceId ? (remainingServices[0]?.id ?? '') : current);
      setRuntimeMap(Object.fromEntries(Object.entries(runtimeRef.current).filter(([id]) => id !== serviceId)));
      delete runIdGuard.current[serviceId];
      setDeleteTargetId('');
      setToast(`“${service.name}”已删除。`);
    } catch (reason) {
      setDeleteError(`无法完成删除：${errorText(reason)}。服务记录已保留，请检查状态后重试。`);
    } finally {
      setDeleteBusy(false);
    }
  };

  const deleteService = services.find((service) => service.id === deleteTargetId);
  const deleteServiceRuntime = deleteService ? runtimeRef.current[deleteTargetId] ?? runtimeForService(deleteService) : null;
  const deleteSessions = deleteTargetId ? sessions.filter((session) => session.serviceId === deleteTargetId && session.kind !== 'service' && session.state === 'running') : [];
  // 外部服务的运行态属于用户原有进程；删除记录只会结束应用自己创建的终端/Agent。
  const deleteNeedsStop = deleteService?.kind === 'desktopApp' || deleteService?.source === 'external'
    ? deleteSessions.length > 0
    : Boolean(deleteServiceRuntime && isActiveRuntime(deleteServiceRuntime.status)) || deleteSessions.length > 0;
  const selectedAgent = activeService ? agents.find((agent) => agent.id === selectedAgentByService[activeService.id]) : undefined;
  const externalActionService = externalAction ? services.find((service) => service.id === externalAction.serviceId && service.source === 'external') : undefined;
  const externalActionRuntime = externalActionService ? runtimes[externalActionService.id] ?? runtimeForService(externalActionService) : undefined;
  // 停止与转换都会影响整个外部进程。两种确认框都展示该实例全部已知端口，
  // 探测失败时明确说明使用旧快照，不能让空列表被误读为“该进程没有其他用途”。
  const externalActionPorts = externalActionRuntime && <div className="external-confirm-ports">
    <strong>该进程的全部已知端口</strong>
    {externalActionRuntime.ports.length ? externalActionRuntime.ports.map((port, index) => <code key={`${port.protocol}:${port.address}:${port.port}:${index}`}>{port.protocol === 'TCP' ? '监听' : '绑定'} {port.protocol} {formatAddressPort(port.address, port.port)} · PID {port.pid ?? '未知'}</code>) : <span>当前快照没有可显示的端口，不代表进程没有在执行任务。</span>}
    {externalActionRuntime.stale && <span>快照已过期，以上为最近一次成功探测结果。</span>}
    {externalActionRuntime.portError && <span>端口探测失败：{externalActionRuntime.portError}</span>}
  </div>;
  const externalActionAllowed = externalAction && externalActionService && externalActionRuntime
    ? externalAction.kind === 'stop'
      ? externalActionRuntime.canStop
      : externalAction.kind === 'unlock'
        ? externalActionRuntime.canUnlock && !externalActionRuntime.protectionUnlocked
        : externalActionRuntime.canConvert && Boolean(externalActionService.workingDir.trim() && externalActionService.command.trim())
    : false;
  const selectedLaunchApplication = launchCandidates?.candidates.find((candidate) => `${candidate.instance.identity.pid}:${candidate.instance.identity.createdAt}` === selectedLaunchCandidate);

  const filteredServices = useMemo(() => {
    const query = search.trim().toLocaleLowerCase();
    return services.filter((service) => {
      const runtime = runtimes[service.id] ?? runtimeForService(service);
      const launchSearch = service.launch.type === 'frontend'
        ? `${service.launch.packageManager} ${service.launch.script} ${service.launch.url}`
        : service.launch.type === 'staticSite'
          ? `${service.launch.root} ${service.launch.port}`
          : service.launch.type === 'desktopApp'
            ? `${service.launch.path} ${service.launch.startDir}`
            : service.command;
      const matchesQuery = !query || `${service.name} ${service.description} ${service.workingDir} ${launchSearch}`.toLocaleLowerCase().includes(query);
      const matchesState = filterStatus === 'all'
        || (filterStatus === 'running' && isActiveRuntime(runtime.status))
        || (filterStatus === 'stopped' && runtime.status === 'stopped')
        || (filterStatus === 'failed' && runtime.status === 'failed');
      const matchesKind = filterServiceKind === 'all' || service.kind === filterServiceKind;
      return matchesQuery && matchesState && matchesKind;
    });
  }, [services, runtimes, search, filterStatus, filterServiceKind]);

  const dirtyCount = dirtyDocuments.length;
  const dirtyConfigLabels = [
    ...(serviceDrawerDirty || configDraftsRef.current.service ? ['服务配置'] : []),
    ...(agentDrawerDirty || configDraftsRef.current.agent ? ['AI 工具配置'] : []),
    ...(settingsDrawerDirty || configDraftsRef.current.settings ? ['外观设置'] : []),
    ...(configDraftsRef.current.content ? ['内容收藏'] : []),
  ];
  const hasDirtyConfig = dirtyConfigLabels.length > 0;
  const activeOperation = activeService ? operationByService[activeService.id] : undefined;
  // 后端停止失败会保留 stopping；操作空闲时允许重试，并发请求仍由 activeOperation 拦截。
  const canStop = Boolean(activeRuntime?.canStop);
  const activeExternal = activeService?.kind !== 'desktopApp' && activeService?.source === 'external';
  const activeExternalCanStop = Boolean(activeExternal && activeRuntime?.canStop);
  const activeExternalHasWorkingDirectory = Boolean(activeService?.workingDir.trim());
  // 所有独立终端和 Agent 都要有明确的本地工作目录，桌面应用的启动工作目录单独处理。
  const activeCanCreateSession = Boolean(activeExternalHasWorkingDirectory);
  const windowsExternalTermination = /Windows/i.test(navigator.userAgent) || navigator.platform.toLowerCase().startsWith('win');
  const validToStart = serviceConfigurationReady(activeService) && !invalidPathByService[activeService?.id ?? ''];
  const activeType = activeService?.kind ?? 'command';
  const startDisabledTitle = !isDesktop
    ? '运行操作请使用桌面应用'
    : activeService && invalidPathByService[activeService.id]
      ? '目录配置不可用，请编辑服务重新选择目录'
      : !validToStart
        ? activeType === 'frontend' ? '请先配置项目目录、包管理器和启动脚本' : activeType === 'staticSite' ? '请先选择静态站点目录' : activeType === 'desktopApp' ? '请先选择桌面应用路径' : '请先配置工作目录和启动命令'
        : '启动服务';

  const focusServiceOutput = () => {
    if (!activeService) return;
    const outputSession = activeSessions.find((session) => session.kind === 'service');
    if (!outputSession) {
      setToast('当前服务尚无输出会话。');
      return;
    }
    window.requestAnimationFrame(() => {
      const buttons = Array.from(document.querySelectorAll<HTMLButtonElement>('.kc-terminal-tab [role="tab"]'));
      buttons.find((button) => button.textContent?.includes(outputSession.title))?.click();
    });
  };

  const saveQuitAndExit = async () => {
    // 此动作只用于没有配置草稿时；配置可能尚未通过表单校验，不能在退出时静默提交。
    if (configDraftsRef.current.service || configDraftsRef.current.agent || configDraftsRef.current.settings || configDraftsRef.current.content) {
      setGlobalError('服务、AI 工具或外观设置仍有未保存修改，请返回抽屉手动保存或明确放弃。');
      return;
    }
    setQuitBusy(true);
    setGlobalError('');
    try {
      await saveAllDocumentDrafts(documentDraftsRef.current, saveDocument, updateDocumentDrafts);
      await api.quit();
      setQuitPrompt(false);
    } catch (reason) {
      setGlobalError(errorText(reason));
    } finally {
      setQuitBusy(false);
    }
  };

  const saveDocumentsAndReturnToEdit = async () => {
    setQuitBusy(true);
    setGlobalError('');
    try {
      // 存在配置草稿时仅保存文档并留在应用内，要求用户回到配置抽屉完成校验和保存。
      await saveAllDocumentDrafts(documentDraftsRef.current, saveDocument, updateDocumentDrafts);
      setQuitPrompt(false);
      setToast('文档已保存；配置草稿仍在抽屉中，请完成保存后再退出。');
    } catch (reason) {
      setGlobalError(`文档保存失败：${errorText(reason)}`);
    } finally {
      setQuitBusy(false);
    }
  };

  const discardAndExit = async () => {
    setQuitBusy(true);
    setGlobalError('');
    try {
      await api.quit();
      setQuitPrompt(false);
    } catch (reason) {
      setGlobalError(`退出应用失败：${errorText(reason)}`);
    } finally {
      setQuitBusy(false);
    }
  };

  const retryBootstrap = () => {
    setBootstrapLoading(true);
    setBootstrapError('');
    void api.bootstrap().then((data) => {
      updateServices(data.services);
      setAgents(data.agents);
      setPreferences(data.preferences);
      setRuntimeMap(Object.fromEntries(data.runtimes.map((runtime) => [runtime.serviceId, runtime])));
      updateSessions(data.sessions);
      setActiveServiceId(data.services[0]?.id ?? '');
    }).catch((reason: unknown) => setBootstrapError(errorText(reason))).finally(() => setBootstrapLoading(false));
  };

  return (
    <div className={`app-root ${isPreview ? 'is-preview' : ''}`} data-theme={resolvedTheme}>
      {isPreview && <div className="app-titlebar" data-tauri-drag-region>
        <div className="titlebar-dots" aria-hidden="true"><i /><i /><i /></div>
        <span>服务中心</span>
        <span className="titlebar-spacer" />
      </div>}

      <div className="app-layout">
        <aside className="sidebar">
          <div className="brand-row"><div className="brand-mark" aria-hidden="true"><LayoutGrid size={20} /></div><strong>服务中心</strong></div>
          <div className="service-search-row" hidden={activePage === 'content'}>
            <label className="search-box"><Search size={17} /><input aria-label={activePage === 'discovery' ? '搜索端口、名称、PID 或路径' : '搜索服务'} placeholder={activePage === 'discovery' ? '搜索端口、名称、PID 或路径…' : '搜索服务…'} value={activePage === 'discovery' ? discoverySearch : search} onChange={(event) => activePage === 'discovery' ? setDiscoverySearch(event.target.value) : setSearch(event.target.value)} /><kbd>⌘ K</kbd></label>
            <div className="filter-wrap">
              <button className={`icon-button filter-trigger ${(activePage === 'discovery' ? discoveryCategories.length !== 3 : filterStatus !== 'all' || filterServiceKind !== 'all') ? 'is-active' : ''}`} type="button" aria-label={activePage === 'discovery' ? '筛选进程分类' : '筛选服务状态和类型'} title={activePage === 'discovery' ? '筛选进程分类' : '筛选服务状态和类型'} aria-expanded={filterOpen} onClick={() => setFilterOpen((open) => !open)}><Filter size={17} /></button>
              {filterOpen && <div className="popover filter-popover" role="menu" aria-label={activePage === 'discovery' ? '进程分类筛选' : '服务状态筛选'}>
                {activePage === 'discovery' ? <>
                  {(['nonSystem', 'system', 'unknown'] as DiscoveryCategory[]).map((category) => <button key={category} className={discoveryCategories.includes(category) ? 'selected' : ''} type="button" role="menuitemcheckbox" aria-checked={discoveryCategories.includes(category)} onClick={() => setDiscoveryCategories((previous) => previous.includes(category) ? previous.filter((value) => value !== category) : [...previous, category])}><span>{category === 'nonSystem' ? '非系统进程' : category === 'system' ? '系统进程' : '待确认'}</span>{discoveryCategories.includes(category) && <Check size={14} />}</button>)}
                  <button className={discoveryCategories.length === 3 ? 'selected' : ''} type="button" role="menuitemradio" aria-checked={discoveryCategories.length === 3} onClick={() => { setDiscoveryCategories(['nonSystem', 'system', 'unknown']); setFilterOpen(false); }}><span>全部分类</span>{discoveryCategories.length === 3 && <Check size={14} />}</button>
                  <button className="clear-filter" type="button" onClick={() => { setDiscoveryCategories(['nonSystem', 'unknown']); setDiscoverySearch(''); setFilterOpen(false); }}>恢复默认（非系统 + 待确认）</button>
                </> : <>
                  <div className="popover-heading">运行状态</div>
                  {[['all', '全部状态'], ['running', '运行中'], ['stopped', '已停止'], ['failed', '异常退出']].map(([value, label]) => <button key={value} className={filterStatus === value ? 'selected' : ''} type="button" role="menuitemradio" aria-checked={filterStatus === value} onClick={() => { setFilterStatus(value as typeof filterStatus); }}><span>{label}</span>{filterStatus === value && <Check size={14} />}</button>)}
                  <div className="popover-heading">服务类型</div>
                  {([['all', '全部类型'], ['command', '命令服务'], ['frontend', '前端项目'], ['staticSite', '静态站点'], ['desktopApp', '桌面应用']] as Array<[ServiceKindFilter, string]>).map(([value, label]) => <button key={value} className={filterServiceKind === value ? 'selected' : ''} type="button" role="menuitemradio" aria-checked={filterServiceKind === value} onClick={() => { setFilterServiceKind(value); setFilterOpen(false); }}><span>{label}</span>{filterServiceKind === value && <Check size={14} />}</button>)}
                  {(search || filterStatus !== 'all' || filterServiceKind !== 'all') && <button className="clear-filter" type="button" onClick={() => { setSearch(''); setFilterStatus('all'); setFilterServiceKind('all'); setFilterOpen(false); }}>清除筛选</button>}
                </>}
              </div>}
            </div>
          </div>

          <nav className="sidebar-navigation" aria-label="工作区">
            <button type="button" className={activePage === 'content' ? 'active' : ''} onClick={() => setActivePage('content')}><Library size={16} /><span>内容库</span></button>
            <button type="button" className={activePage === 'services' ? 'active' : ''} onClick={() => setActivePage('services')}><LayoutGrid size={16} /><span>服务管理</span></button>
            <button type="button" className={activePage === 'discovery' ? 'active' : ''} onClick={() => { setActivePage('discovery'); setActiveDiscoveryMode('ports'); setSelectedDiscoveryProcess(null); }}><Cpu size={16} /><span>发现服务</span>{isPreview && <small>桌面</small>}</button>
          </nav>

          <div className="service-list-heading" hidden={activePage === 'content'}><div><strong>服务</strong><span>{services.length}</span></div><button className="icon-button small" type="button" aria-label="新增服务" title="新增服务" onClick={() => setDrawer({ kind: 'service', mode: 'create' })}><Plus size={17} /></button></div>
          <div className="service-list" aria-label="服务列表" hidden={activePage === 'content'}>
            {bootstrapLoading ? <div className="sidebar-loading"><span className="spinner" />正在载入…</div> : filteredServices.length > 0 ? filteredServices.map((service) => {
              const runtime = runtimes[service.id] ?? runtimeForService(service);
              const externalPorts = [...new Set(runtime.ports.map((port) => `${port.protocol} ${port.port}`))];
              const externalPortSummary = externalPorts.join(', ');
              const isDesktopApp = service.kind === 'desktopApp';
              const portSummary = externalPortSummary || (service.expectedPorts.length ? service.expectedPorts.join(', ') : '');
              const ports = portSummary ? ` · ${portSummary}` : '';
              const listStatus = service.source === 'external' && !isDesktopApp ? `外部 · ${statusText[runtime.status]}` : statusText[runtime.status];
              const listSummary = service.source === 'external' && !isDesktopApp
                ? [externalPortSummary, '外部', statusText[runtime.status]].filter(Boolean).join(' · ')
                : `${listStatus}${ports}`;
              return <button key={service.id} type="button" className={`service-list-item ${service.id === activeServiceId ? 'selected' : ''}`} onClick={() => showService(service.id)}>
                <span className={`status-dot ${runtimeClass(runtime.status)}`} aria-label={statusText[runtime.status]} />
                <span className="service-list-copy"><strong>{service.name}</strong><small><span className={`service-kind-badge kind-${service.kind}`}>{serviceKindText[service.kind]}</span>{listSummary}</small></span>
              </button>;
            }) : services.length === 0 ? (
              <div className="sidebar-empty"><strong>还没有服务</strong><p>添加本地服务后，可在这里管理目录、运行状态、终端和文档。</p><button className="button primary compact" type="button" onClick={() => setDrawer({ kind: 'service', mode: 'create' })}><Plus size={14} /> 新增服务</button></div>
            ) : (
              <div className="sidebar-empty"><strong>没有找到匹配的服务</strong><p>试试其他名称，或清除当前筛选。</p><button className="text-button" type="button" onClick={() => { setSearch(''); setFilterStatus('all'); setFilterServiceKind('all'); }}>清除筛选</button></div>
            )}
          </div>

          {activePage === 'content' && <div className="content-sidebar-note"><Library size={26} /><strong>你的本机内容</strong><p>收藏常用文件夹、应用和游戏，用名称或别名随时打开。</p><small>退出中心后，应用和游戏继续运行。</small></div>}
          <div className="sidebar-bottom">
            <button className="sidebar-action" type="button" onClick={() => setDrawer({ kind: 'agents' })}><Wrench size={18} /><span>AI 工具</span>{agents.length > 0 && <small>{agents.length}</small>}</button>
            <button className="sidebar-action" type="button" onClick={() => setDrawer({ kind: 'settings' })}><Settings size={18} /><span>设置</span></button>
            <button className="sidebar-action theme-action" type="button" onClick={() => setDrawer({ kind: 'settings' })}><Monitor size={18} /><span>{preferences.theme === 'system' ? '跟随系统' : resolvedTheme === 'dark' ? '深色主题' : '浅色主题'}</span><ChevronDown size={14} /></button>
          </div>
        </aside>

        <main className="main-panel">
          <ContentWorkspace suspendDialogs={!!drawer || quitPrompt || !!deleteTargetId || !!desktopPrompt || !!externalAction || !!launchCandidates} visible={activePage === 'content' && !bootstrapLoading && !bootstrapError} agents={agents} onOpenService={showService} onSettings={() => setDrawer({ kind: 'settings', tab: 'content' })} onDirtyChange={reportContentDirty} />
          {bootstrapLoading ? <div className="workspace-message"><span className="spinner" /><strong>正在准备本地工作区…</strong></div> : bootstrapError ? (
            <div className="workspace-message error-workspace"><CircleAlert size={24} /><strong>无法载入服务列表</strong><p>{bootstrapError}</p><button className="button primary" type="button" onClick={retryBootstrap}>重新载入</button></div>
          ) : activePage === 'content' ? null : activePage === 'discovery' ? <>
            <div className="discovery-mode-tabs" role="tablist" aria-label="发现服务类型">
              <button type="button" role="tab" aria-selected={activeDiscoveryMode === 'ports'} className={activeDiscoveryMode === 'ports' ? 'active' : ''} onClick={() => { setActiveDiscoveryMode('ports'); setSelectedDiscoveryProcess(null); }}>端口服务</button>
              <button type="button" role="tab" aria-selected={activeDiscoveryMode === 'applications'} className={activeDiscoveryMode === 'applications' ? 'active' : ''} onClick={() => { setActiveDiscoveryMode('applications'); setSelectedDiscoveryProcess(null); }}>运行中应用</button>
            </div>
            {activeDiscoveryMode === 'applications' ? <ApplicationDiscoveryView
              snapshot={applicationSnapshot}
              isDesktop={isDesktop}
              refreshing={discoveryRefreshing}
              query={discoverySearch}
              categories={discoveryCategories}
              services={services}
              onQueryChange={setDiscoverySearch}
              onRefresh={() => void refreshApplications(true)}
              onAttach={(application) => setDrawer({ kind: 'attach-application', application })}
              onOpenManaged={showService}
            /> : <DiscoveryView
              snapshot={discoverySnapshot}
              isDesktop={isDesktop}
              refreshing={discoveryRefreshing}
              query={discoverySearch}
              categories={discoveryCategories}
              selected={selectedDiscoveryProcess}
              services={services}
              onQueryChange={setDiscoverySearch}
              onRefresh={() => void refreshDiscovery(true)}
              onSelect={setSelectedDiscoveryProcess}
              onBack={() => setSelectedDiscoveryProcess(null)}
              onAttach={(process) => setDrawer({ kind: 'attach-external', process })}
              onOpenManaged={showService}
            />}
          </> : activeService && activeRuntime ? <>
            <header className={`service-header${activeService.kind === 'desktopApp' ? ' desktop-app-service-header' : ''}`}>
              <div className="service-heading-copy">
              <div className="service-title-line"><h1>{activeService.name}</h1><span className={`service-kind-badge header-kind kind-${activeService.kind}`}>{serviceKindText[activeService.kind]}</span>{activeExternal && <span className="source-badge external-source"><ExternalLink size={13} />外部来源</span>}<span className={`status-badge ${runtimeClass(activeRuntime.status)}`}><i className="status-dot" />{statusText[activeRuntime.status]}</span></div>
                <div className="service-subtitle">{activeExternal ? '外部进程关联记录' : activeService.description || '本地服务'}{activeExternal && activeService.external?.identity.pid ? <><span className="meta-divider">|</span>PID {activeService.external.identity.pid}</> : activeRuntime.status === 'running' && activeRuntime.pid ? <><span className="meta-divider">|</span>PID {activeRuntime.pid}</> : null}{activeRuntime.startedAt && activeRuntime.status === 'running' && !activeExternal && activeService.kind !== 'desktopApp' ? <><span className="meta-divider">|</span>启动于 {prettyDate(activeRuntime.startedAt)}</> : null}</div>
              </div>
              <div className="service-controls">
                {activeService.kind === 'desktopApp' ? <>
                  <button className="button secondary" type="button" onClick={() => void performOperation('start')} disabled={!isDesktop || !activeRuntime.canStart || isActiveRuntime(activeRuntime.status) || Boolean(activeOperation)} title={!isDesktop ? '请在桌面应用中启动' : activeRuntime.actionReasons.start || activeRuntime.controlReason || '启动已配置的应用'}><Power size={16} />{activeOperation === 'start' ? '启动中…' : '启动'}</button>
                  <button className="button secondary" type="button" onClick={() => void performDesktopAction('focus')} disabled={!isDesktop || !activeRuntime.canFocus || Boolean(desktopBusyByService[activeService.id])} title={activeRuntime.actionReasons.focus || activeRuntime.controlReason || '显示并激活当前应用'}><AppWindow size={15} />{desktopBusyByService[activeService.id] === 'focus' ? '显示中…' : '显示应用'}</button>
                  <button className="button secondary" type="button" onClick={() => void performDesktopAction('requestQuit')} disabled={!isDesktop || !activeRuntime.canRequestQuit || Boolean(desktopBusyByService[activeService.id])} title={activeRuntime.actionReasons.requestQuit || activeRuntime.controlReason || '向应用发送正常退出请求'}><Square size={14} />{desktopBusyByService[activeService.id] === 'requestQuit' ? '等待退出…' : '请求退出'}</button>
                  <button className="button secondary" type="button" onClick={() => setDesktopPrompt({ kind: 'restart', serviceId: activeService.id })} disabled={!isDesktop || !activeRuntime.canRestart || Boolean(activeOperation) || Boolean(desktopBusyByService[activeService.id])} title={activeRuntime.actionReasons.restart || activeRuntime.controlReason || '确认旧应用退出后再启动新实例'}><RefreshCw size={15} />{activeOperation === 'restart' ? '重启中…' : '重启'}</button>
                  {activeRuntime.canForceQuit && <button className="button danger compact" type="button" onClick={() => setDesktopPrompt({ kind: 'force', serviceId: activeService.id })} disabled={!isDesktop || Boolean(desktopBusyByService[activeService.id])} title={activeRuntime.actionReasons.forceQuit || '强制结束只作用于已核验的主应用实例'}>强制结束</button>}
                  {activeRuntime.canUnlock && !activeRuntime.protectionUnlocked && <button className="icon-button" type="button" aria-label="解锁应用控制" title={activeRuntime.actionReasons.unlock || '临时允许控制当前核验实例'} onClick={() => void performDesktopAction('unlock')} disabled={!isDesktop || Boolean(desktopBusyByService[activeService.id])}><Shield size={16} /></button>}
                </> : activeExternal ? <button className="button danger external-stop-button" type="button" onClick={() => setExternalAction({ kind: 'stop', serviceId: activeService.id })} disabled={!isDesktop || !activeExternalCanStop || Boolean(activeService.external?.sharedHost) || Boolean(activeOperation) || Boolean(externalActionBusy)} title={!isDesktop ? '运行操作请使用桌面应用' : activeService.external?.sharedHost ? '共享宿主始终只读' : activeRuntime.controlReason || '只终止当前外部 PID'}><Square size={14} fill="currentColor" />{windowsExternalTermination ? '终止进程' : '停止外部进程'}</button> : <>
                  <button className="button secondary" type="button" onClick={() => void performOperation('start')} disabled={!isDesktop || !activeRuntime.canStart || !validToStart || isActiveRuntime(activeRuntime.status) || Boolean(activeOperation)} title={startDisabledTitle !== '启动服务' ? startDisabledTitle : activeRuntime.actionReasons.start || '启动服务'}><Power size={16} />{activeOperation === 'start' ? '启动中…' : '启动'}</button>
                  <button className="button secondary" type="button" onClick={() => void performOperation('stop')} disabled={!isDesktop || !canStop || Boolean(activeOperation)} title={!isDesktop ? '运行操作请使用桌面应用' : activeRuntime.actionReasons.stop || activeRuntime.controlReason || '停止当前托管实例'}><Square size={14} fill="currentColor" />{activeOperation === 'stop' ? '停止中…' : activeRuntime.status === 'stopping' ? '重试停止' : '停止'}</button>
                  <button className="button secondary" type="button" onClick={() => void performOperation('restart')} disabled={!isDesktop || !activeRuntime.canRestart || Boolean(activeOperation)} title={!isDesktop ? '运行操作请使用桌面应用' : activeRuntime.actionReasons.restart || activeRuntime.controlReason || '重新启动服务'}><RefreshCw size={16} />{activeOperation === 'restart' ? '重启中…' : '重启'}</button>
                </>}

                <div className="agent-menu-wrap">
                  <div className="agent-launch-group">
                    <button className="button primary agent-launch-main" type="button" disabled={!isDesktop || agents.length === 0 || Boolean(checkingAgentId) || !activeCanCreateSession} onClick={() => selectedAgent ? void launchAgent(selectedAgent) : setAgentMenuOpen((open) => !open)} title={!isDesktop ? '运行操作请使用桌面应用' : !activeCanCreateSession ? '请先补充工作目录' : agents.length === 0 ? '请先配置 AI 工具' : '在独立终端中打开 Agent'}><TerminalSquare size={16} />{checkingAgentId ? '检测中…' : selectedAgent ? `打开 ${selectedAgent.name}` : '打开 Agent'}</button>
                    <button className="button primary agent-launch-caret" type="button" aria-label="选择 Agent" title="选择 Agent" aria-expanded={agentMenuOpen} onClick={() => setAgentMenuOpen((open) => !open)} disabled={!isDesktop || !activeCanCreateSession}><ChevronDown size={15} /></button>
                  </div>
                  {agentMenuOpen && <div className="popover agent-popover" role="menu" aria-label="选择 AI Agent">
                    <div className="popover-heading">在独立终端打开</div>
                    {agents.length === 0 ? <p className="popover-empty">尚未配置 AI 工具。</p> : agents.map((agent) => <button role="menuitem" type="button" key={agent.id} onClick={() => void launchAgent(agent)} disabled={Boolean(checkingAgentId)}><Bot size={15} /><span>{agent.name}</span>{selectedAgentByService[activeService.id] === agent.id && <Check size={14} />}</button>)}
                    <button className="popover-secondary" type="button" onClick={() => { setAgentMenuOpen(false); setDrawer({ kind: 'agents' }); }}><Wrench size={15} /> 管理 AI 工具</button>
                    <button className="popover-secondary" type="button" disabled={!activeCanCreateSession} title={!activeCanCreateSession ? '请先补充工作目录' : ''} onClick={() => { setAgentMenuOpen(false); void openOrdinaryTerminal(); }}><Plus size={15} /> 新建普通终端</button>
                  </div>}
                </div>

                <div className="more-menu-wrap">
                  <button className="icon-button header-more" type="button" aria-label="更多服务操作" title="更多操作" aria-expanded={moreMenuOpen} onClick={() => setMoreMenuOpen((open) => !open)}><MoreHorizontal size={19} /></button>
                  {moreMenuOpen && <div className="popover more-popover" role="menu">
                    <button role="menuitem" type="button" onClick={() => { setDrawer(activeExternal ? { kind: 'external-edit' } : { kind: 'service', mode: 'edit' }); setMoreMenuOpen(false); }}><Pencil size={15} /> {activeExternal ? '编辑外部记录' : '编辑服务'}</button>
                    <button role="menuitem" type="button" className="danger-menu-item" onClick={() => { setDeleteTargetId(activeService.id); setDeleteError(''); setMoreMenuOpen(false); }}><Trash2 size={15} /> 删除服务</button>
                  </div>}
                </div>
              </div>
            </header>

            {isPreview && <div className="preview-banner"><Monitor size={15} /><span>界面预览，运行操作请使用桌面应用</span><span className="preview-banner-spacer" /><small>本地预览数据独立保存</small></div>}
            {activeRuntime.status === 'failed' && !activeExternal && activeService.kind !== 'desktopApp' && <div className="runtime-error-banner" role="status">
              <CircleAlert size={17} /><div><strong>服务未能启动 / 异常退出{activeRuntime.exitCode !== null ? `（退出码 ${activeRuntime.exitCode}）` : ''}</strong><p>{activeRuntime.error || serviceErrors[activeService.id] || (invalidPathByService[activeService.id] ? '工作目录或环境目录不可用，请编辑服务后重新选择目录。' : '服务进程已结束，请查看服务输出或检查启动配置。')}</p></div>
              <button className="button secondary compact" type="button" onClick={focusServiceOutput}><TerminalSquare size={15} />查看服务输出</button>
              <button className="button secondary compact" type="button" onClick={() => setDrawer({ kind: 'service', mode: 'edit' })}>编辑服务</button>
              <button className="button primary compact" type="button" disabled={!isDesktop || !validToStart || Boolean(activeOperation)} onClick={() => void performOperation('start')}>重试启动</button>
            </div>}
            {(serviceErrors[activeService.id] || invalidPathByService[activeService.id]) && activeRuntime.status !== 'failed' && <div className="runtime-notice" role="alert"><CircleAlert size={16} /><span>{serviceErrors[activeService.id] || '目录配置不可用，请编辑服务重新选择目录。'}</span><button className="icon-button small" aria-label="关闭提示" onClick={() => setServiceErrors((previous) => ({ ...previous, [activeService.id]: '' }))}><X size={14} /></button></div>}
            {agentFailure?.serviceId === activeService.id && <div className="runtime-error-banner agent-failure-banner" role="alert"><CircleAlert size={17} /><div><strong>无法启动所选 Agent</strong><p>{agentFailure.message || '未找到可执行程序，请安装对应 CLI，或在 AI 工具设置中指定程序路径。'}</p></div><button className="button secondary compact" type="button" onClick={() => setDrawer({ kind: 'agents' })}>编辑工具配置</button><button className="button secondary compact" type="button" disabled={Boolean(checkingAgentId)} onClick={() => void recheckAgent(agentFailure.agentId)}>{checkingAgentId === agentFailure.agentId ? '检测中…' : '重新检测'}</button><button className="icon-button small" type="button" aria-label="关闭 Agent 提示" onClick={() => setAgentFailure(null)}><X size={15} /></button></div>}

            <nav className="main-tabs" aria-label="服务内容">
              <button type="button" className={activeTab === 'overview' ? 'active' : ''} onClick={() => setActiveTab('overview')}>概览</button>
              <button type="button" className={activeTab === 'environment' ? 'active' : ''} onClick={() => setActiveTab('environment')}>环境</button>
              <button type="button" className={activeTab === 'documents' ? 'active' : ''} onClick={() => setActiveTab('documents')}>文档</button>
            </nav>

            <div className="service-workspace">
              <div className={`service-view ${activeTab === 'documents' ? 'document-view' : ''}`}>
                {activeTab === 'overview' && (activeExternal ? <ExternalServiceView
                  service={activeService}
                  runtime={activeRuntime}
                  isDesktop={isDesktop}
                  onEdit={() => setDrawer({ kind: 'external-edit' })}
                  onCopy={(value, label) => void copyText(value, label)}
                  onOpenDirectory={() => void openServiceDirectory(activeService.id, 'work')}
                  onUnlock={() => setExternalAction({ kind: 'unlock', serviceId: activeService.id })}
                  onConvert={() => setExternalAction({ kind: 'convert', serviceId: activeService.id })}
                /> : activeService.kind === 'desktopApp' ? <DesktopApplicationView
                  service={activeService}
                  runtime={activeRuntime}
                  snapshot={applicationSnapshot}
                  onEdit={() => setDrawer({ kind: 'service', mode: 'edit' })}
                  onCopy={(value, label) => void copyText(value, label)}
                  onOpenDirectory={() => void openServiceDirectory(activeService.id, 'work')}
                /> : <OverviewView
                  service={activeService}
                  runtime={activeRuntime}
                  onEdit={() => setDrawer({ kind: 'service', mode: 'edit' })}
                  onCopy={(value, label) => void copyText(value, label)}
                  onOpenUrl={(value) => void api.openExternalUrl(value).catch((reason) => setGlobalError(`无法打开访问地址：${errorText(reason)}`))}
                  onOpenDirectory={(kind) => void openServiceDirectory(activeService.id, kind)}
                  onShowOutput={focusServiceOutput}
                />)}
                {activeTab === 'environment' && <EnvironmentView service={activeService} onEdit={() => setDrawer(activeExternal ? { kind: 'external-edit' } : { kind: 'service', mode: 'edit' })} onCopy={(value, label) => void copyText(value, label)} />}
                {activeTab === 'documents' && <>
                  {!documentsLoaded[activeService.id] && <div className="document-loading-note"><span className="spinner" />正在载入文档…</div>}
                  {documentLoadErrors[activeService.id] && <div className="runtime-notice" role="alert"><CircleAlert size={16} /><span>文档载入失败：{documentLoadErrors[activeService.id]}</span><button className="button secondary compact" type="button" onClick={() => setDocumentsReloadToken((current) => current + 1)}>重试</button></div>}
                  <DocumentsPanel
                    serviceId={activeService.id}
                    serviceName={activeService.name}
                    drafts={documentDrafts}
                    setDrafts={updateDocumentDrafts}
                    activeKey={activeDocumentByService[activeService.id] ?? ''}
                    setActiveKey={(key) => setActiveDocumentByService((previous) => ({ ...previous, [activeService.id]: key }))}
                    onSave={saveDocument}
                    onDelete={async (draft) => { if (!draft.isNew && draft.document.id) await api.deleteDocument(draft.document.id); }}
                    chooseExport={api.chooseExport}
                    exportDocument={api.exportDocument}
                  />
                </>}
              </div>
              <TerminalPanel serviceId={activeService.id} sessions={activeSessions} appearance={preferences.terminal} systemTheme={systemTheme} onOpenAppearance={() => setDrawer({ kind: 'settings' })} onCreate={() => void openOrdinaryTerminal()} onClose={(sessionId) => void closeTerminal(sessionId)} createDisabled={!isDesktop || !activeCanCreateSession} createDisabledReason={!isDesktop ? '终端操作请使用桌面应用' : !activeCanCreateSession ? '请先补充工作目录' : undefined} initiallyCollapsed={activeService.kind === 'desktopApp' || activeService.kind === 'staticSite'} />
            </div>
          </> : <>
            {isPreview && <div className="preview-banner"><Monitor size={15} /><span>界面预览，运行操作请使用桌面应用</span><span className="preview-banner-spacer" /><small>本地预览数据独立保存</small></div>}
            <div className="no-service-workspace">
              <div className="no-service-card"><div className="empty-symbol"><LayoutGrid size={23} /></div><h1>{services.length ? '选择一个服务' : '还没有服务'}</h1><p>{services.length ? '从左侧服务列表选择要管理的本地服务。' : '添加本地服务后，可在这里管理目录、运行状态、终端和文档。'}</p><button className="button primary" type="button" onClick={() => setDrawer({ kind: 'service', mode: 'create' })}><Plus size={16} />新增服务</button></div>
            </div>
          </>}
        </main>
      </div>

      <footer className="app-statusbar"><div><Folder size={14} /><span>{activePage === 'content' ? '内容库' : activePage === 'discovery' ? '发现服务' : '本地工作区'}</span>{services.length > 0 && <span className="statusbar-count">{services.length} 个服务</span>}</div><div><span>UTF-8</span><i />{activeService?.shell || '系统默认终端'}</div></footer>

      {globalError && <div className="global-alert" role="alert"><CircleAlert size={16} /><span>{globalError}</span><button type="button" aria-label="关闭错误提示" onClick={() => setGlobalError('')}><X size={14} /></button></div>}
      {toast && <div className="toast-message" role="status"><Check size={15} />{toast}</div>}

      {drawer?.kind === 'service' && <ServiceDrawer
        key={`${drawer.mode}:${drawer.mode === 'edit' ? activeService?.id ?? '' : 'new'}`}
        mode={drawer.mode}
        service={drawer.mode === 'edit' ? activeService ?? undefined : undefined}
        runtime={drawer.mode === 'edit' ? activeRuntime ?? undefined : undefined}
        onClose={() => setDrawer(null)}
        onSave={saveService}
        onDirtyChange={reportServiceDrawerDirty}
        chooseDirectory={api.chooseDirectory}
        chooseApplicationPath={api.chooseApplicationPath}
        platformInfo={api.platformInfo}
        inspectFrontendProject={api.inspectFrontendProject}
        inspectApplication={api.inspectApplication}
      />}
      {drawer?.kind === 'external-edit' && activeService?.source === 'external' && <ExternalServiceDrawer
        key={`external:${activeService.id}`}
        service={activeService}
        onClose={() => setDrawer(null)}
        onSave={saveService}
        onDirtyChange={reportServiceDrawerDirty}
        chooseDirectory={api.chooseDirectory}
      />}
      {drawer?.kind === 'attach-external' && <AttachExternalDrawer
        key={`${drawer.process.instance.identity.pid}:${drawer.process.instance.identity.createdAt}`}
        process={drawer.process}
        existingServices={services.filter((service) => service.source === 'external' && service.kind !== 'desktopApp')}
        onClose={() => setDrawer(null)}
        onAttach={attachExternal}
        onDirtyChange={reportServiceDrawerDirty}
        chooseDirectory={api.chooseDirectory}
      />}
      {drawer?.kind === 'attach-application' && <AttachApplicationDrawer
        key={`${drawer.application.instance.identity.pid}:${drawer.application.instance.identity.createdAt}`}
        application={drawer.application}
        existingServices={services.filter((service) => service.kind === 'desktopApp')}
        onClose={() => setDrawer(null)}
        onAttach={attachApplication}
        onDirtyChange={reportServiceDrawerDirty}
        chooseDirectory={api.chooseDirectory}
      />}
      {drawer?.kind === 'agents' && <AgentDrawer
        agents={agents}
        services={services}
        initialServiceId={activeService?.id}
        onClose={() => setDrawer(null)}
        onDirtyChange={reportAgentDrawerDirty}
        onSave={async (agent) => {
          const saved = await api.saveAgent(agent);
          setAgents((previous) => previous.some((item) => item.id === saved.id) ? previous.map((item) => item.id === saved.id ? saved : item) : [...previous, saved]);
          return saved;
        }}
        onDelete={async (agentId) => {
          await api.deleteAgent(agentId);
          setAgents((previous) => previous.filter((agent) => agent.id !== agentId));
          setSelectedAgentByService((previous) => Object.fromEntries(Object.entries(previous).filter(([, selectedId]) => selectedId !== agentId)));
        }}
        onCheck={api.checkAgent}
      />}
      {drawer?.kind === 'settings' && <SettingsDrawer
        initialTab={drawer.tab}
        preferences={preferences}
        systemTheme={systemTheme}
        onSave={savePreferences}
        onDirtyChange={reportSettingsDrawerDirty}
        onClose={() => setDrawer(null)}
      />}

      {externalAction && externalActionService && externalActionRuntime && <Modal
        title={externalAction.kind === 'stop' ? '终止外部进程？' : externalAction.kind === 'unlock' ? '临时解锁当前实例？' : '转换为平台托管并启动？'}
        tone={externalAction.kind === 'unlock' ? 'normal' : 'danger'}
        onCancel={() => !externalActionBusy && setExternalAction(null)}
        actions={[
          { label: '取消', onClick: () => setExternalAction(null), variant: 'secondary', disabled: externalActionBusy, autoFocus: true },
          { label: externalActionBusy ? '处理中…' : externalAction.kind === 'stop' ? windowsExternalTermination ? '确认终止' : '确认停止' : externalAction.kind === 'unlock' ? '临时解锁' : '转换并启动', onClick: () => void confirmExternalAction(), variant: externalAction.kind === 'unlock' ? 'primary' : 'danger', disabled: externalActionBusy || !isDesktop || !externalActionAllowed },
        ]}
      >
        {externalAction.kind === 'stop' ? <>
          <p>将停止“{externalActionService.name}”的外部进程 PID {externalActionService.external?.identity.pid ?? externalActionRuntime.pid ?? '未知'}。{windowsExternalTermination ? 'Windows 会终止选中的进程。' : '先请求退出，五秒后仍未退出会强制终止。'}</p>
          <p>操作只针对这个 PID，不会终止父进程、子进程或整个进程组。</p>
          {externalActionPorts}
          {!externalActionAllowed && <p className="modal-error" role="alert">{externalActionRuntime.controlReason || '当前实例不允许停止。'}</p>}
        </> : externalAction.kind === 'unlock' ? <>
          <p>将仅对 PID {externalActionService.external?.identity.pid ?? externalActionRuntime.pid ?? '未知'} 的当前进程实例临时解除保护，供后续受控操作使用。</p>
          <p>解锁不保存为长期设置；进程实例变化后需重新确认。共享宿主永远不能解锁。</p>
          {!externalActionAllowed && <p className="modal-error" role="alert">{externalActionRuntime.controlReason || '当前实例不允许解锁。'}</p>}
        </> : <>
          <p>平台会先校验“{externalActionService.name}”的工作目录、环境、命令与预期端口，再终止外部 PID {externalActionService.external?.identity.pid ?? externalActionRuntime.pid ?? '未知'}，并启动新的平台托管进程。</p>
          <p>如果预期端口仍被占用，转换会取消；已终止的外部进程不会自动恢复。操作只针对这个 PID，不会终止父进程、子进程或整个进程组。</p>
          {externalActionPorts}
          {!externalActionAllowed && <p className="modal-error" role="alert">{externalActionRuntime.controlReason || '请补齐有效工作目录和启动命令，并确认当前实例允许转换。'}</p>}
        </>}
      </Modal>}

      {desktopPrompt && services.find((service) => service.id === desktopPrompt.serviceId) && <Modal
        title={desktopPrompt.kind === 'restart' ? '重启桌面应用？' : '应用仍在运行，强制结束？'}
        tone="danger"
        onCancel={() => setDesktopPrompt(null)}
        actions={desktopPrompt.kind === 'restart' ? [
          { label: '取消', onClick: () => setDesktopPrompt(null), variant: 'secondary', autoFocus: true },
          { label: '确认重启', onClick: () => void confirmDesktopAction(), variant: 'primary' },
        ] : [
          { label: '继续等待', onClick: () => setDesktopPrompt(null), variant: 'secondary', autoFocus: true },
          { label: desktopBusyByService[desktopPrompt.serviceId] === 'forceQuit' ? '正在结束…' : '确认强制结束', onClick: () => void confirmDesktopAction(), variant: 'danger', disabled: Boolean(desktopBusyByService[desktopPrompt.serviceId]) },
        ]}
      >
        {desktopPrompt.kind === 'restart' ? <><p>服务中心会先请求当前实例退出。只有核验到旧实例已经退出后，才会启动新实例。</p><p>如果应用拒绝退出、仍在显示未保存提示或实例状态无法确认，重启会停止并保留当前跟踪。</p></> : <><p>{desktopPrompt.afterRequest ? '已发送正常退出请求，等待后应用仍在运行。' : '当前应用仍在运行。'}</p><p>强制结束可能丢失未保存内容，只作用于已核验的主应用实例；未关联的辅助进程不会被批量结束。</p>{!runtimeRef.current[desktopPrompt.serviceId]?.canForceQuit && <p className="modal-error">{runtimeRef.current[desktopPrompt.serviceId]?.actionReasons.forceQuit || runtimeRef.current[desktopPrompt.serviceId]?.controlReason || '当前实例无法安全核验，暂不可强制结束。'}</p>}</>}
      </Modal>}

      {launchCandidates && <Modal
        title="选择要关联的运行实例"
        onCancel={() => !candidateBusy && setLaunchCandidates(null)}
        actions={[
          { label: '稍后关联', onClick: () => setLaunchCandidates(null), variant: 'secondary', disabled: candidateBusy, autoFocus: true },
          { label: candidateBusy ? '关联中…' : '关联所选实例', onClick: () => selectedLaunchApplication && void attachLaunchCandidate(selectedLaunchApplication), variant: 'primary', disabled: candidateBusy || !selectedLaunchApplication },
        ]}
      >
        <p>同一路径发现了多个运行实例。选择一个实例后，服务中心只跟踪这个已核验的实例。</p>
        <div className="candidate-application-list">{launchCandidates.candidates.map((candidate) => {
          const candidateKey = `${candidate.instance.identity.pid}:${candidate.instance.identity.createdAt}`;
          return <button key={candidateKey} className={selectedLaunchCandidate === candidateKey ? 'selected' : ''} type="button" role="radio" aria-checked={selectedLaunchCandidate === candidateKey} onClick={() => setSelectedLaunchCandidate(candidateKey)} disabled={candidateBusy}><strong>{candidate.instance.name || '桌面应用'} · PID {candidate.instance.identity.pid}</strong><code>{candidate.path}</code><small>{candidate.windowCount} 个窗口 · {candidate.members.length} 个核验进程</small></button>;
        })}</div>
      </Modal>}

      {deleteTargetId && deleteService && <Modal title={deleteService.kind === 'desktopApp' ? deleteNeedsStop ? '解除关联并关闭终端？' : '解除应用关联？' : deleteService.source === 'external' ? deleteNeedsStop ? '删除外部记录并关闭终端？' : '删除外部服务记录？' : deleteNeedsStop ? '停止并删除此服务？' : '删除此服务？'} tone="danger" onCancel={() => !deleteBusy && setDeleteTargetId('')} actions={[
        { label: '取消', onClick: () => { setDeleteTargetId(''); setDeleteError(''); }, variant: 'secondary', disabled: deleteBusy, autoFocus: true },
        { label: deleteBusy ? '处理中…' : deleteService.kind === 'desktopApp' || deleteService.source === 'external' ? deleteNeedsStop ? '解除关联并关闭终端' : '解除关联' : deleteNeedsStop ? '停止并删除' : '删除服务', onClick: () => void removeService(), variant: 'danger', disabled: deleteBusy || Boolean(operationByService[deleteTargetId]) || Boolean(desktopBusyByService[deleteTargetId]) },
      ]}>
        {deleteService.kind === 'desktopApp' ? <>
          <p>解除“{deleteService.name}”的应用关联只会删除服务中心中的记录，不会退出正在运行的应用。</p>
          {deleteNeedsStop && <p>另外会关闭 {deleteSessions.length} 个由服务中心创建且仍在运行的独立终端或 Agent 会话。</p>}
          <p>备注和文档也会删除；应用文件与工作目录不会删除。</p>
        </> : deleteService.source === 'external' ? <>
          <p>删除“{deleteService.name}”只会移除应用里的外部进程记录，不会终止 PID {deleteService.external?.identity.pid ?? '未知'}；外部进程会继续运行。</p>
          {deleteNeedsStop && <p>另外会关闭 {deleteSessions.length} 个由应用创建且仍在运行的独立终端或 Agent 会话。</p>}
          <p>备注、服务配置和文档记录也会删除；工作目录与环境目录不会删除。</p>
        </> : deleteNeedsStop ? <><p>删除“{deleteService.name}”前会停止服务进程并结束 {deleteSessions.length} 个运行中的独立终端或 Agent 会话。</p><p>服务输出、备注和文档记录也会删除；工作目录与环境目录不会删除。</p></> : <p>将删除“{deleteService.name}”的应用记录、备注和文档，不会删除工作目录或环境目录。</p>}
        {deleteError && <p className="modal-error" role="alert">{deleteError}</p>}
      </Modal>}

      {quitPrompt && <Modal
        title="有未保存的修改"
        onCancel={() => !quitBusy && setQuitPrompt(false)}
        actions={hasDirtyConfig ? [
          { label: '返回编辑', onClick: () => setQuitPrompt(false), variant: 'secondary', disabled: quitBusy, autoFocus: true },
          ...(dirtyCount > 0 ? [{ label: quitBusy ? '保存中…' : '保存文档并返回编辑', onClick: () => void saveDocumentsAndReturnToEdit(), variant: 'primary' as const, disabled: quitBusy }] : []),
          { label: quitBusy ? '退出中…' : '放弃修改并退出', onClick: () => void discardAndExit(), variant: 'danger', disabled: quitBusy },
        ] : [
          { label: '取消', onClick: () => setQuitPrompt(false), variant: 'secondary', disabled: quitBusy },
          { label: quitBusy ? '退出中…' : '放弃文档并退出', onClick: () => void discardAndExit(), variant: 'danger', disabled: quitBusy },
          { label: quitBusy ? '保存中…' : '保存文档并退出', onClick: () => void saveQuitAndExit(), variant: 'primary', disabled: quitBusy, autoFocus: true },
        ]}
      >
        {hasDirtyConfig ? <>
          {dirtyCount > 0 && <p>有 {dirtyCount} 篇文档尚未保存。</p>}
          <p>{dirtyConfigLabels.join('、')}尚未保存。请返回抽屉手动检查并保存，或明确放弃这些配置后退出。保存文档并返回编辑只会保存文档，应用会保持打开。退出应用不会终止外部进程，它们会继续运行。</p>
        </> : <p>有 {dirtyCount} 篇文档尚未保存。保存文档并退出会保留修改；放弃文档并退出将关闭应用并丢弃这些草稿。退出应用不会终止外部进程，它们会继续运行。</p>}
      </Modal>}
    </div>
  );
}

interface OverviewViewProps {
  service: ServiceConfig;
  runtime: ServiceRuntime;
  onEdit: () => void;
  onCopy: (value: string, label: string) => void;
  onOpenUrl: (value: string) => void;
  onOpenDirectory: (kind: 'work' | 'environment') => void;
  onShowOutput: () => void;
}

function OverviewView({ service, runtime, onEdit, onCopy, onOpenUrl, onOpenDirectory, onShowOutput }: OverviewViewProps) {
  const frontendLaunch = service.launch.type === 'frontend' ? service.launch : null;
  const configuredUrl = frontendLaunch?.url.trim() ?? '';
  const webUrls = [...new Set([...runtime.webUrls, configuredUrl].filter(Boolean))];
  const expectedSet = new Set(service.expectedPorts);
  const matchedExpected = runtime.ports.some((port) => expectedSet.has(port.port));
  const conflictExpected = runtime.conflicts.filter((port) => expectedSet.has(port.port));
  const portMessage = runtime.portError
    ? `暂时无法获取端口信息。${runtime.portError}`
    : conflictExpected.length
      ? `预期端口 ${[...new Set(conflictExpected.map((port) => port.port))].join('、')} 已被其他进程占用。`
      : service.expectedPorts.length === 0
        ? '未设置预期端口。'
        : runtime.status === 'running' && !matchedExpected
          ? '进程正在运行，尚未检测到预期端口。'
          : runtime.status === 'running'
            ? '预期端口已检测到；端口监听状态与进程运行状态分别检测。'
            : '预期端口会在启动后检测。';

  const DirectoryRow = ({ label, value, kind }: { label: string; value: string; kind: 'work' | 'environment' }) => (
    <div className="directory-row"><span className="directory-label">{label}</span><code title={value || '未设置'}>{value || '未设置'}</code><div className="directory-actions">
      {value && <button className="icon-button small" type="button" aria-label={`复制${label}`} title="复制路径" onClick={() => onCopy(value, label)}><Copy size={15} /></button>}
      {value && <button className="icon-button small" type="button" aria-label={`打开${label}`} title="在系统中打开" onClick={() => onOpenDirectory(kind)}><FolderOpen size={16} /></button>}
    </div></div>
  );

  return <div className="overview-grid">
    <section className="surface-card directory-card">
      <div className="card-heading"><div><h2>目录与启动</h2><p>当前服务使用的本地路径与启动命令</p></div><button className="text-button" type="button" onClick={onEdit}><Pencil size={14} />编辑</button></div>
      <div className="directory-list">
        <DirectoryRow label="工作目录" value={service.workingDir} kind="work" />
        {service.environmentDir && <DirectoryRow label="环境目录" value={service.environmentDir} kind="environment" />}
        {service.kind === 'command' && <div className="directory-row command-row"><span className="directory-label">启动命令</span><code title={service.command}>{service.command || '未设置启动命令'}</code><div className="directory-actions">{service.command && <button className="icon-button small" type="button" aria-label="复制启动命令" title="复制启动命令" onClick={() => onCopy(service.command, '启动命令')}><Copy size={15} /></button>}</div></div>}
        {frontendLaunch && <>
          <div className="directory-row"><span className="directory-label">包管理器</span><code>{frontendLaunch.packageManager || '未选择'}</code><div className="directory-actions" /></div>
          <div className="directory-row command-row"><span className="directory-label">启动脚本</span><code>{frontendLaunch.script || '未选择'}</code><div className="directory-actions">{frontendLaunch.script && <button className="icon-button small" type="button" aria-label="复制脚本名称" title="复制脚本名称" onClick={() => onCopy(frontendLaunch.script, '启动脚本')}><Copy size={15} /></button>}</div></div>
        </>}
        {service.launch.type === 'staticSite' && <div className="directory-row"><span className="directory-label">预览端口</span><code>{service.launch.port === 0 ? '自动分配' : service.launch.port}</code><div className="directory-actions" /></div>}
      </div>
      {!service.workingDir && <div className="inline-warning"><CircleAlert size={15} /><span>工作目录不可用，请重新选择目录。</span><button className="text-button" type="button" onClick={onEdit}>编辑服务</button></div>}
    </section>

    <div className="overview-side-stack">
      <section className="surface-card ports-card">
        <div className="card-heading"><div><h2>端口</h2><p>预期检查目标与实际监听分别展示</p></div></div>
        <div className="ports-table" role="table" aria-label="服务端口">
          <div className="port-row" role="row"><span role="cell">预期端口</span><span role="cell">{service.expectedPorts.length ? service.expectedPorts.join('、') : '未设置'}</span></div>
          {runtime.ports.map((port, index) => <div className="port-row actual" role="row" key={`${port.protocol}-${port.address}-${port.port}-${index}`}><span role="cell">实际{port.protocol === 'UDP' ? '绑定' : '监听'}</span><span role="cell"><code>{formatAddressPort(port.address, port.port)}</code><small className={`protocol-badge ${port.protocol.toLowerCase()}`}>{port.protocol}</small>{port.pid && <small>PID {port.pid}</small>}</span></div>)}
          {runtime.conflicts.map((port, index) => <div className="port-row conflict" role="row" key={`conflict-${port.protocol}-${port.port}-${index}`}><span role="cell">端口冲突</span><span role="cell"><code>{formatAddressPort(port.address, port.port)}</code><small>{port.pid ? `其他进程 PID ${port.pid}` : '其他进程占用'}</small></span></div>)}
        </div>
        <div className={`port-status-note ${runtime.portError || conflictExpected.length ? 'is-warning' : ''}`} role="status"><span className={`status-dot ${runtime.portError || conflictExpected.length ? 'failed' : runtime.status === 'running' && matchedExpected ? 'running' : 'stopped'}`} /><span>{portMessage}</span></div>
        {(runtime.portError || conflictExpected.length || (runtime.status === 'running' && !matchedExpected && service.expectedPorts.length > 0)) && <button className="text-button port-action" type="button" onClick={conflictExpected.length || runtime.portError ? onShowOutput : onEdit}>{conflictExpected.length || runtime.portError ? <><TerminalSquare size={14} />查看服务输出</> : <><Pencil size={14} />编辑服务</>}</button>}
      </section>

      <section className="surface-card notes-card">
        <div className="card-heading"><div><h2>备注</h2></div><button className="text-button" type="button" onClick={onEdit}><Pencil size={14} />编辑</button></div>
        <p className={service.notes ? 'notes-content' : 'muted-copy'}>{service.notes || '尚无备注。可在服务配置中记录启动前的注意事项。'}</p>
      </section>
    </div>
    {(service.kind === 'frontend' || service.kind === 'staticSite') && <section className="surface-card web-address-card">
      <div className="card-heading"><div><h2>访问地址</h2><p>可编辑地址与运行时实际监听地址分别保留</p></div><span>{webUrls.length} 项</span></div>
      {webUrls.length ? <div className="web-address-list">{webUrls.map((url, index) => <div className="web-address-row" key={`${url}-${index}`}><code title={url}>{url}</code><div><button className="icon-button small" type="button" aria-label="复制访问地址" title="复制地址" onClick={() => onCopy(url, '访问地址')}><Copy size={15} /></button><button className="button secondary compact" type="button" onClick={() => onOpenUrl(url)}><ExternalLink size={14} />打开页面</button></div></div>)}</div> : <div className="empty-inline"><Activity size={17} /><span>{frontendLaunch?.url ? '地址已配置；启动后系统会显示实际候选地址。' : '启动后会根据实际监听端口生成访问地址。'}</span><button className="text-button" type="button" onClick={onEdit}>配置</button></div>}
    </section>}
  </div>;
}

function DesktopApplicationView({ service, runtime, snapshot, onEdit, onCopy, onOpenDirectory }: { service: ServiceConfig; runtime: ServiceRuntime; snapshot: ApplicationSnapshot | null; onEdit: () => void; onCopy: (value: string, label: string) => void; onOpenDirectory: () => void }) {
  const desktopLaunch = service.launch.type === 'desktopApp' ? service.launch : null;
  const targetPath = desktopLaunch?.path ?? '';
  // runtime.runId 保存 pid 与完整原生创建时间；仅凭同一路径无法区分并行运行的两个实例。
  const runtimeIdentity = (() => {
    if (runtime.pid === null || !runtime.runId) return null;
    const prefix = `application:${runtime.pid}:`;
    if (!runtime.runId.startsWith(prefix)) return null;
    const createdAt = runtime.runId.slice(prefix.length);
    return createdAt ? { pid: runtime.pid, createdAt } : null;
  })();
  const mayHaveCurrentBinding = runtime.status === 'running' || runtime.status === 'starting'
    || runtime.status === 'stopping' || runtime.status === 'unknown';
  const application = desktopLaunch && mayHaveCurrentBinding
    ? snapshot?.applications.find((item) => {
      const identity = item.instance.identity;
      const matchesRuntimeIdentity = runtimeIdentity !== null
        && identity.pid === runtimeIdentity.pid
        && identity.createdAt === runtimeIdentity.createdAt;
      // managedServiceId 是后端的显式关联；无关联记录时必须同时匹配 PID 和创建标识。
      // 同路径应用可能有多个实例，因此路径相等本身不构成当前实例的证据。
      return (item.managedServiceId === service.id && (!runtimeIdentity || matchesRuntimeIdentity))
        || (sameApplicationPath(item.path, desktopLaunch.path) && matchesRuntimeIdentity);
    })
    : undefined;
  // stopped/failed 状态下不能把旧外部记录身份当作当前进程；活动身份只来自本次 runtime
  // 或已通过服务关联/完整进程身份核验的应用快照。
  const currentIdentity = mayHaveCurrentBinding
    ? runtimeIdentity ?? application?.instance.identity ?? null
    : null;
  // 将后端能力拒绝原因放到详情中直接展示；对运行中的应用提供平台差异的兜底说明，
  // 让没有窗口、系统拒绝前台切换或无法核验身份等情况不必依赖悬停按钮才能理解。
  const unavailableCapabilities = [
    { key: 'focus', label: '显示应用', available: runtime.canFocus, applies: runtime.status === 'running', fallback: '应用没有可显示的窗口，或系统未允许切换到前台。' },
    { key: 'requestQuit', label: '请求退出', available: runtime.canRequestQuit, applies: runtime.status === 'running', fallback: '系统当前不允许向此应用发送正常退出请求。' },
    { key: 'restart', label: '重启', available: runtime.canRestart, applies: runtime.status === 'running', fallback: '无法确认当前应用已安全退出，暂不能重启。' },
    { key: 'forceQuit', label: '强制结束', available: runtime.canForceQuit, applies: runtime.status === 'running', fallback: '无法安全核验需要结束的主应用实例。' },
  ].flatMap(({ key, label, available, applies, fallback }) => {
    if (available) return [];
    const reason = runtime.actionReasons[key]?.trim() || (applies ? fallback : '');
    return reason ? [`${label}：${reason}`] : [];
  });
  const uniqueCapabilityReasons = [...new Set(unavailableCapabilities)];
  return <div className="desktop-application-view">
    <div className="overview-grid">
      <section className="surface-card directory-card"><div className="card-heading"><div><h2>应用身份</h2><p>使用目标路径和系统实例身份进行关联</p></div><button className="text-button" type="button" onClick={onEdit}><Pencil size={14} />编辑</button></div>
        <div className="directory-list"><div className="directory-row"><span className="directory-label">应用路径</span><code title={targetPath}>{targetPath || '未配置'}</code><div className="directory-actions">{targetPath && <button className="icon-button small" type="button" aria-label="复制应用路径" title="复制路径" onClick={() => onCopy(targetPath, '应用路径')}><Copy size={15} /></button>}</div></div>
          {desktopLaunch?.startDir && <div className="directory-row"><span className="directory-label">Windows 启动目录</span><code title={desktopLaunch.startDir}>{desktopLaunch.startDir}</code><div className="directory-actions"><button className="icon-button small" type="button" aria-label="复制启动目录" title="复制目录" onClick={() => onCopy(desktopLaunch.startDir, '启动目录')}><Copy size={15} /></button></div></div>}
          <div className="directory-row"><span className="directory-label">工作目录</span><code title={service.workingDir || '未设置'}>{service.workingDir || '未设置'}</code><div className="directory-actions">{service.workingDir && <><button className="icon-button small" type="button" aria-label="复制工作目录" title="复制路径" onClick={() => onCopy(service.workingDir, '工作目录')}><Copy size={15} /></button><button className="icon-button small" type="button" aria-label="打开工作目录" title="在系统中打开" onClick={onOpenDirectory}><FolderOpen size={15} /></button></>}</div></div>
          {desktopLaunch && desktopLaunch.args.length > 0 && <div className="directory-row command-row"><span className="directory-label">启动参数</span><code>{desktopLaunch.args.join(' · ')}</code><div className="directory-actions"><button className="icon-button small" type="button" aria-label="复制应用参数" title="复制参数" onClick={() => onCopy(desktopLaunch.args.join('\n'), '启动参数')}><Copy size={15} /></button></div></div>}
        </div>
        {application && <div className="application-member-summary"><strong>{application.windowCount} 个窗口 · {application.members.length} 个核验进程</strong><div>{application.members.map((member) => <span key={`${member.pid}:${member.createdAt}`}>PID {member.pid}</span>)}</div></div>}
        {service.workingDir.trim() === '' && <div className="inline-warning"><CircleAlert size={15} /><span>尚未设置独立终端和 Agent 使用的工作目录。</span><button className="text-button" type="button" onClick={onEdit}>补充目录</button></div>}
      </section>
      <div className="overview-side-stack"><section className="surface-card desktop-state-card"><div className="card-heading"><div><h2>实例状态</h2><p>当前关联的应用实例与监听端口等信息会显示在此处</p></div><AppWindow size={17} /></div><div className="desktop-instance-meta"><span><small>PID</small><code>{currentIdentity?.pid ?? '未关联'}</code></span><span><small>运行状态</small><strong>{statusText[runtime.status]}</strong></span><span><small>创建标识</small><code>{currentIdentity?.createdAt ?? '启动或关联后显示'}</code></span>{application?.bundleId && <span className="full"><small>Bundle ID</small><code>{application.bundleId}</code></span>}{runtime.ports.length > 0 && <span className="full"><small>监听端口</small><code>{runtime.ports.map((port) => formatAddressPort(port.address, port.port)).join(' · ')}</code></span>}</div>
        {(runtime.controlReason || runtime.error) && <div className="external-runtime-note"><CircleAlert size={14} /><span>{runtime.error || runtime.controlReason}</span></div>}
        {uniqueCapabilityReasons.length > 0 && <ul className="desktop-capability-reasons" aria-label="当前不可用的应用操作">{uniqueCapabilityReasons.map((reason) => <li key={reason}><CircleAlert size={13} /><span>{reason}</span></li>)}</ul>}
        {runtime.stale && <div className="external-runtime-note stale"><CircleAlert size={14} /><span>应用状态快照已过期，保留最近一次核验结果。</span></div>}
      </section>
      <section className="surface-card notes-card"><div className="card-heading"><div><h2>备注</h2></div><button className="text-button" type="button" onClick={onEdit}><Pencil size={14} />编辑</button></div><p className={service.notes ? 'notes-content' : 'muted-copy'}>{service.notes || '尚无备注。'}</p></section></div>
    </div>
    <section className="surface-card application-log-card"><div className="card-heading"><div><h2>应用操作记录</h2><p>展示最近的启动、关联和退出结果</p></div><span>{runtime.messages.length} 条</span></div>
      {runtime.messages.length ? <ol>{runtime.messages.slice(-30).map((message, index) => <li key={`${index}-${message}`}>{message}</li>)}</ol> : <div className="empty-inline"><Activity size={17} /><span>尚无应用操作记录；实例操作结果会显示在这里。</span></div>}
    </section>
  </div>;
}

function EnvironmentView({ service, onEdit, onCopy }: { service: ServiceConfig; onEdit: () => void; onCopy: (value: string, label: string) => void }) {
  const environmentName = service.environmentType === 'system' ? '系统环境' : service.environmentType === 'venv' ? 'Python 虚拟环境' : '自定义环境';
  return <div className="environment-view">
    <div className="view-title-row"><div><h2>环境配置</h2><p>查看并编辑此服务使用的环境与程序搜索路径。</p></div><button className="button secondary" type="button" onClick={onEdit}><Pencil size={15} />编辑环境</button></div>
    {!service.environmentDir && service.environmentType !== 'system' && <div className="runtime-notice environment-warning"><CircleAlert size={16} /><span>环境目录不可用，请检查路径或选择其他环境。</span><button className="text-button" type="button" onClick={onEdit}>编辑服务</button></div>}
    <section className="surface-card environment-card">
      <div className="environment-info-grid">
        <div className="environment-info-block"><span>环境类型</span><strong><Cpu size={16} />{environmentName}</strong></div>
        <div className="environment-info-block"><span>环境目录</span><div className="environment-value"><code title={service.environmentDir}>{service.environmentDir || '未设置'}</code>{service.environmentDir && <button className="icon-button small" aria-label="复制环境目录" title="复制环境目录" onClick={() => onCopy(service.environmentDir, '环境目录')}><Copy size={15} /></button>}</div></div>
        <div className="environment-info-block full"><span>自定义可执行文件目录</span>{service.executableDirs.length ? <div className="path-chip-list">{service.executableDirs.map((path) => <button type="button" key={path} className="path-chip" title={`复制 ${path}`} onClick={() => onCopy(path, '程序目录')}><code>{path}</code><Copy size={13} /></button>)}</div> : <p className="muted-copy">未添加自定义目录。</p>}</div>
      </div>
    </section>
    <section className="surface-card environment-card">
      <div className="card-heading"><div><h3>环境变量</h3><p>传入服务进程的本地变量</p></div><button className="text-button" type="button" onClick={onEdit}><Pencil size={14} />编辑</button></div>
      {envEntries(service.env).length ? <div className="environment-table" role="table" aria-label="环境变量"><div className="env-table-head" role="row"><span role="columnheader">名称</span><span role="columnheader">值</span><span role="columnheader" /></div>{envEntries(service.env).map(([key, value]) => <div className="env-table-row" role="row" key={key}><code role="cell">{key}</code><code role="cell" title={value}>{value}</code><button className="icon-button small" type="button" aria-label={`复制环境变量 ${key}`} title="复制变量值" onClick={() => onCopy(value, key)}><Copy size={14} /></button></div>)}</div> : <div className="empty-inline"><Activity size={18} /><span>未配置环境变量。</span></div>}
    </section>
    <div className="environment-footnote"><Activity size={15} /><span>环境仅用于启动进程，不会自动安装依赖或修改项目文件。</span></div>
  </div>;
}
