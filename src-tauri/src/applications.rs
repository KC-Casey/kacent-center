//! 独立桌面应用的发现、身份跟踪和原生控制。
//!
//! 桌面应用进程不属于服务 PTY/进程组：退出服务中心、解除关联或删除服务记录都不会终止
//! 它们。所有控制操作必须同时核验 PID 创建时间和规范化目标路径；Windows 不把应用放
//! 入 close-on-exit Job，macOS 只通过 NSWorkspace 请求启动、激活或退出。

#[cfg(target_os = "macos")]
mod macos;
#[cfg(not(any(target_os = "macos", windows)))]
mod unsupported;
#[cfg(windows)]
mod windows;

use crate::discovery::DiscoveryManager;
use crate::models::{
    ApplicationBinding, ApplicationInspection, ApplicationSnapshot, AttachApplicationRequest,
    DiscoveredApplication, ExternalInstance, LaunchConfig, PortInfo, ProcessIdentity,
    ServiceConfig, ServiceLaunchResult, ServiceRuntime,
};
use crate::store::Store;
use chrono::{SecondsFormat, Utc};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use tauri::{AppHandle, Emitter};

const SCAN_INTERVAL: Duration = Duration::from_secs(2);
const QUIT_GRACE: Duration = Duration::from_secs(5);
const CONTROL_POLL: Duration = Duration::from_millis(80);
const MAX_OPERATION_MESSAGES: usize = 32;

#[derive(Clone)]
struct NativeApplication {
    identity: ProcessIdentity,
    path: String,
    name: String,
    bundle_id: Option<String>,
    window_count: u32,
}

/// 共享的本地应用目标。服务配置和内容收藏都通过这组字段进入同一套原生身份核验，
/// 不在内容层复制一份启动器或 PTY 逻辑。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationTarget {
    pub path: String,
    pub args: Vec<String>,
    pub start_dir: String,
}

/// serviceLink 确认流程使用的桌面服务快照。
///
/// `updated_at` 代表服务配置版本；`target` 额外覆盖时间戳分辨率不足时的目标
/// 变化；`identity` 记录当时的完整 PID 创建身份。动作层必须在应用全局操作锁内
/// 比较这三个字段，不能只比较 PID，也不能只依赖内容层的串行锁。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ApplicationServiceSnapshot {
    pub config_version: String,
    pub target: ApplicationTarget,
    pub identity: Option<ProcessIdentity>,
}

/// 记录控制目标的拥有者类型，供上层确认提示和日志区分服务与内容收藏。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ApplicationOwner {
    Service(String),
    Content(String),
}

impl ApplicationOwner {
    pub fn id(&self) -> &str {
        match self {
            Self::Service(id) | Self::Content(id) => id,
        }
    }
}

/// 内容启动结果。多实例时明确返回候选，不由后端猜测要接管哪一个实例。
#[derive(Clone, Debug)]
pub enum ApplicationTargetLaunch {
    Running(DiscoveredApplication),
    SelectionRequired(Vec<DiscoveredApplication>),
    /// 启动器已经交付了一个身份，但当前原生枚举还不能安全确认它的应用路径。
    /// 此状态必须留在应用管理器里，后续相同路径启动先返回这里，不能再次创建实例。
    Pending(ContentPending),
}

/// 内容启动后的待关联快照。它只在当前进程内保存完整创建身份；身份无法确认退出时，
/// 也会保守保留，避免权限短暂变化导致下一次启动误创建副本。
#[derive(Clone, Debug)]
pub struct ContentPending {
    pub target_path: String,
    pub identities: Vec<ProcessIdentity>,
    pub reason: String,
}

/// 原生枚举允许返回部分结果并附带权限提示；单个进程不可读时，不应抛弃其余应用。
#[derive(Default)]
struct NativeEnumeration {
    applications: Vec<NativeApplication>,
    warning: Option<String>,
}

#[derive(Default)]
struct ManagerState {
    snapshot: ApplicationSnapshot,
    runtimes: HashMap<String, ServiceRuntime>,
    /// 解除保护只对本次应用运行期有效，并且绑定到完整身份而非 PID。
    unlocked: HashMap<String, ProcessIdentity>,
    /// 启动器进程或多个同路径实例尚未由用户明确选定时，保存本次待关联身份。
    pending: HashMap<String, PendingAssociation>,
    /// 内容收藏没有服务运行态表，因此单独记录已启动但尚未安全关联的实例。
    /// 键是内容 ID，路径去重在查询时按所有条目检查，防止两个收藏各自重试启动。
    content_pending: HashMap<String, PendingAssociation>,
    /// 防止后台快照在一次显式启动/退出控制进行中覆盖其运行态。
    busy: HashMap<String, String>,
    last_messages: HashMap<String, Vec<String>>,
    monitor_started: bool,
}

#[derive(Clone)]
struct PendingAssociation {
    /// 启动器可能已转交其他进程；原 PID 消失不能证明应用退出，需显式关联或解除收藏。
    retain_until_associated: bool,
    target_path: String,
    identities: Vec<ProcessIdentity>,
    reason: String,
}

/// 桌面应用的原生会话管理器。Store 保存目标路径和 PID 创建身份；Windows 启动句柄只在内存中保留。
pub struct ApplicationManager {
    store: Arc<Store>,
    discovery: Arc<DiscoveryManager>,
    app: Option<AppHandle>,
    state: Mutex<ManagerState>,
    refresh_lock: Mutex<()>,
    /// 将同路径桌面应用的发现、启动、关联和退出请求串行化，避免两条服务记录同时启动副本。
    operations: Mutex<()>,
}

impl ApplicationManager {
    pub fn new(
        store: Arc<Store>,
        discovery: Arc<DiscoveryManager>,
        app: Option<AppHandle>,
    ) -> Arc<Self> {
        Arc::new(Self {
            store,
            discovery,
            app,
            state: Mutex::new(ManagerState::default()),
            refresh_lock: Mutex::new(()),
            operations: Mutex::new(()),
        })
    }

    /// 只启动一条串行扫描线程；窗口隐藏时也持续刷新，但不会对 GUI 应用发送退出请求。
    pub fn start_monitor(self: &Arc<Self>) {
        {
            let mut state = lock_recover(&self.state);
            if state.monitor_started {
                return;
            }
            state.monitor_started = true;
        }

        let manager = Arc::downgrade(self);
        let _ = thread::Builder::new()
            .name("application-monitor".into())
            .spawn(move || loop {
                thread::sleep(SCAN_INTERVAL);
                let Some(manager) = manager.upgrade() else {
                    break;
                };
                // 内容关联不写入 application_bindings；只检查服务关联会让内容实例失去
                // 后台刷新。两张表任一有活动关联时才扫描，保持无收藏时的低开销。
                let has_service_bindings = manager
                    .store
                    .application_bindings()
                    .map(|bindings| !bindings.is_empty())
                    .unwrap_or(false);
                let has_content_bindings = manager
                    .store
                    .content_application_bindings()
                    .map(|bindings| !bindings.is_empty())
                    .unwrap_or(false);
                if has_service_bindings || has_content_bindings {
                    manager.snapshot(false);
                }
            });
    }

    /// 主动刷新运行中应用列表，并同步每条桌面应用服务的运行态。
    pub fn refresh(&self) -> ApplicationSnapshot {
        self.snapshot(true)
    }

    pub fn runtimes(&self) -> Vec<ServiceRuntime> {
        lock_recover(&self.state)
            .runtimes
            .values()
            .cloned()
            .collect()
    }

    /// 取 2 秒缓存。扫描失败保留最后一份可信快照，不将旧实例误标为停止。
    pub fn snapshot(&self, force: bool) -> ApplicationSnapshot {
        self.snapshot_impl(force, false)
    }

