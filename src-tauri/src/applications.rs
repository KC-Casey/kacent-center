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
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
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
    /// 防止后台快照在一次显式启动/退出控制进行中覆盖其运行态。
    busy: HashMap<String, String>,
    last_messages: HashMap<String, Vec<String>>,
    monitor_started: bool,
}

#[derive(Clone)]
struct PendingAssociation {
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
                // 没有已关联的应用时不在后台全机扫描；发现页通过 snapshot 命令按需刷新。
                if manager
                    .store
                    .application_bindings()
                    .map(|bindings| !bindings.is_empty())
                    .unwrap_or(false)
                {
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

    fn set_pending_unknown(
        &self,
        service_id: &str,
        target_path: &str,
        identities: Vec<ProcessIdentity>,
        reason: &str,
    ) -> ServiceRuntime {
        let pending = PendingAssociation {
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
