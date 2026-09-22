use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// 数据传输使用 camelCase；外部关联可保存 PID 与创建身份，但运行状态和解除保护仅存内存。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceConfig {
    pub id: String,
    #[serde(default = "command_kind")]
    pub kind: String,
    #[serde(default)]
    pub launch: LaunchConfig,
    #[serde(default = "managed_source")]
    pub source: String,
    #[serde(default)]
    pub external: Option<ExternalInstance>,
    pub name: String,
    pub description: String,
    pub working_dir: String,
    pub environment_type: String,
    pub environment_dir: String,
    pub executable_dirs: Vec<String>,
    pub env: BTreeMap<String, String>,
    pub command: String,
    pub shell: String,
    pub expected_ports: Vec<u16>,
    pub notes: String,
    pub created_at: String,
    pub updated_at: String,
}
fn managed_source() -> String {
    "managed".into()
}
fn command_kind() -> String {
    "command".into()
}

/// 启动配方与来源分别描述“如何启动”和“从何接入”，避免桌面应用误入 PTY 清理路径。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum LaunchConfig {
    #[default]
    Command,
    Frontend {
        package_manager: String,
        script: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        url: String,
    },
    StaticSite {
        root: String,
        #[serde(default)]
        port: u16,
        #[serde(default = "default_true")]
        spa_fallback: bool,
    },
    DesktopApp {
        path: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        start_dir: String,
    },
}
fn default_true() -> bool {
    true
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FrontendProject {
    pub name: String,
    pub scripts: BTreeMap<String, String>,
    pub package_manager: Option<String>,
    pub suggested_script: Option<String>,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationBinding {
    pub instance: ExternalInstance,
    pub target_path: String,
    pub bundle_id: Option<String>,
    pub origin: String,
    pub bound_at: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredApplication {
    pub instance: ExternalInstance,
    pub path: String,
    pub bundle_id: Option<String>,
    pub window_count: u32,
    pub managed_service_id: Option<String>,
    #[serde(default)]
    pub members: Vec<ProcessIdentity>,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationSnapshot {
    pub updated_at: Option<String>,
    pub stale: bool,
    pub error: Option<String>,
    pub applications: Vec<DiscoveredApplication>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachApplicationRequest {
    pub identity: ProcessIdentity,
    pub path: String,
    pub name: String,
    pub notes: String,
    pub working_dir: String,
    pub existing_service_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ApplicationInspection {
    pub path: String,
    pub name: String,
    pub bundle_id: Option<String>,
    pub platform: String,
}

/// 创建时间以字符串传输，避免 JavaScript 将 Windows 100ns 时间戳截断成不精确数字。
/// PID 只是定位线索；所有外部控制必须同时核对创建身份，不能按端口或进程名重新绑定。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct ProcessIdentity {
    pub pid: u32,
    pub created_at: String,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalInstance {
    pub identity: ProcessIdentity,
    pub name: String,
    pub executable: Option<String>,
    pub working_dir: Option<String>,
    pub category: String,
    pub reasons: Vec<String>,
    pub shared_host: bool,
    pub controllable: bool,
    pub control_reason: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredProcess {
    pub instance: ExternalInstance,
    pub ports: Vec<PortInfo>,
    pub managed_service_id: Option<String>,
    /// 经原生身份及父子关系复核的展示分组根；不用于扩展接入或停止操作的范围。
    #[serde(default)]
    pub family_identity: Option<ProcessIdentity>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverySnapshot {
    pub revision: u64,
    pub updated_at: Option<String>,
    pub stale: bool,
    pub error: Option<String>,
    pub processes: Vec<DiscoveredProcess>,
    pub unattributed_ports: Vec<PortInfo>,
}
#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AttachExternalRequest {
    pub identity: ProcessIdentity,
    pub name: String,
    pub notes: String,
    pub working_dir: String,
    pub existing_service_id: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "camelCase")]
pub struct PortInfo {
    pub address: String,
    pub port: u16,
    pub protocol: String,
    pub pid: Option<u32>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceRuntime {
    pub service_id: String,
    pub run_id: Option<String>,
    pub status: String,
    pub pid: Option<u32>,
    pub ports: Vec<PortInfo>,
    pub conflicts: Vec<PortInfo>,
    pub port_error: Option<String>,
    pub exit_code: Option<u32>,
    pub error: Option<String>,
    pub started_at: Option<String>,
    pub updated_at: Option<String>,
    pub stale: bool,
    pub can_stop: bool,
    pub can_unlock: bool,
    pub can_convert: bool,
    pub protection_unlocked: bool,
    pub control_reason: Option<String>,
    #[serde(default)]
    pub can_start: bool,
    #[serde(default)]
    pub can_restart: bool,
    #[serde(default)]
    pub can_focus: bool,
    #[serde(default)]
    pub can_request_quit: bool,
    #[serde(default)]
    pub can_force_quit: bool,
    #[serde(default)]
    pub action_reasons: BTreeMap<String, String>,
    #[serde(default)]
    pub web_urls: Vec<String>,
    /// 简短的有界操作/预览日志，不冒充应用本身的控制台输出。
    #[serde(default)]
    pub messages: Vec<String>,
}
impl ServiceRuntime {
    pub fn stopped(service_id: &str) -> Self {
        Self {
            service_id: service_id.into(),
            run_id: None,
            status: "stopped".into(),
            pid: None,
            ports: vec![],
            conflicts: vec![],
            port_error: None,
            exit_code: None,
            error: None,
            started_at: None,
            updated_at: None,
            stale: false,
            can_stop: false,
            can_unlock: false,
            can_convert: false,
            protection_unlocked: false,
            control_reason: None,
            can_start: true,
            can_restart: false,
            can_focus: false,
            can_request_quit: false,
            can_force_quit: false,
            action_reasons: BTreeMap::new(),
            web_urls: vec![],
            messages: vec![],
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceLaunchResult {
    pub runtime: ServiceRuntime,
    pub session: Option<TerminalSession>,
    /// 多个精确匹配实例只供用户选择，后端不会默认接管其中任意一个。
    #[serde(default)]
    pub candidates: Vec<DiscoveredApplication>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalSession {
    pub id: String,
    pub service_id: String,
    pub run_id: Option<String>,
    pub kind: String,
    pub title: String,
    pub pid: Option<u32>,
    pub state: String,
    pub exit_code: Option<u32>,
    pub created_at: String,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TerminalEvent {
    pub session_id: String,
    pub service_id: String,
    pub run_id: Option<String>,
    pub seq: u64,
    pub kind: String,
    pub data: Vec<u8>,
    pub exit_code: Option<u32>,
    pub dropped_bytes: usize,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentProfile {
    pub id: String,
    pub name: String,
    pub executable: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
}
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentAvailability {
    pub available: bool,
    pub resolved_path: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Preferences {
    pub theme: String,
    /// 终端外观单独保存，缺少该字段的旧偏好 JSON 会自动使用稳定默认值。
    #[serde(default)]
    pub terminal: TerminalAppearance,
}
impl Default for Preferences {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            terminal: TerminalAppearance::default(),
        }
    }
}

/// 终端外观设置。字段名称与前端 `TerminalAppearance` 保持 camelCase，主题和光标样式
/// 使用字符串以便保留前端的自定义主题契约，具体枚举范围由存储层保存时校验。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalAppearance {
    pub theme: String,
    pub font_family: String,
    pub font_size: f64,
    pub line_height: f64,
    pub cursor_style: String,
    pub cursor_blink: bool,
    pub colors: TerminalColors,
}

impl Default for TerminalAppearance {
    fn default() -> Self {
        Self {
            theme: "system".into(),
            font_family: "\"SFMono-Regular\", Consolas, \"Liberation Mono\", monospace".into(),
            font_size: 13.0,
            line_height: 1.45,
            cursor_style: "block".into(),
            cursor_blink: true,
            colors: TerminalColors::default(),
        }
    }
}

/// 终端颜色调色板，字段名称与前端调色板契约保持一致。
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct TerminalColors {
    pub background: String,
    pub foreground: String,
    pub cursor: String,
    pub selection_background: String,
    pub black: String,
    pub red: String,
    pub green: String,
    pub yellow: String,
    pub blue: String,
    pub magenta: String,
    pub cyan: String,
    pub white: String,
    pub bright_black: String,
    pub bright_red: String,
    pub bright_green: String,
    pub bright_yellow: String,
    pub bright_blue: String,
    pub bright_magenta: String,
    pub bright_cyan: String,
    pub bright_white: String,
}

impl Default for TerminalColors {
    fn default() -> Self {
        Self {
            background: "#11171c".into(),
            foreground: "#d9e3ee".into(),
            cursor: "#e6edf7".into(),
            selection_background: "#365779".into(),
            black: "#25313d".into(),
            red: "#f47983".into(),
            green: "#73d59a".into(),
            yellow: "#e4c979".into(),
            blue: "#76a8ff".into(),
            magenta: "#c8a5eb".into(),
            cyan: "#80d7d1".into(),
            white: "#d9e3ee".into(),
            bright_black: "#5f6f7e".into(),
            bright_red: "#ff9ca4".into(),
            bright_green: "#a0edbf".into(),
            bright_yellow: "#f5e8a8".into(),
            bright_blue: "#a8c8ff".into(),
            bright_magenta: "#e4d2ff".into(),
            bright_cyan: "#a9f0eb".into(),
            bright_white: "#ffffff".into(),
        }
    }
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ServiceDocument {
    pub id: String,
    pub service_id: String,
    pub title: String,
    pub content: String,
    pub updated_at: String,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bootstrap {
    pub services: Vec<ServiceConfig>,
    pub agents: Vec<AgentProfile>,
    pub preferences: Preferences,
    pub runtimes: Vec<ServiceRuntime>,
    pub sessions: Vec<TerminalSession>,
}