    /// start_inner 已持有全局操作锁时调用，避免嵌套获取同一把锁。
    fn snapshot_impl(&self, force: bool, operation_lock_held: bool) -> ApplicationSnapshot {
        // 所有外部快照与服务变更采用相同锁顺序：operations -> refresh_lock。
        // 避免扫描末尾才取操作锁时，与启动期间的嵌套扫描形成锁顺序反转。
        let _operation_guard = if operation_lock_held {
            None
        } else {
            Some(lock_recover(&self.operations))
        };
        let _refresh = lock_recover(&self.refresh_lock);
        if !force {
            let state = lock_recover(&self.state);
            let still_fresh = state
                .snapshot
                .updated_at
                .as_deref()
                .and_then(parse_timestamp)
                .map(|updated| Utc::now().signed_duration_since(updated).num_milliseconds() < 2_000)
                .unwrap_or(false);
            if still_fresh && !state.snapshot.stale {
                return state.snapshot.clone();
            }
        }

        let native = match platform::enumerate() {
            Ok(enumeration) => enumeration,
            Err(error) => return self.mark_snapshot_failed(error),
        };

        let port_snapshot = self.discovery.snapshot(false);
        let bindings = match self.store.application_bindings() {
            Ok(bindings) => bindings,
            Err(error) => {
                return self.mark_snapshot_failed(format!("读取桌面应用关联记录失败：{error}"));
            }
        };

        let all_services = match self.store.services() {
            Ok(services) => services,
            Err(error) => {
                return self.mark_snapshot_failed(format!("读取桌面应用配置失败：{error}"));
            }
        };
        let desktop_services = all_services
            .iter()
            .filter(|service| service.kind == "desktopApp")
            .collect::<Vec<_>>();

        let bindings_by_id = bindings
            .iter()
            .map(|(id, binding)| (id.as_str(), binding))
            .collect::<HashMap<_, _>>();
        let mut applications = Vec::with_capacity(native.applications.len());
        let mut live_by_service =
            HashMap::<String, (ApplicationBinding, DiscoveredApplication)>::new();

        for native_app in native.applications {
            let mut instance = self
                .discovery
                .inspect(&native_app.identity)
                .ok()
                .flatten()
                .unwrap_or_else(|| unknown_instance(&native_app));
            // AppKit 提供的本地化标题比进程名更适合展示；身份与可执行路径仍用原生信息。
            instance.name = native_app.name.clone();
            instance.executable =
                platform::application_executable(&native_app.path).or(instance.executable);

            let members = self.process_family(&native_app.identity, &instance, &port_snapshot);
            let managed_service_id = bindings_by_id
                .iter()
                .find_map(|(service_id, binding)| {
                    if binding.instance.identity == native_app.identity
                        && same_path(&binding.target_path, &native_app.path)
                    {
                        Some((*service_id).to_owned())
                    } else {
                        None
                    }
                })
                .or_else(|| {
                    // 外部命令服务仍由 ExternalManager 控制；此标记只阻止把同一 PID 当成新桌面应用接入。
                    all_services.iter().find_map(|service| {
                        (service.source == "external"
                            && service.kind != "desktopApp"
                            && service
                                .external
                                .as_ref()
                                .map(|external| external.identity == native_app.identity)
                                .unwrap_or(false))
                        .then(|| service.id.clone())
                    })
                });
            let discovered = DiscoveredApplication {
                instance: instance.clone(),
                path: native_app.path.clone(),
                bundle_id: native_app.bundle_id.clone(),
                window_count: native_app.window_count,
                managed_service_id: managed_service_id.clone(),
                members: members.clone(),
            };

            if let Some(service_id) = managed_service_id {
                if let Some((_, binding)) = bindings_by_id
                    .get(service_id.as_str())
                    .map(|binding| (service_id.as_str(), *binding))
                {
                    live_by_service.insert(service_id, (binding.clone(), discovered.clone()));
                }
            }
            applications.push(discovered);
        }

        applications.sort_by(|left, right| {
            left.instance
                .name
                .to_lowercase()
                .cmp(&right.instance.name.to_lowercase())
                .then(left.instance.identity.pid.cmp(&right.instance.identity.pid))
        });

        let now = timestamp_now();
        let unlocked = lock_recover(&self.state).unlocked.clone();
        let mut next_runtimes = HashMap::new();
        let pending_before_scan = lock_recover(&self.state).pending.clone();
        let busy_before_scan = lock_recover(&self.state).busy.clone();
        for service in &desktop_services {
            if busy_before_scan.contains_key(&service.id) {
                if let Some(runtime) = lock_recover(&self.state).runtimes.get(&service.id).cloned()
                {
                    next_runtimes.insert(service.id.clone(), runtime);
                    continue;
                }
            }
            let binding = bindings_by_id.get(service.id.as_str()).copied();
            let runtime = match (binding, live_by_service.get(&service.id)) {
                (Some(binding), Some((_, discovered))) => {
                    let is_unlocked = unlocked
                        .get(&service.id)
                        .map(|identity| identity == &binding.instance.identity)
                        .unwrap_or(false);
                    let ports = ports_for_members(&discovered.members, &port_snapshot.processes);
                    self.make_runtime(
                        &service.id,
                        Some(binding),
                        Some(&discovered.instance),
                        ports,
                        is_unlocked,
                        discovered.window_count,
                        port_snapshot.stale,
                        Some(&now),
                    )
                }
                (Some(binding), None) => {
                    match platform::inspect_identity(&binding.instance.identity) {
                        Ok(Some(native)) => {
                            let mut runtime = unknown_runtime(
                                &service.id,
                                Some(&binding.instance.identity),
                                if same_path(&native.path, &binding.target_path) {
                                    "原实例仍存在，但未出现在系统应用枚举结果中；暂按待确认状态保留。"
                                } else {
                                    "原 PID 仍存在，但可执行路径已变化；拒绝把它当作关联应用。"
                                },
                                &now,
                            );
                            runtime.stale = true;
                            runtime.messages = lock_recover(&self.state)
                                .last_messages
                                .get(&service.id)
                                .cloned()
                                .unwrap_or_default();
                            runtime
                        }
                        Ok(None) => {
                            let mut runtime = ServiceRuntime::stopped(&service.id);
                            runtime.updated_at = Some(now.clone());
                            runtime.messages = lock_recover(&self.state)
                                .last_messages
                                .get(&service.id)
                                .cloned()
                                .unwrap_or_default();
                            runtime
                        }
                        Err(error) => {
                            let mut runtime = unknown_runtime(
                                &service.id,
                                Some(&binding.instance.identity),
                                &format!("无法核实原实例是否退出：{error}"),
                                &now,
                            );
                            runtime.stale = true;
                            runtime
                        }
                    }
                }
                (None, _) => {
                    if let Some(pending) = pending_before_scan.get(&service.id) {
                        self.pending_runtime(&service.id, pending, &applications, &now)
                    } else {
                        let mut runtime = ServiceRuntime::stopped(&service.id);
                        runtime.updated_at = Some(now.clone());
                        runtime.messages = lock_recover(&self.state)
                            .last_messages
                            .get(&service.id)
                            .cloned()
                            .unwrap_or_default();
                        runtime
                    }
                }
            };
            next_runtimes.insert(service.id.clone(), runtime);
        }

        let snapshot = ApplicationSnapshot {
            updated_at: Some(now),
            // 权限不足时展示其余可读应用，但把快照标为不完整，启动操作会据此避免重复启动。
            stale: native.warning.is_some(),
            error: native.warning,
            applications,
        };

        // 扫描耗时期间若服务关联已改变，丢弃本轮结果；否则旧身份事件可能覆盖新 runId。
        let latest_bindings = match self.store.application_bindings() {
            Ok(bindings) => bindings,
            Err(error) => {
                return self.mark_snapshot_failed(format!("复核应用关联记录失败：{error}"));
            }
        };
        if !same_binding_sets(&bindings, &latest_bindings) {
            return self.mark_snapshot_failed(
                "应用关联在扫描期间发生变化；当前列表暂时不可信，已保留上次结果。".into(),
            );
        }

        let mut resolved_pending = HashSet::new();
        for (service_id, pending) in &pending_before_scan {
            if !desktop_services
                .iter()
                .any(|service| service.id == *service_id)
                || bindings_by_id.contains_key(service_id.as_str())
            {
                resolved_pending.insert(service_id.clone());
                continue;
            }
            if pending.identities.is_empty() {
                continue;
            }
            let mut remaining = Vec::new();
            for identity in &pending.identities {
                match platform::inspect_identity(identity) {
                    Ok(Some(_)) | Err(_) => remaining.push(identity.clone()),
                    Ok(None) => {}
                }
            }
            if remaining.is_empty() {
                resolved_pending.insert(service_id.clone());
            } else if let Some(entry) = lock_recover(&self.state).pending.get_mut(service_id) {
                entry.identities = remaining;
            }
        }

        {
            let mut state = lock_recover(&self.state);
            state.snapshot = snapshot.clone();
            for (service_id, runtime) in next_runtimes {
                if !state.busy.contains_key(&service_id) {
                    state.runtimes.insert(service_id, runtime);
                }
            }
            // 旧实例已退出后，解除保护不能继承给以后复用的同一 service id。
            state.unlocked.retain(|service_id, identity| {
                bindings_by_id
                    .get(service_id.as_str())
                    .map(|binding| binding.instance.identity == *identity)
                    .unwrap_or(false)
                    && live_by_service.contains_key(service_id)
            });
            for service_id in resolved_pending {
                state.pending.remove(&service_id);
            }
        }
        self.emit_snapshot(&snapshot);
        for runtime in self.runtimes() {
            self.emit_runtime(&runtime);
        }
        snapshot
    }

    pub fn inspect_path(path: &str) -> Result<ApplicationInspection, String> {
        let canonical = platform::validate_application_path(path)?;
        platform::inspect_path(&canonical)
    }

    /// 校验内容收藏使用的本地应用目标，但不启动应用。
    pub fn validate_target(&self, target: &ApplicationTarget) -> Result<ApplicationTarget, String> {
        let path = platform::validate_application_path(&target.path)?;
        if !target.start_dir.trim().is_empty() {
            let directory = PathBuf::from(target.start_dir.trim());
            if !directory.is_absolute() || !directory.is_dir() {
                return Err("应用启动目录必须是现有的绝对目录。".into());
            }
        }
        if target.args.iter().any(|arg| arg.contains('\0')) {
            return Err("应用参数不能包含 NUL 字符。".into());
        }
        Ok(ApplicationTarget {
            path,
            args: target.args.clone(),
            start_dir: target.start_dir.clone(),
        })
    }

    /// 刷新原生应用发现，并把同路径实例交给内容层做唯一/多实例决策。
    pub fn content_candidates(
        &self,
        target_path: &str,
    ) -> Result<Vec<DiscoveredApplication>, String> {
        let path = platform::validate_application_path(target_path)?;
        let snapshot = self.snapshot(true);
        if snapshot.stale {
            return Err(format!(
                "运行中应用快照不可用，无法确认是否已有同路径实例。{}",
                snapshot
                    .error
                    .map(|error| format!(" {error}"))
                    .unwrap_or_default()
            ));
        }
        Ok(snapshot
            .applications
            .into_iter()
            .filter(|application| same_path(&application.path, &path))
            .collect())
    }

    /// 查询同路径是否已有内容启动处于待关联状态。调用方只得到快照副本；身份检查和
    /// 清理仍在应用全局操作锁内完成，因此和 service.start/attach 共用同一去重边界。
    pub fn content_pending(&self, target_path: &str) -> Result<Option<ContentPending>, String> {
        let path = platform::validate_application_path(target_path)?;
        let _operation = lock_recover(&self.operations);
        Ok(self.content_pending_for_path_locked(&path))
    }

    /// 显式完成内容关联后清理该收藏自己的待关联记录。
    pub fn clear_content_pending(&self, content_id: &str) {
        let _operation = lock_recover(&self.operations);
        lock_recover(&self.state).content_pending.remove(content_id);
    }

    /// 服务或内容显式接管某个身份后，清理所有包含该身份的待关联记录。
    /// 这一步不能只按 owner 清理：服务 attach 可能接管一个由内容启动器交付的实例。
    pub fn clear_content_pending_identity(&self, identity: &ProcessIdentity) {
        let _operation = lock_recover(&self.operations);
        self.clear_content_pending_identity_locked(identity);
    }

