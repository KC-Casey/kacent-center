import { FormEvent, useEffect, useLayoutEffect, useMemo, useRef, useState } from 'react';
import { AppWindow, Check, Folder, Plus, RefreshCw, Trash2, X } from 'lucide-react';
import type { ApplicationInspection, FrontendProject, ServiceConfig, ServiceRuntime } from '../types';
import Modal from './Modal';
import { trapDialogFocus } from './focusTrap';

interface EnvironmentRow { key: string; value: string }
type ServiceKind = ServiceConfig['kind'];
type PlatformInfo = 'macos' | 'windows' | 'unknown';

interface ServiceDrawerProps {
  mode: 'create' | 'edit';
  service?: ServiceConfig;
  runtime?: ServiceRuntime;
  onClose: () => void;
  onSave: (service: ServiceConfig) => Promise<void>;
  onDirtyChange: (dirty: boolean) => void;
  chooseDirectory: () => Promise<string | null>;
  chooseApplicationPath: (platform: 'macos' | 'windows') => Promise<string | null>;
  platformInfo: () => Promise<PlatformInfo>;
  inspectFrontendProject: (path: string) => Promise<FrontendProject>;
  inspectApplication: (path: string) => Promise<ApplicationInspection>;
}

const kindOptions: Array<{ value: ServiceKind; label: string; detail: string }> = [
  { value: 'command', label: '命令服务', detail: '运行现有的本地启动命令。' },
  { value: 'frontend', label: '前端项目', detail: '从 package.json 选择包管理器和开发脚本。' },
  { value: 'staticSite', label: '静态站点', detail: '使用服务中心内置的 HTTP 预览。' },
  { value: 'desktopApp', label: '桌面应用', detail: '启动、关联和查看本机 .exe 或 .app。' },
];

const blankService = (): ServiceConfig => ({
  id: '', kind: 'command', launch: { type: 'command' }, source: 'managed', external: null,
  name: '', description: '', workingDir: '', environmentType: 'system', environmentDir: '',
  executableDirs: [], env: {}, command: '', shell: '', expectedPorts: [], notes: '', createdAt: '', updatedAt: '',
});

function freshLaunch(kind: ServiceKind, previous: ServiceConfig['launch']): ServiceConfig['launch'] {
  if (previous.type === kind) return previous;
  if (kind === 'frontend') return { type: 'frontend', packageManager: '', script: '', args: [], url: '' };
  if (kind === 'staticSite') return { type: 'staticSite', root: previous.type === 'staticSite' ? previous.root : '', port: 0, spaFallback: true };
  if (kind === 'desktopApp') return { type: 'desktopApp', path: '', args: [], startDir: '' };
  return { type: 'command' };
}

function splitLines(value: string): string[] { return value.split(/\r?\n/).map((item) => item.trim()).filter(Boolean); }

function parsePorts(value: string): { ports: number[]; invalid: string[] } {
  const parts = value.split(/[\s,;，；]+/).map((item) => item.trim()).filter(Boolean);
  const invalid: string[] = [];
  const ports: number[] = [];
  for (const part of parts) {
    const number = Number(part);
    if (!/^\d+$/.test(part) || !Number.isInteger(number) || number < 1 || number > 65535) invalid.push(part);
    else if (!ports.includes(number)) ports.push(number);
  }
  return { ports, invalid };
}

function packageManagerKey(value: string | null | undefined): string {
  const normalized = value?.toLowerCase() ?? '';
  return ['pnpm', 'yarn', 'bun', 'npm'].find((manager) => normalized === manager || normalized.startsWith(`${manager}@`) || normalized.includes(`/${manager}`)) ?? '';
}

function isRuntimeActive(runtime?: ServiceRuntime): boolean {
  return runtime?.status === 'running' || runtime?.status === 'starting' || runtime?.status === 'stopping' || runtime?.status === 'unknown';
}

/**
 * 四种服务类型共用名称、说明、工作区、环境变量和备注；启动配置按 kind 使用联合类型保存。
 * 运行中只锁定服务类型与桌面应用关联目标，其他普通配置在下次启动时生效。
 */
