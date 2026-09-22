/** 前后端通信契约：运行状态由 Rust 维护，配置与运行实例不得混用。 */
export interface ServiceConfig {
  id: string;
  kind: 'command' | 'frontend' | 'staticSite' | 'desktopApp';
  launch: LaunchConfig;
  source: 'managed' | 'external';
  external: ExternalInstance | null;
  name: string;
  description: string;
  workingDir: string;
  environmentType: 'system' | 'venv' | 'custom';
  environmentDir: string;
  executableDirs: string[];
  env: Record<string, string>;
  command: string;
  shell: string;
  expectedPorts: number[];
  notes: string;
  createdAt: string;
  updatedAt: string;
}
export type LaunchConfig =
  | { type: 'command' }
  | { type: 'frontend'; packageManager: string; script: string; args: string[]; url: string }
  | { type: 'staticSite'; root: string; port: number; spaFallback: boolean }
  | { type: 'desktopApp'; path: string; args: string[]; startDir: string };
export interface FrontendProject { name: string; scripts: Record<string, string>; packageManager: string | null; suggestedScript: string | null; warnings: string[] }
export interface ApplicationInspection { path: string; name: string; bundleId: string | null; platform: string }
export interface DiscoveredApplication { instance: ExternalInstance; path: string; bundleId: string | null; windowCount: number; managedServiceId: string | null; members: ProcessIdentity[] }
export interface ApplicationSnapshot { updatedAt: string | null; stale: boolean; error: string | null; applications: DiscoveredApplication[] }
export interface AttachApplicationRequest { identity: ProcessIdentity; path: string; name: string; notes: string; workingDir: string; existingServiceId: string | null }
export interface ServiceLaunchResult { runtime: ServiceRuntime; session: TerminalSession | null; candidates: DiscoveredApplication[] }
export interface PortInfo {
  address: string;
  port: number;
  protocol: 'TCP' | 'UDP';
  pid: number | null;
}
export interface ServiceRuntime {
  serviceId: string;
  runId: string | null;
  status: 'stopped' | 'starting' | 'running' | 'stopping' | 'failed' | 'unknown';
  pid: number | null;
  ports: PortInfo[];
  conflicts: PortInfo[];
  portError: string | null;
  exitCode: number | null;
  error: string | null;
  startedAt: string | null;
  updatedAt: string | null;
  stale: boolean;
  canStop: boolean;
  canUnlock: boolean;
  canConvert: boolean;
  protectionUnlocked: boolean;
  controlReason: string | null;
  canStart: boolean;
  canRestart: boolean;
  canFocus: boolean;
  canRequestQuit: boolean;
  canForceQuit: boolean;
  actionReasons: Record<string, string>;
  webUrls: string[];
  messages: string[];
}
/** 外部进程创建标识保持字符串精度，不能仅按 PID 或端口匹配。 */
export interface ProcessIdentity { pid: number; createdAt: string }
export interface ExternalInstance {
  identity: ProcessIdentity;
  name: string;
  executable: string | null;
  workingDir: string | null;
  category: 'system' | 'nonSystem' | 'unknown';
  reasons: string[];
  sharedHost: boolean;
  controllable: boolean;
  controlReason: string | null;
}
export interface DiscoveredProcess {
  instance: ExternalInstance;
  ports: PortInfo[];
  managedServiceId: string | null;
  /** 仅用于合并显示已核验的进程族，接入与停止仍使用 instance.identity。 */
  familyIdentity?: ProcessIdentity | null;
}
export interface DiscoverySnapshot {
  revision: number;
  updatedAt: string | null;
  stale: boolean;
  error: string | null;
  processes: DiscoveredProcess[];
  unattributedPorts: PortInfo[];
}
export interface AttachExternalRequest { identity: ProcessIdentity; name: string; notes: string; workingDir: string; existingServiceId: string | null }
export interface TerminalSession {
  id: string;
  serviceId: string;
  runId: string | null;
  kind: 'service' | 'shell' | 'agent';
  title: string;
  pid: number | null;
  state: 'running' | 'exited';
  exitCode: number | null;
  createdAt: string;
}
export interface TerminalEvent {
  sessionId: string;
  serviceId: string;
  runId: string | null;
  seq: number;
  kind: 'data' | 'exit' | 'gap';
  data: number[];
  exitCode: number | null;
  droppedBytes: number;
}
export interface AgentProfile {
  id: string;
  name: string;
  executable: string;
  args: string[];
  env: Record<string, string>;
}
export interface AgentAvailability { available: boolean; resolvedPath: string | null; error: string | null }
export interface TerminalColors {
  background: string; foreground: string; cursor: string; selectionBackground: string;
  black: string; red: string; green: string; yellow: string; blue: string; magenta: string; cyan: string; white: string;
  brightBlack: string; brightRed: string; brightGreen: string; brightYellow: string; brightBlue: string; brightMagenta: string; brightCyan: string; brightWhite: string;
}
export interface TerminalAppearance {
  theme: 'system' | 'dark' | 'light' | 'dracula' | 'solarizedDark' | 'custom';
  fontFamily: string; fontSize: number; lineHeight: number;
  cursorStyle: 'block' | 'underline' | 'bar'; cursorBlink: boolean;
  colors: TerminalColors;
}
export interface Preferences { theme: 'system' | 'dark' | 'light'; terminal: TerminalAppearance }
export interface ServiceDocument { id: string; serviceId: string; title: string; content: string; updatedAt: string }
export interface Bootstrap {
  services: ServiceConfig[];
  agents: AgentProfile[];
  preferences: Preferences;
  runtimes: ServiceRuntime[];
  sessions: TerminalSession[];
}