    /// 查询桌面服务当前是否仍持有与创建身份匹配的实例；不会刷新或启动服务。
    pub fn service_is_running(&self, service_id: &str) -> Result<bool, String> {
        let _operation = lock_recover(&self.operations);
        // 仅凭内存中的 starting/unknown 不能称为“正在运行”：启动器可能尚未返回
        // 可核验身份，或者快照权限不足。必须先有持久化关联，再用完整创建身份和路径
        // 重新核验；这样 serviceLink 的 open 不会因为缺少 binding 而误走 focus。
        let Some(binding) = self.store.application_binding(service_id)? else {
            return Ok(false);
        };
        match platform::inspect_identity(&binding.instance.identity)? {
            Some(native) => {
                if !same_path(&native.path, &binding.target_path) {
                    return Err("桌面服务关联 PID 的路径已改变。".into());
                }
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// 在应用操作锁内读取 serviceLink 的配置版本、启动目标和完整原生身份。
    ///
    /// 内容确认前必须先取得这份快照，再去核验内容确认戳。这样旧服务界面若在
    /// 两步之间重启或改配置，后续动作会因为快照不一致而失败，而不会把确认转移
    /// 到新实例。
    pub fn service_application_snapshot(
        &self,
        service_id: &str,
    ) -> Result<ApplicationServiceSnapshot, String> {
        let _operation = lock_recover(&self.operations);
        self.service_application_snapshot_locked(service_id)
    }

    fn service_application_snapshot_locked(
        &self,
        service_id: &str,
    ) -> Result<ApplicationServiceSnapshot, String> {
        let service = self.store.service(service_id)?;
        if service.kind != "desktopApp" {
            return Err("serviceLink 只能指向 desktopApp 服务。".into());
        }
        let config_version = service_config_version(&service);
        let LaunchConfig::DesktopApp {
            path,
            args,
            start_dir,
        } = service.launch
        else {
            return Err("桌面服务缺少有效的应用目标。".into());
        };
        let identity = self
            .store
            .application_binding(service_id)?
            .map(|binding| binding.instance.identity);
        Ok(ApplicationServiceSnapshot {
            config_version,
            target: ApplicationTarget {
                path,
                args,
                start_dir,
            },
            identity,
        })
    }

    /// 与 Windows/macOS 原生适配层保持一致地报告“请求退出”是否有可用路径。
    pub fn content_can_request_quit(&self, window_count: u32) -> bool {
        platform::can_request_quit(window_count)
    }

    /// 为 serviceLink 执行动作时，把身份检查和真正控制放进同一把应用操作锁，避免
    /// 内容确认后旧桌面服务被重启而误控新实例。
    pub fn service_link_action(
        &self,
        service_id: &str,
        action: &str,
        expected_identity: Option<&ProcessIdentity>,
    ) -> Result<(), String> {
        let _operation = lock_recover(&self.operations);
        if let Some(expected) = expected_identity {
            let binding = self
                .store
                .application_binding(service_id)?
                .ok_or_else(|| "桌面服务当前没有关联实例，内容确认已失效。".to_owned())?;
            if &binding.instance.identity != expected {
                return Err("桌面服务关联实例已变化，请刷新后重新确认。".into());
            }
        }
        self.service_link_action_locked(service_id, action)
    }

    /// 使用内容确认时捕获的完整 serviceLink 快照执行动作。
    ///
    /// 该检查和真正的启动、激活、退出共用 `operations` 锁。快照中的 `identity: None`
    /// 也有明确含义：如果旧界面在确认后新建了关联，当前 binding 不再等于快照，必须
    /// 拒绝操作，不能把“当时没有实例”的确认自动转移给新实例。
    pub fn service_link_action_snapshot(
        &self,
        service_id: &str,
        action: &str,
        expected: &ApplicationServiceSnapshot,
    ) -> Result<(), String> {
        let _operation = lock_recover(&self.operations);
        let current = self.service_application_snapshot_locked(service_id)?;
        if &current != expected {
            return Err("桌面服务配置或关联实例已变化，请刷新后重新确认。".into());
        }
        self.service_link_action_locked(service_id, action)
    }

    /// 已持有 `operations` 锁时执行 serviceLink 动作。
    fn service_link_action_locked(&self, service_id: &str, action: &str) -> Result<(), String> {
        if matches!(action, "focus" | "requestQuit" | "forceQuit") {
            let (_, instance, _) = self.current_control_target(service_id)?;
            if instance.shared_host
                || !instance.controllable
                || matches!(instance.category.as_str(), "system" | "unknown")
            {
                return Err(instance
                    .control_reason
                    .clone()
                    .unwrap_or_else(|| "系统或待确认应用默认只读，不能从内容收藏控制。".into()));
            }
        }
        match action {
            "start" => {
                let service = self.store.service(service_id)?;
                self.start_locked(service)?;
            }
            "focus" => self.focus_locked(service_id)?,
            "requestQuit" => {
                let runtime = self.request_quit_locked(service_id)?;
                if runtime.status != "stopped" {
                    return Err(runtime.error.unwrap_or_else(|| {
                        "应用尚未退出，仍保留跟踪；可继续等待或单独确认强制结束。".into()
                    }));
                }
            }
            "forceQuit" => {
                self.force_quit_locked(service_id)?;
            }
            other => return Err(format!("桌面服务不支持内容操作：{other}")),
        }
        Ok(())
    }

    /// 由保存服务配置的命令调用。桌面服务的配置写入必须和启动、接入、内容关联
    /// 共用同一把应用操作锁，并在锁内重新核验运行态；不能只依赖上层按服务 ID
    /// 创建的普通编辑锁。
    pub fn save_service_config(&self, config: ServiceConfig) -> Result<ServiceConfig, String> {
        let _operation = lock_recover(&self.operations);
        if config.kind != "desktopApp" {
            return Err("save_service_config 只接受 desktopApp 服务配置。".into());
        }
        if !config.id.trim().is_empty() {
            let previous = self.store.service(&config.id)?;
            let target_changed = desktop_service_target_changed(&previous, &config);
            if previous.kind != config.kind || target_changed {
                self.ensure_desktop_edit_is_stopped(&config.id)?;
            }
        }
        self.store.save_service(config)
    }

    /// 目标或类型变更时保守确认当前服务没有运行中的实例。
    fn ensure_desktop_edit_is_stopped(&self, service_id: &str) -> Result<(), String> {
        let state = lock_recover(&self.state);
        if state.pending.contains_key(service_id) || state.busy.contains_key(service_id) {
            return Err("实例活动期间不能切换服务类型或运行目标，请先停止或解除关联。".into());
        }
        drop(state);

        let Some(binding) = self.store.application_binding(service_id)? else {
            return Ok(());
        };
        match platform::inspect_identity(&binding.instance.identity) {
            Ok(None) => Ok(()),
            Ok(Some(native)) if same_path(&native.path, &binding.target_path) => {
                Err("实例活动期间不能切换服务类型或运行目标，请先停止或解除关联。".into())
            }
            Ok(Some(_)) => Err("无法确认原关联实例已停止，已拒绝切换服务类型或运行目标。".into()),
            Err(error) => Err(format!(
                "无法核验原关联实例是否停止，已拒绝切换服务类型或运行目标：{error}"
            )),
        }
    }

    /// 启动或复用本地应用。整个决策持有与服务启动相同的全局应用操作锁，确保同路径
    /// 的内容收藏不会互相启动副本；方法不会创建 services/application_bindings/PTY。
    pub fn start_content_target(
        &self,
        owner: &ApplicationOwner,
        target: &ApplicationTarget,
    ) -> Result<ApplicationTargetLaunch, String> {
        let _operation = lock_recover(&self.operations);
        let target = self.validate_target(target)?;
        if let Some(pending) = self.content_pending_for_path_locked(&target.path) {
            return match owner {
                ApplicationOwner::Content(_) => Ok(ApplicationTargetLaunch::Pending(pending)),
                ApplicationOwner::Service(_) => Err(format!(
                    "同一路径已有内容启动处于待关联状态，已拒绝重复启动：{}",
                    pending.reason
                )),
            };
        }
        let snapshot = self.snapshot_impl(true, true);
        if snapshot.stale {
            return Err(format!(
                "运行中应用快照不可用，无法确认是否已有同路径实例。{}",
                snapshot
                    .error
                    .map(|error| format!(" {error}"))
                    .unwrap_or_default()
            ));
        }
        let mut candidates = snapshot
            .applications
            .into_iter()
            .filter(|application| same_path(&application.path, &target.path))
            .collect::<Vec<_>>();
        // 已由桌面服务持有的实例必须通过 serviceLink 使用；内容层不能复制所有权。
        if let Some(application) = candidates
            .iter()
            .find(|application| application.managed_service_id.is_some())
        {
            return Err(format!(
                "PID {} 已由桌面服务关联，请改用 serviceLink 收藏。",
                application.instance.identity.pid
            ));
        }
        if candidates.len() > 1 {
            return Ok(ApplicationTargetLaunch::SelectionRequired(candidates));
        }
        if let Some(application) = candidates.pop() {
            // 即使身份已经可见，也要先登记一个很短的待关联保留。真正写入
            // content_application_bindings 发生在调用方返回后；这段窗口内 service.start
            // 仍必须看到同一路径占用，不能因为尚未落库就再次启动。
            if let ApplicationOwner::Content(content_id) = owner {
                self.set_content_pending(
                    content_id,
                    &target.path,
                    vec![application.instance.identity.clone()],
                    "内容正在保存已核验的原生应用关联，请稍候。",
                    false,
                );
            }
            return Ok(ApplicationTargetLaunch::Running(application));
        }

        let identity = platform::launch(
            &target.path,
            &target.args,
            &target.start_dir,
            &BTreeMap::new(),
        )
        .map_err(|error| format!("无法启动内容 {} 的本地应用：{error}", owner.id()))?;
        let native = match platform::inspect_identity(&identity) {
            Ok(Some(native)) if same_path(&native.path, &target.path) => native,
            Ok(Some(_)) => {
                let reason = "启动器交付的进程路径与内容目标不同，已保留待关联，不会自动接管。";
                return self.pending_content_launch(owner, &target.path, identity, reason);
            }
            Ok(None) => {
                let reason = "应用已请求启动，但原生身份暂不可核验；已保留待关联状态。";
                return self.pending_content_launch(owner, &target.path, identity, reason);
            }
            Err(error) => {
                let reason =
                    format!("应用已请求启动，但无法读取原生身份：{error}；已保留待关联状态。");
                return self.pending_content_launch(owner, &target.path, identity, &reason);
            }
        };
        if let ApplicationOwner::Content(content_id) = owner {
            self.set_content_pending(
                content_id,
                &target.path,
                vec![identity.clone()],
                "内容正在保存已核验的原生应用关联，请稍候。",
                false,
            );
        }
        Ok(ApplicationTargetLaunch::Running(
            self.discovered_from_native(&native),
        ))
    }

    /// 启动器返回身份但原生检查失败时保存待关联记录。服务启动没有内容 owner，仍应
    /// 直接失败并把原因交给服务运行态；内容启动则返回 Pending，阻止所有同路径重试。
    fn pending_content_launch(
        &self,
        owner: &ApplicationOwner,
        target_path: &str,
        identity: ProcessIdentity,
        reason: &str,
    ) -> Result<ApplicationTargetLaunch, String> {
        match owner {
            ApplicationOwner::Content(content_id) => Ok(ApplicationTargetLaunch::Pending(
                self.set_content_pending(content_id, target_path, vec![identity], reason, true),
            )),
            ApplicationOwner::Service(_) => Err(reason.to_owned()),
        }
    }

    /// 用内容关联的完整创建身份重新读取原生实例。PID 相同但创建时间不同会返回“不存在”。
    pub fn inspect_content_identity(
        &self,
        identity: &ProcessIdentity,
        target_path: &str,
    ) -> Result<Option<DiscoveredApplication>, String> {
        let canonical = platform::validate_application_path(target_path)?;
        let Some(native) = platform::inspect_identity(identity)? else {
            return Ok(None);
        };
        if !same_path(&native.path, &canonical) {
            return Err("当前 PID 的原生应用路径与内容目标不同。".into());
        }
        Ok(Some(self.discovered_from_native(&native)))
    }

    /// 对内容收藏执行激活前的身份、路径、共享宿主和控制权限核验。
    fn content_control_target(
        &self,
        identity: &ProcessIdentity,
        target_path: &str,
    ) -> Result<(ApplicationBinding, DiscoveredApplication), String> {
        let canonical = platform::validate_application_path(target_path)?;
        let Some(native) = platform::inspect_identity(identity)? else {
            return Err("关联的应用实例已退出或 PID 身份已变化。".into());
        };
        if !same_path(&native.path, &canonical) {
            return Err("当前 PID 的应用路径与内容目标不同，拒绝控制。".into());
        }
        let discovered = self.discovered_from_native(&native);
        let instance = discovered.instance.clone();
        if instance.shared_host {
            return Err("共享宿主只读，不能执行内容应用控制。".into());
        }
        if !instance.controllable {
            return Err(instance
                .control_reason
                .clone()
                .unwrap_or_else(|| "当前用户无法控制此应用实例。".into()));
        }
        if matches!(instance.category.as_str(), "system" | "unknown") {
            return Err("系统或待确认应用默认只读，不能从内容收藏控制。".into());
        }
        Ok((
            ApplicationBinding {
                instance,
                target_path: canonical,
                bundle_id: discovered.bundle_id.clone(),
                origin: "content".into(),
                bound_at: timestamp_now(),
            },
            discovered,
        ))
    }

    pub fn focus_content(
        &self,
        identity: &ProcessIdentity,
        target_path: &str,
    ) -> Result<(), String> {
        let _operation = lock_recover(&self.operations);
        let (binding, application) = self.content_control_target(identity, target_path)?;
        if application.window_count == 0 {
            return Err("该应用当前没有可显示的窗口。".into());
        }
        platform::focus(&binding.instance.identity, &binding.target_path)
    }

    pub fn request_quit_content(
        &self,
        identity: &ProcessIdentity,
        target_path: &str,
    ) -> Result<(), String> {
        let _operation = lock_recover(&self.operations);
        let (binding, _) = self.content_control_target(identity, target_path)?;
        platform::request_quit(&binding.instance.identity, &binding.target_path)?;
        let deadline = Instant::now() + QUIT_GRACE;
        while Instant::now() < deadline {
            if platform::inspect_identity(&binding.instance.identity)?.is_none() {
                return Ok(());
            }
            thread::sleep(CONTROL_POLL);
        }
        Err("应用在 5 秒内未退出；内容收藏仍保留原关联。".into())
    }

    pub fn force_quit_content(
        &self,
        identity: &ProcessIdentity,
        target_path: &str,
    ) -> Result<(), String> {
        let _operation = lock_recover(&self.operations);
        let (binding, _) = self.content_control_target(identity, target_path)?;
        platform::force_quit(&binding.instance.identity, &binding.target_path)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            if platform::inspect_identity(&binding.instance.identity)?.is_none() {
                return Ok(());
            }
            thread::sleep(CONTROL_POLL);
        }
        Err("系统接受了结束请求，但应用仍可见；内容收藏仍保留原关联。".into())
    }

    /// 打开文件夹或 Steam 固定 URI。参数直接传给系统启动器，不通过 shell 拼接命令。
    pub fn open_location(&self, location: &str) -> Result<(), String> {
        if location.trim().is_empty() || location.contains('\0') {
            return Err("打开目标为空或包含非法字符。".into());
        }
        #[cfg(target_os = "macos")]
        let result = Command::new("open").arg(location).status();
        #[cfg(target_os = "windows")]
        let result = Command::new("explorer").arg(location).status();
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let result = Command::new("xdg-open").arg(location).status();
        let status = result.map_err(|error| format!("无法请求系统打开目标：{error}"))?;
        if !status.success() {
            return Err(format!("系统打开目标失败，退出码：{:?}", status.code()));
        }
        Ok(())
    }

    /// 按服务中保存的 `.app`/`.exe` 启动；同一路径唯一实例会被明确复用，多实例时返回
    /// 候选清单，绝不凭 Bundle ID 或进程名任意选择一个实例。
    pub fn start(&self, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let service_id = config.id.clone();
        let result = {
            let _operation = lock_recover(&self.operations);
            self.start_locked(config)
        };
        self.refresh();
        match result {
            Ok(mut result) => {
                result.runtime = self.runtime_or_stopped(&service_id);
                Ok(result)
            }
            Err(error) => {
                let runtime = self.runtime_or_stopped(&service_id);
                if runtime.status == "starting" {
                    let mut failed = ServiceRuntime::stopped(&service_id);
                    failed.status = "failed".into();
                    failed.error = Some(error.clone());
                    failed.updated_at = Some(timestamp_now());
                    self.set_runtime(failed);
                }
                Err(error)
            }
        }
    }

    fn start_locked(&self, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let service_id = config.id.clone();
        let busy = self.begin_busy(&service_id, "starting")?;
        let mut starting = ServiceRuntime::stopped(&service_id);
        starting.status = "starting".into();
        starting.can_start = false;
        starting.updated_at = Some(timestamp_now());
        self.set_runtime(starting);
        let result = self.start_inner(config);
        drop(busy);
        match result {
            Ok(result) => Ok(result),
            Err(error) => Err(error),
        }
    }

    fn start_inner(&self, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let (configured_path, args, start_dir) = desktop_launch(&config)?;
        let path = platform::validate_application_path(&configured_path)?;
        if lock_recover(&self.state).pending.contains_key(&config.id) {
            return Err(
                "此应用已有尚未确认的原生实例；请先完成关联或等待该实例退出后重试。".into(),
            );
        }

        if let Some(binding) = self.store.application_binding(&config.id)? {
            match platform::inspect_identity(&binding.instance.identity) {
                Ok(Some(native))
                    if same_path(&native.path, &binding.target_path)
                        && same_path(&native.path, &path) =>
                {
                    return Ok(self.launch_result(&config.id, Vec::new()));
                }
                Ok(Some(_)) => {
                    return Err("原关联 PID 仍运行但目标路径不同，拒绝启动第二个实例。".into())
                }
                Err(error) => {
                    return Err(format!(
                        "无法核验原关联实例，已拒绝启动可能重复的应用：{error}"
                    ))
                }
                Ok(None) => {}
            }
        }

        // 内容启动器可能已经拿到创建身份，但还没能从系统读取应用路径。此时不能让
        // service.start 通过一次空快照再启动第二份同路径实例；待关联进程退出后，查询
        // 会自动清理这条内存记录，服务才可继续启动。
        if let Some(pending) = self.content_pending_for_path_locked(&path) {
            return Err(format!(
                "同一路径已有内容启动处于待关联状态，已拒绝重复启动：{}",
                pending.reason
            ));
        }

        let snapshot = self.snapshot_impl(true, true);
        if snapshot.stale {
            return Err(format!(
                "运行中应用快照已过期，无法确认是否已有同路径实例，已取消启动。{}",
                snapshot
                    .error
                    .map(|error| format!(" {error}"))
                    .unwrap_or_default()
            ));
        }
        let mut candidates = snapshot
            .applications
            .iter()
            .filter(|application| same_path(&application.path, &path))
            .cloned()
            .collect::<Vec<_>>();
        if candidates.len() > 1 {
            let reason = "同一路径存在多个运行实例，请在“运行中应用”中选定要关联的实例。";
            let identities = candidates
                .iter()
                .map(|candidate| candidate.instance.identity.clone())
                .collect();
            let runtime = self.set_pending_unknown(&config.id, &path, identities, reason);
            return Ok(ServiceLaunchResult {
                runtime,
                session: None,
                candidates: std::mem::take(&mut candidates),
            });
        }
        if let Some(candidate) = candidates.pop() {
            let binding = self.binding_from_discovered(&candidate, "managed")?;
            self.store.bind_application(&config.id, &binding)?;
            lock_recover(&self.state).pending.remove(&config.id);
            self.push_message(
                &config.id,
                format!(
                    "已复用同一路径运行实例 PID {}。",
                    candidate.instance.identity.pid
                ),
            );
            return Ok(self.launch_result(&config.id, Vec::new()));
        }

        let identity = platform::launch(&path, &args, &start_dir, &config.env)?;
        let native = match platform::inspect_identity(&identity) {
            Ok(Some(native)) if same_path(&native.path, &path) => native,
            Ok(Some(_)) => {
                let reason = "启动器交付的进程路径与目标应用不一致，已保留为待关联且不会自动接管。";
                let runtime = self.set_pending_unknown(&config.id, &path, vec![identity], reason);
                return Ok(ServiceLaunchResult {
                    runtime,
                    session: None,
                    candidates: Vec::new(),
                });
            }
            Ok(None) => {
                let reason = "应用已启动请求但原生应用实例尚不可核验；已保留待关联状态。";
                let runtime = self.set_pending_unknown(&config.id, &path, vec![identity], reason);
                return Ok(ServiceLaunchResult {
                    runtime,
                    session: None,
                    candidates: Vec::new(),
                });
            }
            Err(error) => {
                let reason = format!(
                    "应用已请求启动，但无法读取原生实例：{error}；请检查运行中应用并手动关联。"
                );
                let runtime = self.set_pending_unknown(&config.id, &path, vec![identity], &reason);
                return Ok(ServiceLaunchResult {
                    runtime,
                    session: None,
                    candidates: Vec::new(),
                });
            }
        };
        let application = self.discovered_from_native(&native);
        let binding = self.binding_from_discovered(&application, "managed")?;
        if let Err(error) = self.store.bind_application(&config.id, &binding) {
            let reason =
                format!("应用已启动，但保存跟踪关联失败：{error}；请在运行中应用中手动关联。");
            let runtime = self.set_pending_unknown(&config.id, &path, vec![identity], &reason);
            return Ok(ServiceLaunchResult {
                runtime,
                session: None,
                candidates: Vec::new(),
            });
        }
        lock_recover(&self.state).pending.remove(&config.id);
        self.push_message(&config.id, format!("已启动并关联 PID {}。", identity.pid));
        Ok(self.launch_result(&config.id, Vec::new()))
    }

    /// 仅请求应用正常退出。五秒后仍运行就保留跟踪并返回，不自动升级为强制终止。
    pub fn request_quit(&self, id: &str) -> Result<ServiceRuntime, String> {
        let _operation = lock_recover(&self.operations);
        self.request_quit_locked(id)
    }

    fn request_quit_locked(&self, id: &str) -> Result<ServiceRuntime, String> {
        let busy = self.begin_busy(id, "stopping")?;
        let (binding, instance, unlocked) = self.current_control_target(id)?;
        if let Err(error) = self.require_unlocked(id, &binding, &instance, unlocked) {
            drop(busy);
            return Err(error);
        }
        let mut stopping = self.runtime_or_stopped(id);
        stopping.status = "stopping".into();
        stopping.can_start = false;
        stopping.updated_at = Some(timestamp_now());
        self.set_runtime(stopping);
        if let Err(error) = platform::request_quit(&binding.instance.identity, &binding.target_path)
        {
            drop(busy);
            let _ = self.snapshot_impl(true, true);
            return Err(error);
        }

        let deadline = Instant::now() + QUIT_GRACE;
        while Instant::now() < deadline {
            match self.identity_still_matches(&binding) {
                Ok(false) => {
                    self.push_message(
                        id,
                        format!("PID {} 已正常退出。", binding.instance.identity.pid),
                    );
                    drop(busy);
                    let _ = self.snapshot_impl(true, true);
                    return Ok(self.runtime_or_stopped(id));
                }
                Err(error) => {
                    drop(busy);
                    let _ = self.snapshot_impl(true, true);
                    return Err(error);
                }
                Ok(true) => {}
            }
            thread::sleep(CONTROL_POLL);
        }

        self.push_message(
            id,
            "应用在 5 秒内未退出；仍在跟踪，可继续等待或单独确认强制结束。".into(),
        );
        drop(busy);
        let _ = self.snapshot_impl(true, true);
        let mut runtime = self.runtime_or_stopped(id);
        if runtime.status == "running" {
            runtime.error = Some("应用尚未退出；没有执行强制终止。".into());
            runtime.can_force_quit = self.can_force_current(&binding, &instance, unlocked);
            runtime.can_request_quit = true;
        }
        self.set_runtime(runtime.clone());
        Ok(runtime)
    }

    /// 强制结束是单独的显式操作；重新核验身份、路径、共享宿主与本会话保护状态。
    pub fn force_quit(&self, id: &str) -> Result<ServiceRuntime, String> {
        let _operation = lock_recover(&self.operations);
        self.force_quit_locked(id)
    }

    fn force_quit_locked(&self, id: &str) -> Result<ServiceRuntime, String> {
        let busy = self.begin_busy(id, "forceStopping")?;
        let (binding, instance, unlocked) = self.current_control_target(id)?;
        if let Err(error) = self.require_unlocked(id, &binding, &instance, unlocked) {
            drop(busy);
            return Err(error);
        }
        if !self.can_force_current(&binding, &instance, unlocked) {
            drop(busy);
            return Err(instance
                .control_reason
                .clone()
                .unwrap_or_else(|| "当前应用实例不允许强制结束。".into()));
        }
        let mut stopping = self.runtime_or_stopped(id);
        stopping.status = "stopping".into();
        stopping.can_start = false;
        stopping.updated_at = Some(timestamp_now());
        self.set_runtime(stopping);
        if let Err(error) = platform::force_quit(&binding.instance.identity, &binding.target_path) {
            drop(busy);
            let _ = self.snapshot_impl(true, true);
            return Err(error);
        }
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            match self.identity_still_matches(&binding) {
                Ok(false) => {
                    self.push_message(
                        id,
                        format!("已结束关联应用 PID {}。", binding.instance.identity.pid),
                    );
                    drop(busy);
                    let _ = self.snapshot_impl(true, true);
                    return Ok(self.runtime_or_stopped(id));
                }
                Err(error) => {
                    drop(busy);
                    let _ = self.snapshot_impl(true, true);
                    return Err(error);
                }
                Ok(true) => {}
            }
            thread::sleep(CONTROL_POLL);
        }
        drop(busy);
        let _ = self.snapshot_impl(true, true);
        Err("系统接受了结束请求，但进程仍可见；保留原关联供重试。".into())
    }

    pub fn focus(&self, id: &str) -> Result<(), String> {
        let _operation = lock_recover(&self.operations);
        self.focus_locked(id)
    }

    fn focus_locked(&self, id: &str) -> Result<(), String> {
        let (binding, instance, _unlocked) = self.current_control_target(id)?;
        if instance.shared_host {
            return Err("共享宿主只读，不允许对宿主执行激活操作。".into());
        }
        let application = platform::inspect_identity(&binding.instance.identity)?
            .ok_or_else(|| "原应用实例已退出或身份发生变化，拒绝激活。".to_owned())?;
        if !same_path(&application.path, &binding.target_path) {
            return Err("当前 PID 的应用路径与已关联目标不同，拒绝激活。".into());
        }
        if application.window_count == 0 {
            return Err("该应用当前没有可显示的窗口。".into());
        }
        platform::focus(&binding.instance.identity, &binding.target_path)
    }

    /// 先发正常退出请求；旧实例确实退出后才启动。取消保存提示或退出超时都不会并行启动。
    pub fn restart(&self, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let (configured_path, _, _) = desktop_launch(&config)?;
        let configured_path = platform::validate_application_path(&configured_path)?;
        let service_id = config.id.clone();
        let result = {
            let _operation = lock_recover(&self.operations);
            if let Some(binding) = self.store.application_binding(&service_id)? {
                if !same_path(&binding.target_path, &configured_path) {
                    return Err(
                        "当前配置的应用路径与关联运行实例不同；请先确认旧实例，再单独更新关联目标。"
                            .into(),
                    );
                }
                match platform::inspect_identity(&binding.instance.identity) {
                    Err(error) => return Err(format!("无法核验旧应用实例，已取消重启：{error}")),
                    Ok(Some(native)) if !same_path(&native.path, &binding.target_path) => {
                        return Err("旧 PID 仍运行但应用路径已改变，已取消重启。".into())
                    }
                    Ok(Some(_)) => {
                        let runtime = self.request_quit_locked(&service_id)?;
                        if runtime.status != "stopped" {
                            return Err("旧实例尚未确认退出；重启已取消，请先让应用正常退出，必要时单独确认强制结束。".into());
                        }
                    }
                    Ok(None) => {
                        // 只有原生接口明确确认旧创建身份不存在，才允许下一步启动。
                    }
                }
            } else {
                // 未关联但仍处于启动器待关联状态时，不能猜测旧实例已退出。
                let _ = self.snapshot_impl(true, true);
                if lock_recover(&self.state).pending.contains_key(&service_id) {
                    return Err("此服务有尚未确认的桌面应用实例；先从运行中应用完成关联，或等待实例退出后重试。".into());
                }
            }
            self.start_locked(config)
        };
        self.refresh();
        match result {
            Ok(mut result) => {
                result.runtime = self.runtime_or_stopped(&service_id);
                Ok(result)
            }
            Err(error) => Err(error),
        }
    }

    /// 把当前运行应用与既有桌面服务建立关联；身份与路径任一不符都不会保存。
    pub fn attach(&self, request: AttachApplicationRequest) -> Result<ServiceConfig, String> {
        let _operation = lock_recover(&self.operations);
        let path = platform::validate_application_path(&request.path)?;
        let native = platform::inspect_identity(&request.identity)?
            .ok_or_else(|| "该应用实例已退出或 PID 创建身份不匹配。".to_owned())?;
        if !same_path(&native.path, &path) {
            return Err("所选进程的规范化应用路径与目标不一致，拒绝接入。".into());
        }
        let discovered = self.discovered_from_native(&native);
        let binding = self.binding_from_discovered(&discovered, "external")?;
        let service = if let Some(id) = request.existing_service_id.as_deref() {
            let previous = self.store.service(id)?;
            if previous.kind != "desktopApp" {
                return Err("只能将运行中应用关联到桌面应用类型的服务。".into());
            }
            // 接入不是隐式解除旧关联：旧创建身份仍活动或无法核验时，不能把记录转给另一个实例。
            if let Some(current) = self.store.application_binding(id)? {
                if current.instance.identity != request.identity {
                    match platform::inspect_identity(&current.instance.identity) {
                        Ok(None) => {},
                        Ok(Some(_)) => return Err("该服务仍关联另一个运行中的应用实例，请先让旧实例退出；如需同时管理，请创建另一条记录。".into()),
                        Err(error) => return Err(format!("无法确认原关联实例已退出，已拒绝替换关联：{error}")),
                    }
                }
            }
            let mut service = previous.clone();
            service.name = non_empty(request.name, &service.name);
            service.notes = request.notes;
            service.working_dir = request.working_dir;
            let (args, start_dir) = match service.launch {
                LaunchConfig::DesktopApp {
                    args, start_dir, ..
                } => (args, start_dir),
                _ => (Vec::new(), String::new()),
            };
            service.launch = LaunchConfig::DesktopApp {
                path: path.clone(),
                args,
                start_dir,
            };
            service.source = "external".into();
            service.external = Some(discovered.instance.clone());
            let saved = self.store.save_service(service)?;
            if let Err(error) = self.store.bind_application(&saved.id, &binding) {
                let _ = self.store.save_service(previous);
                return Err(error);
            }
            saved
        } else {
            let config = ServiceConfig {
                id: String::new(),
                kind: "desktopApp".into(),
                launch: LaunchConfig::DesktopApp {
                    path: path.clone(),
                    args: Vec::new(),
                    start_dir: String::new(),
                },
                source: "external".into(),
                external: Some(discovered.instance.clone()),
                name: non_empty(request.name, &native.name),
                description: String::new(),
                working_dir: request.working_dir,
                environment_type: "system".into(),
                environment_dir: String::new(),
                executable_dirs: Vec::new(),
                env: BTreeMap::new(),
                command: String::new(),
                shell: String::new(),
                expected_ports: Vec::new(),
                notes: request.notes,
                created_at: String::new(),
                updated_at: String::new(),
            };
            let saved = self.store.save_service(config)?;
            if let Err(error) = self.store.bind_application(&saved.id, &binding) {
                let _ = self.store.delete_service(&saved.id);
                return Err(error);
            }
            saved
        };
        // attach 明确接管了这个原生实例；它不再是“内容启动后待关联”，否则同一路径
        // 的内容状态会一直 unknown，且后续启动会被错误地阻塞。
        self.clear_content_pending_identity_locked(&request.identity);
        lock_recover(&self.state).pending.remove(&service.id);
        self.push_message(
            &service.id,
            format!(
                "已关联运行中应用 PID {}；删除记录不会关闭应用。",
                request.identity.pid
            ),
        );
        let _ = self.snapshot_impl(true, true);
        Ok(service)
    }

    /// 解除系统/未知应用的当前实例保护；退出或 PID 创建时间改变后自动失效。
    pub fn unlock(&self, id: &str) -> Result<ServiceRuntime, String> {
        let _operation = lock_recover(&self.operations);
        let binding = self
            .store
            .application_binding(id)?
            .ok_or_else(|| "该桌面应用服务尚未关联运行实例。".to_owned())?;
        let current = platform::inspect_identity(&binding.instance.identity)?
            .ok_or_else(|| "原应用实例已退出或 PID 身份已变化，不能解除保护。".to_owned())?;
        if !same_path(&current.path, &binding.target_path) {
            return Err("应用路径与关联目标不一致，不能解除保护。".into());
        }
        let current_instance = self.discovered_from_native(&current).instance;
        if current_instance.shared_host {
            return Err("共享宿主只读，不允许解除保护。".into());
        }
        if !matches!(current_instance.category.as_str(), "system" | "unknown") {
            return Err("该应用无需解除系统/未知分类保护。".into());
        }
        lock_recover(&self.state)
            .unlocked
            .insert(id.into(), binding.instance.identity.clone());
        self.push_message(
            id,
            "已对当前应用实例临时解除保护；应用退出后自动失效。".into(),
        );
        let _ = self.snapshot_impl(true, true);
        Ok(self.runtime_or_stopped(id))
    }

    /// 删除记录只删除本地身份关联；不会向仍运行的桌面应用发送信号。
    pub fn forget(&self, id: &str) {
        {
            let _operation = lock_recover(&self.operations);
            if let Err(error) = self.store.clear_application_binding(id) {
                self.push_message(id, format!("解除应用关联失败：{error}"));
                return;
            }
            let mut state = lock_recover(&self.state);
            state.unlocked.remove(id);
            state.pending.remove(id);
            state.runtimes.remove(id);
            state.last_messages.remove(id);
        }
        self.refresh();
    }

    /// 供端口发现层标记属于已关联桌面应用的 PID；仅返回当前完整身份仍匹配的主进程。
    pub fn managed_processes(&self) -> HashMap<u32, String> {
        let Ok(bindings) = self.store.application_bindings() else {
            return HashMap::new();
        };
        bindings
            .into_iter()
            .filter_map(|(id, binding)| {
                platform::inspect_identity(&binding.instance.identity)
                    .ok()
                    .flatten()
                    .filter(|application| same_path(&application.path, &binding.target_path))
                    .map(|_| (binding.instance.identity.pid, id))
            })
            .collect()
    }

    fn process_family(
        &self,
        identity: &ProcessIdentity,
        instance: &ExternalInstance,
        snapshot: &crate::models::DiscoverySnapshot,
    ) -> Vec<ProcessIdentity> {
        // 发现器只把已核验的直接父子进程族写入 familyIdentity；这里仅用于端口归组和展示，
        // 绝不据此扩大退出、强制结束或持久化关联的进程范围。
        let family_root = snapshot
            .processes
            .iter()
            .find(|process| process.instance.identity == *identity)
            .and_then(|process| process.family_identity.as_ref())
            .unwrap_or(identity);
        let mut family = snapshot
            .processes
            .iter()
            .filter(|process| {
                process
                    .family_identity
                    .as_ref()
                    .map(|root| root == family_root)
                    .unwrap_or(&process.instance.identity == family_root)
            })
            .map(|process| process.instance.identity.clone())
            .collect::<Vec<_>>();
        if !family.iter().any(|member| member == identity) {
            family.push(identity.clone());
        }
        family.sort_by_key(|member| member.pid);
        family.dedup();
        if family.is_empty() {
            let _ = instance;
            vec![identity.clone()]
        } else {
            family
        }
    }

    /// 未能确定唯一关联身份时保持 unknown；只有每个候选都经原生接口确认退出后才回到 stopped。
    fn pending_runtime(
        &self,
        service_id: &str,
        pending: &PendingAssociation,
        applications: &[DiscoveredApplication],
        now: &str,
    ) -> ServiceRuntime {
        let mut live = Vec::new();
        let mut uncertain = false;
        for identity in &pending.identities {
            match platform::inspect_identity(identity) {
                Ok(Some(_)) => live.push(identity.clone()),
                Ok(None) => {}
                Err(_) => {
                    uncertain = true;
                    live.push(identity.clone());
                }
            }
        }

        if live.is_empty() && !uncertain {
            let mut runtime = ServiceRuntime::stopped(service_id);
            runtime.updated_at = Some(now.to_owned());
            runtime.messages = lock_recover(&self.state)
                .last_messages
                .get(service_id)
                .cloned()
                .unwrap_or_default();
            return runtime;
        }

        let identity = (live.len() == 1).then(|| live[0].clone());
        let visible_exact_candidate = live.iter().any(|identity| {
            applications.iter().any(|application| {
                application.instance.identity == *identity
                    && same_path(&application.path, &pending.target_path)
            })
        });
        let mut runtime = unknown_runtime(service_id, identity.as_ref(), &pending.reason, now);
        runtime.stale = uncertain || !visible_exact_candidate;
        runtime.messages = lock_recover(&self.state)
            .last_messages
            .get(service_id)
            .cloned()
            .unwrap_or_default();
        runtime
    }

    /// 在应用全局操作锁内清理并查询内容启动待关联记录。每个身份都重新通过原生接口
    /// 核验：已确认归属的暂存身份明确不存在才清除；启动器转交未核验时始终保留，
    /// 因为启动器 PID 已退出并不能证明它交给的另一个应用也退出。
    fn content_pending_for_path_locked(&self, target_path: &str) -> Option<ContentPending> {
        let entries = lock_recover(&self.state).content_pending.clone();
        let mut resolved = Vec::new();
        let mut replacements = Vec::new();
        let mut found = None;

        for (content_id, pending) in entries {
            if !same_path(&pending.target_path, target_path) {
                continue;
            }
            let mut remaining = Vec::new();
            for identity in &pending.identities {
                match platform::inspect_identity(identity) {
                    Ok(None) => {}
                    // 路径不一致也要保留身份：PID 创建身份仍存活时，不能假设原启动
                    // 已失败并再次创建同路径实例；后续由用户显式关联或等待退出。
                    Ok(Some(_)) | Err(_) => remaining.push(identity.clone()),
                }
            }
            if remaining.is_empty() && !pending.retain_until_associated {
                resolved.push(content_id);
                continue;
            }
            let mut next = pending.clone();
            next.identities = remaining.clone();
            replacements.push((content_id.clone(), remaining));
            if found.is_none() {
                found = Some(ContentPending {
                    target_path: next.target_path,
                    identities: next.identities,
                    reason: next.reason,
                });
            }
        }

        if !resolved.is_empty() || !replacements.is_empty() {
            let mut state = lock_recover(&self.state);
            for content_id in resolved {
                state.content_pending.remove(&content_id);
            }
            for (content_id, identities) in replacements {
                if let Some(pending) = state.content_pending.get_mut(&content_id) {
                    pending.identities = identities;
                }
            }
        }
        found
    }

    fn set_content_pending(
        &self,
        content_id: &str,
        target_path: &str,
        identities: Vec<ProcessIdentity>,
        reason: &str,
        retain_until_associated: bool,
    ) -> ContentPending {
        let pending = PendingAssociation {
            retain_until_associated,
            target_path: target_path.to_owned(),
            identities: identities.clone(),
            reason: reason.to_owned(),
        };
        lock_recover(&self.state)
            .content_pending
            .insert(content_id.to_owned(), pending.clone());
        ContentPending {
            target_path: pending.target_path,
            identities: pending.identities,
            reason: pending.reason,
        }
    }

    fn clear_content_pending_identity_locked(&self, identity: &ProcessIdentity) {
        let mut state = lock_recover(&self.state);
        state.content_pending.retain(|_, pending| {
            let matched = pending
                .identities
                .iter()
                .any(|candidate| candidate == identity);
            pending.identities.retain(|candidate| candidate != identity);
            !matched && (pending.retain_until_associated || !pending.identities.is_empty())
        });
    }

    fn set_pending_unknown(
        &self,
        service_id: &str,
        target_path: &str,
        identities: Vec<ProcessIdentity>,
        reason: &str,
    ) -> ServiceRuntime {
        let pending = PendingAssociation {
            retain_until_associated: false,
            target_path: target_path.to_owned(),
            identities: identities.clone(),
            reason: reason.to_owned(),
        };
        let identity = (identities.len() == 1).then(|| identities[0].clone());
        let mut runtime = unknown_runtime(service_id, identity.as_ref(), reason, &timestamp_now());
        // 多个精确可见实例属于选择歧义，不是扫描过期；启动器身份不明则保留过期标志。
        runtime.stale = identities.len() < 2;
        {
            let mut state = lock_recover(&self.state);
            runtime.messages = state
                .last_messages
                .get(service_id)
                .cloned()
                .unwrap_or_default();
            state.pending.insert(service_id.to_owned(), pending);
            state
                .runtimes
                .insert(service_id.to_owned(), runtime.clone());
        }
        self.emit_runtime(&runtime);
        runtime
    }

    /// 扫描或存储失败时，保留最后的 PID 和端口，但撤销所有可能触达进程的能力。
    fn mark_snapshot_failed(&self, error: String) -> ApplicationSnapshot {
        let bindings = self.store.application_bindings().unwrap_or_default();
        let now = timestamp_now();
        let (snapshot, runtimes) = {
            let mut state = lock_recover(&self.state);
            state.snapshot.stale = true;
            state.snapshot.error = Some(error.clone());

            let busy = state.busy.keys().cloned().collect::<HashSet<_>>();
            for (service_id, binding) in bindings {
                if busy.contains(&service_id) {
                    continue;
                }
                let mut runtime =
                    unknown_runtime(&service_id, Some(&binding.instance.identity), &error, &now);
                if let Some(previous) = state.runtimes.get(&service_id) {
                    runtime.ports = previous.ports.clone();
                    runtime.messages = previous.messages.clone();
                    runtime.protection_unlocked = previous.protection_unlocked;
                    runtime.started_at = previous.started_at.clone();
                }
                state.runtimes.insert(service_id, runtime);
            }

            for (service_id, runtime) in &mut state.runtimes {
                if busy.contains(service_id) || runtime.status == "stopped" {
                    continue;
                }
                runtime.status = "unknown".into();
                runtime.stale = true;
                runtime.can_start = false;
                runtime.can_restart = false;
                runtime.can_stop = false;
                runtime.can_unlock = false;
                runtime.can_convert = false;
                runtime.can_focus = false;
                runtime.can_request_quit = false;
                runtime.can_force_quit = false;
                runtime.error = Some(error.clone());
                runtime.control_reason = Some(error.clone());
                runtime.updated_at = Some(now.clone());
                for action in ["start", "restart", "focus", "requestQuit", "forceQuit"] {
                    runtime.action_reasons.insert(action.into(), error.clone());
                }
            }

            (
                state.snapshot.clone(),
                state.runtimes.values().cloned().collect::<Vec<_>>(),
            )
        };
        self.emit_snapshot(&snapshot);
        for runtime in runtimes {
            self.emit_runtime(&runtime);
        }
        snapshot
    }

    fn begin_busy(&self, service_id: &str, operation: &str) -> Result<BusyGuard<'_>, String> {
        let mut state = lock_recover(&self.state);
        if let Some(active) = state.busy.get(service_id) {
            return Err(format!("该应用正在执行“{active}”，请稍后重试。"));
        }
        state
            .busy
            .insert(service_id.to_owned(), operation.to_owned());
        Ok(BusyGuard {
            state: &self.state,
            service_id: service_id.to_owned(),
        })
    }