export default function ServiceDrawer({ mode, service, runtime, onClose, onSave, onDirtyChange, chooseDirectory, chooseApplicationPath, platformInfo, inspectFrontendProject, inspectApplication }: ServiceDrawerProps) {
  const initial = useMemo(() => service
    ? { ...service, env: { ...service.env }, executableDirs: [...service.executableDirs], launch: { ...service.launch } as ServiceConfig['launch'] }
    : blankService(), [service]);
  const [draft, setDraft] = useState<ServiceConfig>(initial);
  const [portsText, setPortsText] = useState(initial.expectedPorts.join(', '));
  const [executableText, setExecutableText] = useState(initial.executableDirs.join('\n'));
  const [environmentRows, setEnvironmentRows] = useState<EnvironmentRow[]>(Object.entries(initial.env).map(([key, value]) => ({ key, value })));
  const [frontendProject, setFrontendProject] = useState<FrontendProject | null>(null);
  const [applicationInfo, setApplicationInfo] = useState<ApplicationInspection | null>(null);
  const [platform, setPlatform] = useState<PlatformInfo>('unknown');
  const [inspecting, setInspecting] = useState(false);
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState('');
  const [discardPrompt, setDiscardPrompt] = useState(false);
  const drawerRef = useRef<HTMLElement>(null);
  const originalFingerprint = useMemo(() => JSON.stringify({
    service: initial, portsText: initial.expectedPorts.join(', '), executableText: initial.executableDirs.join('\n'),
    environmentRows: Object.entries(initial.env).map(([key, value]) => ({ key, value })),
  }), [initial]);
  const currentFingerprint = JSON.stringify({ service: draft, portsText, executableText, environmentRows });
  const dirty = currentFingerprint !== originalFingerprint;
  const parsedPorts = parsePorts(portsText);
  const kind = draft.kind;
  const active = isRuntimeActive(runtime);
  const kindLocked = mode === 'edit' && active;
  const applicationTargetLocked = kind === 'desktopApp' && active;
  const staticRootLocked = kind === 'staticSite' && active;

  useEffect(() => { void platformInfo().then(setPlatform).catch(() => setPlatform('unknown')); }, [platformInfo]);
  useEffect(() => trapDialogFocus(drawerRef), []);
  useLayoutEffect(() => {
    onDirtyChange(dirty);
    return () => onDirtyChange(false);
  }, [dirty, onDirtyChange]);
  useEffect(() => {
    const onKeyDown = (event: KeyboardEvent) => {
      if (event.key === 'Escape' && !discardPrompt) {
        event.stopPropagation();
        requestClose();
      }
    };
    document.addEventListener('keydown', onKeyDown);
    return () => document.removeEventListener('keydown', onKeyDown);
  });

  const setField = <K extends keyof ServiceConfig>(key: K, value: ServiceConfig[K]) => {
    setDraft((previous) => ({ ...previous, [key]: value }));
    setError('');
  };
  const updateLaunch = <T extends ServiceConfig['launch']['type']>(type: T, values: Partial<Extract<ServiceConfig['launch'], { type: T }>>) => {
    setDraft((previous) => ({ ...previous, launch: { ...previous.launch, ...values, type } as ServiceConfig['launch'] }));
    setError('');
  };

  const changeWorkingDirectory = (value: string) => {
    setField('workingDir', value);
    if (kind === 'frontend') {
      setFrontendProject(null);
      updateLaunch('frontend', { packageManager: '', script: '' });
    }
  };

  function requestClose() {
    if (dirty && !saving) setDiscardPrompt(true);
    else if (!saving) onClose();
  }

  const selectDirectory = async (target: 'workingDir' | 'environmentDir' | 'staticRoot' | 'applicationStartDir') => {
    try {
      const directory = await chooseDirectory();
      if (!directory) return;
      if (target === 'staticRoot' && draft.launch.type === 'staticSite') {
        updateLaunch('staticSite', { root: directory });
        setField('workingDir', directory);
      } else if (target === 'applicationStartDir' && draft.launch.type === 'desktopApp') {
        updateLaunch('desktopApp', { startDir: directory });
      } else if (target === 'workingDir' && kind === 'frontend') changeWorkingDirectory(directory);
      else if (target === 'workingDir' || target === 'environmentDir') setField(target, directory);
    } catch (reason) {
      setError(`无法选择目录：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const inspectFrontend = async () => {
    if (!draft.workingDir.trim()) return setError('请先选择前端项目目录。');
    setInspecting(true);
    setError('');
    try {
      const project = await inspectFrontendProject(draft.workingDir.trim());
      setFrontendProject(project);
      const launch = draft.launch.type === 'frontend' ? draft.launch : { type: 'frontend' as const, packageManager: '', script: '', args: [], url: '' };
      const detectedManager = packageManagerKey(project.packageManager);
      updateLaunch('frontend', {
        packageManager: launch.packageManager || detectedManager,
        script: launch.script || project.suggestedScript || '',
      });
      if (!draft.name.trim() && project.name) setField('name', project.name);
    } catch (reason) {
      setError(`读取前端项目失败：${reason instanceof Error ? reason.message : String(reason)}`);
    } finally {
      setInspecting(false);
    }
  };

  const inspectDesktopPath = async (path: string) => {
    if (platform !== 'macos' && platform !== 'windows') {
      setError('当前平台暂不支持桌面应用启动，请在 macOS 或 Windows 桌面版使用。');
      return;
    }
    if (!path.trim()) { setError('请先填写应用路径。'); return; }
    setInspecting(true);
    setError('');
    setApplicationInfo(null);
    try {
      const info = await inspectApplication(path.trim());
      setApplicationInfo(info);
      if (info.path) updateLaunch('desktopApp', { path: info.path });
      if (!draft.name.trim() || draft.name === service?.name) setField('name', info.name);
    } catch (reason) {
      setError(`检查桌面应用失败：${reason instanceof Error ? reason.message : String(reason)}`);
    } finally {
      setInspecting(false);
    }
  };

  const chooseDesktopApplication = async () => {
    if (platform !== 'macos' && platform !== 'windows') {
      setError('当前平台暂不支持桌面应用启动，请在 macOS 或 Windows 桌面版使用。');
      return;
    }
    try {
      const path = await chooseApplicationPath(platform);
      if (!path) return;
      updateLaunch('desktopApp', { path });
      await inspectDesktopPath(path);
    } catch (reason) {
      setError(`检查桌面应用失败：${reason instanceof Error ? reason.message : String(reason)}`);
    }
  };

  const changeKind = (nextKind: ServiceKind) => {
    if (kindLocked || nextKind === kind) return;
    const nextLaunch = freshLaunch(nextKind, draft.launch);
    setDraft((previous) => ({
      ...previous,
      kind: nextKind,
      launch: nextLaunch,
      command: nextKind === 'command' ? previous.command : '',
      // 静态站点以 root 为工作目录，其他类型保留 Agent/终端工作区。
      workingDir: nextKind === 'staticSite' && nextLaunch.type === 'staticSite' ? nextLaunch.root : previous.workingDir,
    }));
    setFrontendProject(null);
    setApplicationInfo(null);
    setError('');
  };

  const submit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    const name = draft.name.trim();
    const workingDir = draft.workingDir.trim();
    const command = draft.command.trim();
    const env: Record<string, string> = {};
    for (const row of environmentRows) {
      const key = row.key.trim();
      if (key) {
        if (Object.prototype.hasOwnProperty.call(env, key)) return setError(`环境变量“${key}”重复，请保留一项。`);
        env[key] = row.value;
      } else if (row.value.trim()) return setError('环境变量有值但缺少名称，请填写名称或删除该行。');
    }
    if (!name) return setError('请填写服务名称。');
    if (parsedPorts.invalid.length) return setError(`端口无效：${parsedPorts.invalid.join('、')}。请输入 1–65535 的端口号。`);

    let launch = draft.launch;
    let finalWorkingDir = workingDir;
    let expectedPorts = parsedPorts.ports;
    if (kind === 'command') {
      if (!workingDir) return setError('命令服务需要有效的工作目录。');
      if (!command) return setError('请填写启动命令。');
    } else if (kind === 'frontend') {
      if (draft.launch.type !== 'frontend') return setError('前端启动配置无效，请重新选择服务类型。');
      if (!workingDir) return setError('请选择前端项目目录。');
      if (!draft.launch.packageManager) return setError('包管理器识别缺失或存在冲突，请手动选择 npm、pnpm、yarn 或 bun。');
      if (!draft.launch.script.trim()) return setError('请选择 package.json 中的启动脚本。');
      launch = { ...draft.launch, script: draft.launch.script.trim(), url: draft.launch.url.trim(), args: draft.launch.args.filter((arg) => arg.trim().length > 0) };
      expectedPorts = [];
    } else if (kind === 'staticSite') {
      if (draft.launch.type !== 'staticSite') return setError('静态站点配置无效，请重新选择服务类型。');
      const root = draft.launch.root.trim();
      if (!root) return setError('请选择静态站点目录。');
      if (!Number.isInteger(draft.launch.port) || draft.launch.port < 0 || draft.launch.port > 65535) return setError('端口应为 0（自动分配）或 1–65535。');
      launch = { ...draft.launch, root };
      finalWorkingDir = root;
      expectedPorts = draft.launch.port > 0 ? [draft.launch.port] : [];
    } else {
      if (draft.launch.type !== 'desktopApp') return setError('桌面应用配置无效，请重新选择服务类型。');
      const path = draft.launch.path.trim();
      if (!path) return setError('请选择桌面应用文件。');
      if (platform === 'macos' && !path.toLowerCase().endsWith('.app')) return setError('macOS 桌面应用路径应指向 .app 应用包。');
      if (platform === 'windows' && !path.toLowerCase().endsWith('.exe')) return setError('Windows 桌面应用路径应指向 .exe 文件。');
      launch = { ...draft.launch, path, args: draft.launch.args.filter((arg) => arg.trim().length > 0), startDir: draft.launch.startDir.trim() };
      // Windows 启动目录只影响新进程；工作目录仍单独用于普通终端与 Agent。
      expectedPorts = [];
    }

    const payload: ServiceConfig = {
      ...draft,
      kind,
      launch,
      name,
      description: draft.description.trim(),
      workingDir: finalWorkingDir,
      environmentDir: draft.environmentDir.trim(),
      executableDirs: splitLines(executableText),
      env,
      command: kind === 'command' ? command : '',
      shell: draft.shell.trim(),
      expectedPorts,
      notes: draft.notes,
      createdAt: draft.createdAt,
      updatedAt: '',
    };

    setSaving(true);
    setError('');
    try {
      await onSave(payload);
    } catch (reason) {
      setError(reason instanceof Error ? reason.message : String(reason));
    } finally {
      setSaving(false);
    }
  };

  const title = mode === 'create' ? '新增服务' : '编辑服务';
  const selectedScript = draft.launch.type === 'frontend' ? draft.launch.script : '';
  const scriptRaw = selectedScript && frontendProject?.scripts[selectedScript];
  const desktopLaunch = draft.launch.type === 'desktopApp' ? draft.launch : null;
  const staticLaunch = draft.launch.type === 'staticSite' ? draft.launch : null;
  const frontendLaunch = draft.launch.type === 'frontend' ? draft.launch : null;

  return <>
    <div className="drawer-scrim" onMouseDown={(event) => event.target === event.currentTarget && requestClose()}>
      <aside ref={drawerRef} className="service-drawer" role="dialog" aria-modal="true" aria-labelledby="service-drawer-title">
        <header className="drawer-header">
          <div><h2 id="service-drawer-title">{title}</h2><p>{kindOptions.find((item) => item.value === kind)?.detail}</p></div>
          <button className="icon-button" type="button" aria-label="关闭服务配置" title="关闭" onClick={requestClose}><X size={18} /></button>
        </header>

        <form id="service-form" className="drawer-form" onSubmit={submit}>
          <section className="form-section">
            <div className="section-heading"><h3>服务类型</h3>{kindLocked && <small>实例正在运行，服务类型暂不可更改</small>}</div>
            <div className="service-kind-picker" role="radiogroup" aria-label="服务类型">
              {kindOptions.map((option) => <button key={option.value} type="button" role="radio" aria-checked={kind === option.value} className={`service-kind-option ${kind === option.value ? 'selected' : ''}`} disabled={kindLocked} onClick={() => changeKind(option.value)}>
                <strong>{option.label}</strong><small>{option.detail}</small>
              </button>)}
            </div>
          </section>

          <section className="form-section">
            <div className="section-heading"><h3>基本信息</h3></div>
            <div className="form-grid two-columns">
              <label className="field"><span>服务名称 <b aria-hidden="true">*</b></span><input autoFocus value={draft.name} onChange={(event) => setField('name', event.target.value)} placeholder={kind === 'desktopApp' ? '例如：代码编辑器' : kind === 'frontend' ? '例如：管理前端' : '例如：知识库 API'} /></label>
              <label className="field"><span>描述</span><input value={draft.description} onChange={(event) => setField('description', event.target.value)} placeholder="简要说明这个服务的用途" /></label>
            </div>
          </section>

          {(kind === 'command' || kind === 'frontend' || kind === 'desktopApp') && <section className="form-section">
            <div className="section-heading"><h3>{kind === 'desktopApp' ? '终端与 Agent 工作区' : '工作目录'}</h3></div>
            <label className="field"><span>工作目录 {kind !== 'desktopApp' && <b aria-hidden="true">*</b>}</span>
              <div className="path-input-row"><input value={draft.workingDir} onChange={(event) => kind === 'frontend' ? changeWorkingDirectory(event.target.value) : setField('workingDir', event.target.value)} placeholder={kind === 'desktopApp' ? '可选：用于独立终端和 Agent' : '选择项目所在目录'} /><button className="icon-button" type="button" aria-label="选择工作目录" title="选择目录" onClick={() => void selectDirectory('workingDir')}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectDirectory('workingDir')}>选择</button></div>
              {kind === 'desktopApp' && <small>macOS 应用启动目录由系统决定；此目录只用于独立终端与 Agent。</small>}
            </label>
          </section>}

          {kind === 'staticSite' && <section className="form-section">
            <div className="section-heading"><h3>静态站点</h3><small>仅预览所选目录中的文件</small></div>
            <label className="field"><span>站点目录 <b aria-hidden="true">*</b></span><div className="path-input-row"><input value={staticLaunch?.root ?? ''} disabled={staticRootLocked} onChange={(event) => updateLaunch('staticSite', { root: event.target.value })} placeholder="选择包含 index.html 的构建目录" /><button className="icon-button" type="button" aria-label="选择站点目录" title="选择目录" disabled={staticRootLocked} onClick={() => void selectDirectory('staticRoot')}><Folder size={17} /></button><button className="button secondary compact" type="button" disabled={staticRootLocked} onClick={() => void selectDirectory('staticRoot')}>选择</button></div><small>{staticRootLocked ? '当前站点正在运行；请先停止预览，再更改站点目录。' : '服务中心不会自动执行构建；请先准备好静态文件。'}</small></label>
            <div className="form-grid two-columns"><label className="field"><span>端口</span><input className="mono-input" type="number" min={0} max={65535} value={staticLaunch?.port ?? 0} onChange={(event) => updateLaunch('staticSite', { port: Number(event.target.value) })} /><small>0 表示自动分配；指定端口被占用时会显示错误。</small></label><label className="field checkbox-field"><span>路由回退</span><span className="checkbox-row"><input type="checkbox" checked={staticLaunch?.spaFallback ?? true} onChange={(event) => updateLaunch('staticSite', { spaFallback: event.target.checked })} />为单页应用将页面路由回退到 index.html</span></label></div>
          </section>}

          {kind === 'frontend' && <section className="form-section">
            <div className="section-heading section-heading-action"><h3>前端启动配置</h3><button className="button secondary compact" type="button" disabled={inspecting || !draft.workingDir.trim()} onClick={() => void inspectFrontend()}><RefreshCw size={14} className={inspecting ? 'spin-icon' : ''} />{inspecting ? '读取中…' : '读取 package.json'}</button></div>
            <div className="form-grid two-columns">
              <label className="field"><span>包管理器 <b aria-hidden="true">*</b></span><select value={frontendLaunch?.packageManager ?? ''} onChange={(event) => updateLaunch('frontend', { packageManager: event.target.value })}><option value="">请选择包管理器</option>{['npm', 'pnpm', 'yarn', 'bun'].map((manager) => <option key={manager} value={manager}>{manager}</option>)}</select><small>{frontendProject?.packageManager ? `项目声明：${frontendProject.packageManager}` : '根据唯一锁文件识别；缺失或冲突时请手动选择。'}</small></label>
              <label className="field"><span>启动脚本 <b aria-hidden="true">*</b></span><select value={frontendLaunch?.script ?? ''} onChange={(event) => updateLaunch('frontend', { script: event.target.value })}><option value="">请选择 package.json 脚本</option>{Object.keys(frontendProject?.scripts ?? {}).map((script) => <option key={script} value={script}>{script}</option>)}{selectedScript && !frontendProject?.scripts[selectedScript] && <option value={selectedScript}>{selectedScript}</option>}</select><small>{scriptRaw ? `scripts.${selectedScript}: ${scriptRaw}` : frontendProject?.suggestedScript ? `建议脚本：${frontendProject.suggestedScript}` : '优先建议 dev，其次 start。'}</small></label>
            </div>
            {frontendProject && <div className="project-inspection-note"><AppWindow size={15} /><span><strong>{frontendProject.name || '未命名项目'}</strong> · 已读取 {Object.keys(frontendProject.scripts).length} 个脚本</span></div>}
            {frontendProject?.warnings.map((warning, index) => <div className="inline-warning" key={`${warning}-${index}`}><span>{warning}</span></div>)}
            <label className="field"><span>脚本参数</span><textarea rows={2} value={(frontendLaunch?.args ?? []).join('\n')} onChange={(event) => updateLaunch('frontend', { args: splitLines(event.target.value) })} placeholder="每行一个参数，可留空" /><small>服务中心会将脚本名与每项参数分别传入包管理器。</small></label>
            <label className="field"><span>访问地址</span><input className="mono-input" value={frontendLaunch?.url ?? ''} onChange={(event) => updateLaunch('frontend', { url: event.target.value })} placeholder="例如：http://localhost:5173" /><small>未填写时，启动后根据实际监听端口提供候选地址。</small></label>
          </section>}

          {kind === 'desktopApp' && <section className="form-section">
            <div className="section-heading"><h3>桌面应用</h3><small>{platform === 'macos' ? 'macOS 应用包' : platform === 'windows' ? 'Windows 可执行文件' : '需要 macOS 或 Windows'}</small></div>
            <label className="field"><span>应用路径 <b aria-hidden="true">*</b></span><div className="path-input-row"><input className="mono-input" value={desktopLaunch?.path ?? ''} disabled={applicationTargetLocked} onChange={(event) => { updateLaunch('desktopApp', { path: event.target.value }); setApplicationInfo(null); }} placeholder={platform === 'macos' ? '/Applications/应用名.app' : 'C:\\Program Files\\应用名\\app.exe'} /><button className="button secondary compact" type="button" disabled={applicationTargetLocked || inspecting || (platform !== 'macos' && platform !== 'windows')} onClick={() => void chooseDesktopApplication()}><AppWindow size={15} />选择</button><button className="button secondary compact" type="button" disabled={applicationTargetLocked || inspecting || !desktopLaunch?.path.trim()} onClick={() => desktopLaunch && void inspectDesktopPath(desktopLaunch.path)}>{inspecting ? '检查中…' : '检查路径'}</button></div><small>按规范化应用路径关联。运行中的实例不可更改关联目标。</small></label>
            {applicationInfo && <div className="project-inspection-note"><AppWindow size={15} /><span>{applicationInfo.name}{applicationInfo.bundleId ? ` · ${applicationInfo.bundleId}` : ''} · {applicationInfo.platform}</span></div>}
            <label className="field"><span>启动参数</span><textarea rows={2} value={(desktopLaunch?.args ?? []).join('\n')} onChange={(event) => updateLaunch('desktopApp', { args: splitLines(event.target.value) })} placeholder="每行一个参数，可留空" /><small>参数只对新启动的应用实例生效。</small></label>
            {platform === 'windows' && <label className="field"><span>Windows 启动目录</span><div className="path-input-row"><input className="mono-input" value={desktopLaunch?.startDir ?? ''} onChange={(event) => updateLaunch('desktopApp', { startDir: event.target.value })} placeholder="可留空，使用应用默认目录" /><button className="icon-button" type="button" aria-label="选择 Windows 启动目录" title="选择目录" onClick={() => void selectDirectory('applicationStartDir')}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectDirectory('applicationStartDir')}>选择</button></div></label>}
            {platform === 'macos' && <p className="field-hint">macOS 应用通过系统工作区启动；应用启动目录由系统决定。工作目录仅用于独立终端与 Agent。</p>}
          </section>}

          {kind === 'command' && <section className="form-section">
            <div className="section-heading"><h3>运行配置</h3></div>
            <label className="field"><span>启动命令 <b aria-hidden="true">*</b></span><input className="mono-input" value={draft.command} onChange={(event) => setField('command', event.target.value)} placeholder="例如：python -m uvicorn main:app --port 8080" /></label>
            <div className="form-grid two-columns"><label className="field"><span>终端</span><input className="mono-input" value={draft.shell} onChange={(event) => setField('shell', event.target.value)} placeholder="留空使用系统默认终端" /><small>填写可执行程序名称或完整路径。</small></label><label className="field"><span>预期端口</span><input className="mono-input" value={portsText} onChange={(event) => setPortsText(event.target.value)} placeholder="例如：8080, 8081" inputMode="numeric" /><small>只用于检查端口，不代表服务已启动。</small></label></div>
          </section>}

          {kind !== 'staticSite' && <section className="form-section">
            <div className="section-heading"><h3>环境</h3></div>
            <div className="form-grid two-columns"><label className="field"><span>环境类型</span><select value={draft.environmentType} onChange={(event) => setField('environmentType', event.target.value as ServiceConfig['environmentType'])}><option value="system">系统环境</option><option value="venv">Python 虚拟环境</option><option value="custom">自定义环境</option></select><small>使用已有环境，不会自动安装依赖。</small></label><label className="field"><span>环境目录</span><div className="path-input-row"><input value={draft.environmentDir} onChange={(event) => setField('environmentDir', event.target.value)} placeholder="可留空，使用系统环境" /><button className="icon-button" type="button" aria-label="选择环境目录" title="选择目录" onClick={() => void selectDirectory('environmentDir')}><Folder size={17} /></button><button className="button secondary compact" type="button" onClick={() => void selectDirectory('environmentDir')}>选择</button></div></label></div>
            <label className="field"><span>自定义可执行文件目录</span><textarea rows={2} value={executableText} onChange={(event) => setExecutableText(event.target.value)} placeholder={'每行一个目录\n例如：/opt/homebrew/bin'} /><small>这些目录会加入该服务的程序搜索路径。</small></label>
            <div className="section-heading section-heading-action"><h3>环境变量</h3><button className="text-button" type="button" onClick={() => setEnvironmentRows((rows) => [...rows, { key: '', value: '' }])}><Plus size={15} /> 添加</button></div>
            {environmentRows.length === 0 ? <p className="inline-empty">未添加环境变量。</p> : <div className="env-editor"><div className="env-labels"><span>名称</span><span>值</span><span aria-hidden="true" /></div>{environmentRows.map((row, index) => <div className="env-edit-row" key={`${index}-${row.key}`}><input aria-label={`环境变量第 ${index + 1} 行名称`} value={row.key} onChange={(event) => setEnvironmentRows((rows) => rows.map((item, itemIndex) => itemIndex === index ? { ...item, key: event.target.value } : item))} placeholder="例如：APP_ENV" /><input aria-label={`环境变量第 ${index + 1} 行值`} value={row.value} onChange={(event) => setEnvironmentRows((rows) => rows.map((item, itemIndex) => itemIndex === index ? { ...item, value: event.target.value } : item))} placeholder="变量值" /><button className="icon-button danger-quiet" type="button" aria-label={`删除环境变量第 ${index + 1} 行`} title="删除此行" onClick={() => setEnvironmentRows((rows) => rows.filter((_, itemIndex) => itemIndex !== index))}><Trash2 size={16} /></button></div>)}</div>}
          </section>}

          <section className="form-section last-section"><div className="section-heading"><h3>备注</h3></div><label className="field"><span className="visually-hidden">服务备注</span><textarea rows={3} value={draft.notes} onChange={(event) => setField('notes', event.target.value)} placeholder="记录用途、启动前需要确认的事项或应用说明。" /></label></section>
          {error && <div className="form-error" role="alert">{error}</div>}
        </form>

        <footer className="drawer-footer"><span>{mode === 'create' ? '创建后不会自动启动' : '普通配置修改将在下次启动时生效'}</span><div><button className="button secondary" type="button" onClick={requestClose} disabled={saving}>取消</button><button className="button primary" type="submit" form="service-form" disabled={saving}>{saving ? <><span className="spinner" /> 保存中…</> : <><Check size={16} /> {mode === 'create' ? '创建服务' : '保存修改'}</>}</button></div></footer>
      </aside>
    </div>
    {discardPrompt && <Modal title="放弃服务配置修改？" onCancel={() => setDiscardPrompt(false)} actions={[
      { label: '继续编辑', onClick: () => setDiscardPrompt(false), variant: 'secondary', autoFocus: true },
      { label: '放弃修改', onClick: onClose, variant: 'danger' },
    ]}><p>当前表单有未保存的修改，关闭后这些修改会丢失。</p></Modal>}
  </>;
}