    fn make_runtime(
        &self,
        service_id: &str,
        binding: Option<&ApplicationBinding>,
        instance: Option<&ExternalInstance>,
        ports: Vec<PortInfo>,
        unlocked: bool,
        window_count: u32,
        port_stale: bool,
        now: Option<&String>,
    ) -> ServiceRuntime {
        let mut runtime = ServiceRuntime::stopped(service_id);
        runtime.status = "running".into();
        runtime.updated_at = now.cloned().or_else(|| Some(timestamp_now()));
        runtime.started_at = binding.map(|binding| binding.bound_at.clone());
        runtime.run_id = binding.map(|binding| {
            format!(
                "application:{}:{}",
                binding.instance.identity.pid, binding.instance.identity.created_at
            )
        });
        runtime.pid = binding.map(|binding| binding.instance.identity.pid);
        let previous = self.runtime_or_stopped(service_id);
        runtime.ports =
            if port_stale && previous.run_id == runtime.run_id && !previous.ports.is_empty() {
                previous.ports
            } else {
                ports
            };
        runtime.stale = port_stale;
        runtime.can_start = false;
        runtime.can_stop = false;
        runtime.can_convert = false;
        runtime.can_unlock = binding
            .zip(instance)
            .map(|(binding, instance)| {
                !unlocked
                    && !instance.shared_host
                    && matches!(instance.category.as_str(), "system" | "unknown")
                    && binding.instance.identity.created_at.len() > 0
            })
            .unwrap_or(false);
        runtime.protection_unlocked = unlocked;
        runtime.control_reason = instance.and_then(|instance| {
            if instance.shared_host {
                Some("共享宿主只读，不能对宿主进程执行退出操作。".into())
            } else if matches!(instance.category.as_str(), "system" | "unknown") && !unlocked {
                Some("系统或待确认应用默认只读；需对当前实例临时解除保护。".into())
            } else {
                instance.control_reason.clone()
            }
        });
        let may_control = binding
            .zip(instance)
            .map(|(_, instance)| {
                !instance.shared_host
                    && instance.controllable
                    && (!matches!(instance.category.as_str(), "system" | "unknown") || unlocked)
            })
            .unwrap_or(false);
        runtime.can_restart = may_control;
        let shared_host = instance
            .map(|instance| instance.shared_host)
            .unwrap_or(true);
        runtime.can_focus = binding.is_some() && window_count > 0 && !shared_host;
        runtime.can_request_quit = may_control && platform::can_request_quit(window_count);
        runtime.can_force_quit = may_control;
        if !runtime.can_request_quit {
            runtime.action_reasons.insert(
                "requestQuit".into(),
                runtime
                    .control_reason
                    .clone()
                    .unwrap_or_else(|| "当前用户无法控制此应用实例。".into()),
            );
        }
        if !runtime.can_force_quit {
            runtime.action_reasons.insert(
                "forceQuit".into(),
                runtime
                    .control_reason
                    .clone()
                    .unwrap_or_else(|| "当前用户无法强制结束此应用实例。".into()),
            );
        }
        if !runtime.can_restart {
            runtime.action_reasons.insert(
                "restart".into(),
                runtime
                    .control_reason
                    .clone()
                    .unwrap_or_else(|| "当前应用实例不允许重启。".into()),
            );
        }
        if runtime.can_focus {
            runtime.action_reasons.insert(
                "focus".into(),
                "激活操作会由操作系统决定是否允许切换前台。".into(),
            );
        } else {
            runtime.action_reasons.insert(
                "focus".into(),
                if window_count == 0 {
                    "该应用当前没有可显示的窗口。".into()
                } else {
                    runtime
                        .control_reason
                        .clone()
                        .unwrap_or_else(|| "当前应用只读，不能执行激活操作。".into())
                },
            );
        }
        if !runtime.can_request_quit {
            runtime.action_reasons.insert(
                "requestQuit".into(),
                if may_control {
                    "该应用当前没有可发送正常关闭请求的窗口。".into()
                } else {
                    runtime
                        .control_reason
                        .clone()
                        .unwrap_or_else(|| "当前用户无法请求应用正常退出。".into())
                },
            );
        }
        runtime.messages = lock_recover(&self.state)
            .last_messages
            .get(service_id)
            .cloned()
            .unwrap_or_default();
        runtime
    }

    fn discovered_from_native(&self, native: &NativeApplication) -> DiscoveredApplication {
        let mut instance = self
            .discovery
            .inspect(&native.identity)
            .ok()
            .flatten()
            .unwrap_or_else(|| unknown_instance(native));
        instance.name = native.name.clone();
        instance.executable =
            platform::application_executable(&native.path).or(instance.executable);
        DiscoveredApplication {
            instance,
            path: native.path.clone(),
            bundle_id: native.bundle_id.clone(),
            window_count: native.window_count,
            managed_service_id: None,
            members: vec![native.identity.clone()],
        }
    }

    fn binding_from_discovered(
        &self,
        application: &DiscoveredApplication,
        origin: &str,
    ) -> Result<ApplicationBinding, String> {
        if application.instance.identity.created_at.trim().is_empty() {
            return Err("缺少原生 PID 创建身份，不能保存桌面应用关联。".into());
        }
        Ok(ApplicationBinding {
            instance: application.instance.clone(),
            target_path: application.path.clone(),
            bundle_id: application.bundle_id.clone(),
            origin: origin.into(),
            bound_at: timestamp_now(),
        })
    }

    fn current_control_target(
        &self,
        id: &str,
    ) -> Result<(ApplicationBinding, ExternalInstance, bool), String> {
        let binding = self
            .store
            .application_binding(id)?
            .ok_or_else(|| "该桌面应用服务没有关联的运行实例。".to_owned())?;
        let native = platform::inspect_identity(&binding.instance.identity)?
            .ok_or_else(|| "关联的应用实例已退出或 PID 身份已变化。".to_owned())?;
        if !same_path(&native.path, &binding.target_path) {
            return Err("当前进程的应用路径与关联目标不同，已拒绝控制。".into());
        }
        let instance = self
            .discovery
            .inspect(&binding.instance.identity)?
            .ok_or_else(|| "无法重新核验应用进程分类与权限。".to_owned())?;
        let unlocked = lock_recover(&self.state)
            .unlocked
            .get(id)
            .map(|identity| identity == &binding.instance.identity)
            .unwrap_or(false);
        Ok((binding, instance, unlocked))
    }

    fn require_unlocked(
        &self,
        _id: &str,
        binding: &ApplicationBinding,
        instance: &ExternalInstance,
        unlocked: bool,
    ) -> Result<(), String> {
        if instance.shared_host {
            return Err("共享宿主只读，不能对宿主进程执行退出操作。".into());
        }
        if matches!(instance.category.as_str(), "system" | "unknown") && !unlocked {
            return Err("系统或待确认应用默认只读，请先解除当前实例保护。".into());
        }
        if !instance.controllable {
            return Err(instance
                .control_reason
                .clone()
                .unwrap_or_else(|| "当前用户没有控制此应用的权限。".into()));
        }
        if binding.instance.identity.created_at.trim().is_empty() {
            return Err("关联记录缺少进程创建身份，已拒绝控制。".into());
        }
        Ok(())
    }

    fn can_force_current(
        &self,
        binding: &ApplicationBinding,
        instance: &ExternalInstance,
        unlocked: bool,
    ) -> bool {
        binding.instance.identity.created_at.len() > 0
            && !instance.shared_host
            && instance.controllable
            && (!matches!(instance.category.as_str(), "system" | "unknown") || unlocked)
    }

    fn identity_still_matches(&self, binding: &ApplicationBinding) -> Result<bool, String> {
        match platform::inspect_identity(&binding.instance.identity)? {
            None => Ok(false),
            Some(native) if same_path(&native.path, &binding.target_path) => Ok(true),
            Some(_) => {
                Err("PID 创建身份仍存在，但可执行路径已改变；不能把它当作旧应用退出。".into())
            }
        }
    }

    fn launch_result(
        &self,
        id: &str,
        candidates: Vec<DiscoveredApplication>,
    ) -> ServiceLaunchResult {
        let runtime = self.runtime_or_stopped(id);
        ServiceLaunchResult {
            runtime,
            session: None,
            candidates,
        }
    }

    fn runtime_or_stopped(&self, id: &str) -> ServiceRuntime {
        lock_recover(&self.state)
            .runtimes
            .get(id)
            .cloned()
            .unwrap_or_else(|| ServiceRuntime::stopped(id))
    }

    fn set_runtime(&self, runtime: ServiceRuntime) {
        lock_recover(&self.state)
            .runtimes
            .insert(runtime.service_id.clone(), runtime.clone());
        self.emit_runtime(&runtime);
    }

    fn push_message(&self, id: &str, message: String) {
        let runtime = {
            let mut state = lock_recover(&self.state);
            let messages = {
                let messages = state.last_messages.entry(id.into()).or_default();
                messages.push(message);
                if messages.len() > MAX_OPERATION_MESSAGES {
                    let excess = messages.len() - MAX_OPERATION_MESSAGES;
                    messages.drain(0..excess);
                }
                messages.clone()
            };
            let runtime = state
                .runtimes
                .entry(id.into())
                .or_insert_with(|| ServiceRuntime::stopped(id));
            runtime.messages = messages;
            runtime.updated_at = Some(timestamp_now());
            runtime.clone()
        };
        self.emit_runtime(&runtime);
    }

    fn emit_snapshot(&self, snapshot: &ApplicationSnapshot) {
        if let Some(app) = &self.app {
            let _ = app.emit("application-snapshot", snapshot);
        }
    }

    fn emit_runtime(&self, runtime: &ServiceRuntime) {
        if let Some(app) = &self.app {
            let _ = app.emit("service-runtime", runtime);
        }
    }
}

/// 供 Tauri 命令层在不构造 manager 的场景复用路径校验。
pub fn inspect_path(path: &str) -> Result<ApplicationInspection, String> {
    ApplicationManager::inspect_path(path)
}

struct BusyGuard<'a> {
    state: &'a Mutex<ManagerState>,
    service_id: String,
}

impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        lock_recover(self.state).busy.remove(&self.service_id);
    }
}

fn unknown_runtime(
    service_id: &str,
    identity: Option<&ProcessIdentity>,
    reason: &str,
    now: &str,
) -> ServiceRuntime {
    let mut runtime = ServiceRuntime::stopped(service_id);
    runtime.status = "unknown".into();
    runtime.pid = identity.map(|identity| identity.pid);
    runtime.run_id =
        identity.map(|identity| format!("application:{}:{}", identity.pid, identity.created_at));
    runtime.updated_at = Some(now.to_owned());
    runtime.stale = true;
    runtime.can_start = false;
    runtime.can_restart = false;
    runtime.can_stop = false;
    runtime.error = Some(reason.to_owned());
    runtime.control_reason = Some(reason.to_owned());
    runtime
        .action_reasons
        .insert("start".into(), "实例尚未确认，避免重复启动。".into());
    runtime.action_reasons.insert(
        "restart".into(),
        "只有明确确认旧实例退出后才能重启。".into(),
    );
    runtime
        .action_reasons
        .insert("focus".into(), reason.to_owned());
    runtime
        .action_reasons
        .insert("requestQuit".into(), reason.to_owned());
    runtime
        .action_reasons
        .insert("forceQuit".into(), reason.to_owned());
    runtime
}

fn same_binding_sets(
    left: &[(String, ApplicationBinding)],
    right: &[(String, ApplicationBinding)],
) -> bool {
    fn signature(
        bindings: &[(String, ApplicationBinding)],
    ) -> Vec<(String, u32, String, String, Option<String>, String, String)> {
        let mut values = bindings
            .iter()
            .map(|(id, binding)| {
                (
                    id.clone(),
                    binding.instance.identity.pid,
                    binding.instance.identity.created_at.clone(),
                    normalize_path(&binding.target_path),
                    binding.bundle_id.clone(),
                    binding.origin.clone(),
                    binding.bound_at.clone(),
                )
            })
            .collect::<Vec<_>>();
        values.sort();
        values
    }

    signature(left) == signature(right)
}

fn desktop_launch(config: &ServiceConfig) -> Result<(String, Vec<String>, String), String> {
    if config.kind != "desktopApp" {
        return Err("此操作只适用于桌面应用服务。".into());
    }
    let LaunchConfig::DesktopApp {
        path,
        args,
        start_dir,
    } = &config.launch
    else {
        return Err("桌面应用缺少可执行目标配置。".into());
    };
    if config.id.trim().is_empty() {
        return Err("请先保存桌面应用服务配置，再启动应用。".into());
    }
    Ok((path.clone(), args.clone(), start_dir.clone()))
}

/// 比较桌面服务的实际启动目标。路径使用规范化比较，参数和工作目录也属于目标；
/// 只有名称、说明等展示字段变化时，运行中的实例才允许继续使用原关联。
fn desktop_service_target_changed(previous: &ServiceConfig, requested: &ServiceConfig) -> bool {
    match (&previous.launch, &requested.launch) {
        (
            LaunchConfig::DesktopApp {
                path: previous_path,
                args: previous_args,
                start_dir: previous_start_dir,
            },
            LaunchConfig::DesktopApp {
                path: requested_path,
                args: requested_args,
                start_dir: requested_start_dir,
            },
        ) => {
            !same_path(previous_path, requested_path)
                || previous_args != requested_args
                || previous_start_dir != requested_start_dir
        }
        _ => true,
    }
}

fn ports_for_members(
    members: &[ProcessIdentity],
    processes: &[crate::models::DiscoveredProcess],
) -> Vec<PortInfo> {
    let member_ids = members
        .iter()
        .map(|member| (member.pid, member.created_at.as_str()))
        .collect::<HashSet<_>>();
    let mut ports = processes
        .iter()
        .filter(|process| {
            member_ids.contains(&(
                process.instance.identity.pid,
                process.instance.identity.created_at.as_str(),
            ))
        })
        .flat_map(|process| process.ports.iter().cloned())
        .collect::<Vec<_>>();
    ports.sort_by(|left, right| {
        left.protocol
            .cmp(&right.protocol)
            .then(left.port.cmp(&right.port))
            .then(left.address.cmp(&right.address))
    });
    ports.dedup();
    ports
}

fn unknown_instance(application: &NativeApplication) -> ExternalInstance {
    ExternalInstance {
        identity: application.identity.clone(),
        name: application.name.clone(),
        executable: platform::application_executable(&application.path),
        working_dir: None,
        category: "unknown".into(),
        reasons: vec!["无法核实应用的系统组件来源，默认保护。".into()],
        shared_host: false,
        controllable: false,
        control_reason: Some("无法复核该应用的原生进程权限，当前只允许查看。".into()),
    }
}

fn same_path(left: &str, right: &str) -> bool {
    let left = normalize_path(left);
    let right = normalize_path(right);
    #[cfg(windows)]
    {
        left.eq_ignore_ascii_case(&right)
    }
    #[cfg(not(windows))]
    {
        left == right
    }
}

fn normalize_path(path: &str) -> String {
    let canonical = std::fs::canonicalize(path)
        .unwrap_or_else(|_| PathBuf::from(path))
        .to_string_lossy()
        .into_owned();
    #[cfg(windows)]
    {
        canonical.trim_end_matches(['\\', '/']).to_owned()
    }
    #[cfg(not(windows))]
    {
        canonical
    }
}

fn parse_timestamp(value: &str) -> Option<chrono::DateTime<Utc>> {
    chrono::DateTime::parse_from_rfc3339(value)
        .ok()
        .map(|parsed| parsed.with_timezone(&Utc))
}

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// 配置版本同时保留可读的 updatedAt，并附加完整配置摘要，避免同一毫秒内两次
/// 保存得到相同时间戳而让旧确认继续有效。摘要只放进版本戳，不把环境变量等配置
/// 内容返回给前端。
fn service_config_version(service: &ServiceConfig) -> String {
    let mut hasher = Sha1::new();
    if let Ok(encoded) = serde_json::to_vec(service) {
        hasher.update(encoded);
    }
    format!("{}:{:x}", service.updated_at, hasher.finalize())
}

fn non_empty(value: String, fallback: &str) -> String {
    if value.trim().is_empty() {
        fallback.into()
    } else {
        value.trim().to_owned()
    }
}

fn lock_recover<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

mod platform {
    #[cfg(target_os = "macos")]
    pub(super) use super::macos::*;
    #[cfg(not(any(target_os = "macos", windows)))]
    pub(super) use super::unsupported::*;
    #[cfg(windows)]
    pub(super) use super::windows::*;
}

#[cfg(test)]
mod content_pending_tests {
    use super::*;

    #[test]
    fn exited_launcher_does_not_clear_unverified_handoff() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&directory.path().join("pending.sqlite3")).unwrap());
        let manager = ApplicationManager::new(store, DiscoveryManager::new(), None);
        // 使用本进程 PID 和故意不匹配的创建标识，只做只读核验，不控制任何真实进程。
        let old_identity = ProcessIdentity {
            pid: std::process::id(),
            created_at: "并非当前实例".into(),
        };
        let path = directory
            .path()
            .join("待关联.app")
            .to_string_lossy()
            .into_owned();
        manager.set_content_pending(
            "launcher",
            &path,
            vec![old_identity],
            "启动器转交未核验",
            true,
        );
        assert!(manager.content_pending_for_path_locked(&path).is_some());
        manager.clear_content_pending_identity(&ProcessIdentity {
            pid: 1,
            created_at: "无关身份".into(),
        });
        assert!(manager.content_pending_for_path_locked(&path).is_some());
        manager.clear_content_pending("launcher");
        assert!(manager.content_pending_for_path_locked(&path).is_none());
    }
}
