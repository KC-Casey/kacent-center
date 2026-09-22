//! 外部实例只保存观察关联，不持有或伪造 PTY、Child、进程组或 Job Object。
//! 退出和删除关联永远不会调用外部终止；只有明确的停止/转换请求会进入 terminate。
use crate::{discovery::DiscoveryManager, models::*, runtime::RuntimeManager, store::Store};
use chrono::{SecondsFormat, Utc};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};
use tauri::{AppHandle, Emitter};

#[derive(Default)]
struct ExternalState {
    runtimes: HashMap<String, ServiceRuntime>,
    unlocked: HashSet<ProcessIdentity>,
    busy: HashSet<String>,
}
pub struct ExternalManager {
    store: Arc<Store>,
    runtime: Arc<RuntimeManager>,
    discovery: Arc<DiscoveryManager>,
    app: Option<AppHandle>,
    state: Mutex<ExternalState>,
    attach_lock: Mutex<()>,
    refresh_lock: Mutex<()>,
}
struct BusyGuard<'a> {
    manager: &'a ExternalManager,
    id: String,
}
impl Drop for BusyGuard<'_> {
    fn drop(&mut self) {
        self.manager
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .busy
            .remove(&self.id);
    }
}
impl ExternalManager {
    pub fn new(
        store: Arc<Store>,
        runtime: Arc<RuntimeManager>,
        app: Option<AppHandle>,
    ) -> Arc<Self> {
        Arc::new(Self {
            discovery: runtime.discovery(),
            store,
            runtime,
            app,
            state: Mutex::new(ExternalState::default()),
            attach_lock: Mutex::new(()),
            refresh_lock: Mutex::new(()),
        })
    }
    pub fn start_monitor(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || loop {
            let Some(manager) = weak.upgrade() else {
                break;
            };
            manager.refresh();
            drop(manager);
            thread::sleep(Duration::from_secs(2));
        });
    }
    fn busy(&self, id: &str) -> Result<BusyGuard<'_>, String> {
        let mut state = self.state.lock().map_err(|_| "外部实例状态不可用")?;
        if !state.busy.insert(id.into()) {
            return Err("该外部实例正在操作，请稍后重试。".into());
        }
        Ok(BusyGuard {
            manager: self,
            id: id.into(),
        })
    }
    fn emit(&self, runtime: &ServiceRuntime) {
        if let Some(app) = &self.app {
            let _ = app.emit("service-runtime", runtime);
        }
    }
    pub fn runtimes(&self) -> Vec<ServiceRuntime> {
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .values()
            .cloned()
            .collect()
    }
    pub fn snapshot(&self, force: bool) -> DiscoverySnapshot {
        let mut snapshot = self.discovery.snapshot(force);
        let mut owners = self.runtime.managed_processes();
        if let Ok(services) = self.store.services() {
            for service in services {
                if service.source == "external" && service.kind != "desktopApp" {
                    if let Some(external) = service.external {
                        // 快照的 PID 相同但创建时间不同，不能继承旧关联。
                        if snapshot
                            .processes
                            .iter()
                            .any(|p| p.instance.identity == external.identity)
                        {
                            owners.entry(external.identity.pid).or_insert(service.id);
                        }
                    }
                }
            }
        }
        for process in &mut snapshot.processes {
            process.managed_service_id = owners.get(&process.instance.identity.pid).cloned();
        }
        snapshot
    }
    pub fn attach(&self, request: AttachExternalRequest) -> Result<ServiceConfig, String> {
        let _attach = self.attach_lock.lock().map_err(|_| "接入操作锁不可用")?;
        if request.identity.created_at.is_empty() || request.identity.pid == 0 {
            return Err("无法核验进程创建身份，仅支持查看端口。".into());
        }
        if self
            .runtime
            .managed_processes()
            .contains_key(&request.identity.pid)
        {
            return Err("该进程已属于平台创建的会话，请打开已有服务。".into());
        }
        let current = self
            .discovery
            .inspect(&request.identity)?
            .ok_or("原进程已退出或身份已改变，请刷新后重新选择。")?;
        let mut service = if let Some(id) = &request.existing_service_id {
            let existing = self.store.service(id)?;
            if existing.source != "external" || existing.kind == "desktopApp" {
                return Err("只能重新关联外部服务记录。".into());
            }
            if let Some(old) = &existing.external {
                if old.identity != request.identity
                    && self.discovery.inspect(&old.identity)?.is_some()
                {
                    return Err("该记录原来的进程仍在运行，不能替换关联。".into());
                }
            }
            existing
        } else {
            ServiceConfig {
                id: String::new(),
                kind: "command".into(),
                launch: LaunchConfig::Command,
                source: "external".into(),
                external: None,
                name: String::new(),
                description: String::new(),
                working_dir: String::new(),
                environment_type: "system".into(),
                environment_dir: String::new(),
                executable_dirs: vec![],
                env: BTreeMap::new(),
                command: String::new(),
                shell: String::new(),
                expected_ports: vec![],
                notes: String::new(),
                created_at: String::new(),
                updated_at: String::new(),
            }
        };
        let guard = self.busy(&service.id)?;
        let old_identity = service.external.as_ref().map(|e| e.identity.clone());
        service.external = Some(current);
        service.name = request.name;
        service.notes = request.notes;
        service.working_dir = request.working_dir;
        service.external = Some(
            self.discovery
                .inspect(&request.identity)?
                .ok_or("接入期间原进程已退出或身份改变，请刷新后重新选择。")?,
        );
        let service = self.store.save_service(service)?;
        // 重新关联不继承旧实例的临时解除保护决定。
        if let Some(old) = old_identity {
            self.state
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .unlocked
                .remove(&old);
        }
        drop(guard);
        self.refresh();
        Ok(service)
    }
    pub fn refresh(&self) {
        let _refresh = self.refresh_lock.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(services) = self.store.services() else {
            return;
        };
        let external = services
            .into_iter()
            .filter(|s| s.source == "external" && s.kind != "desktopApp")
            .collect::<Vec<_>>();
        let ids = external
            .iter()
            .map(|s| s.id.clone())
            .collect::<HashSet<_>>();
        self.state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .retain(|id, _| ids.contains(id));
        if external.is_empty() {
            return;
        }
        let needs_ports = {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            external.iter().any(|s| {
                state
                    .runtimes
                    .get(&s.id)
                    .is_none_or(|r| r.status != "stopped")
            })
        };
        // 全部关联都已确认退出时不再轮询全机 socket；新接入或仍活动/未知的实例才需要扫描。
        let snapshot = if needs_ports {
            self.discovery.snapshot(false)
        } else {
            DiscoverySnapshot {
                revision: 0,
                updated_at: None,
                stale: false,
                error: None,
                processes: vec![],
                unattributed_ports: vec![],
            }
        };
        let identities = snapshot
            .processes
            .iter()
            .map(|p| (p.instance.identity.pid, p.instance.identity.clone()))
            .collect::<HashMap<_, _>>();
        let ports = if snapshot.stale {
            Err(snapshot.error.unwrap_or("端口快照已过期。".into()))
        } else {
            Ok(snapshot
                .processes
                .into_iter()
                .flat_map(|p| p.ports)
                .chain(snapshot.unattributed_ports)
                .collect::<Vec<_>>())
        };
        for service in external {
            let (previous, unlocked) = {
                let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
                if state.busy.contains(&service.id) {
                    continue;
                }
                (
                    state.runtimes.get(&service.id).cloned(),
                    service
                        .external
                        .as_ref()
                        .is_some_and(|e| state.unlocked.contains(&e.identity)),
                )
            };
            let next = self.inspect_runtime(&service, previous, unlocked, &ports, &identities);
            let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            // 扫描期间可能完成删除、重新关联或转换。发布前再次比较持久化身份，
            // 避免旧扫描覆盖新 runId 或把已删除记录重新放回运行时。
            if state.busy.contains(&service.id) {
                continue;
            }
            let still_same = self.store.service(&service.id).ok().is_some_and(|s| {
                s.source == "external"
                    && s.kind != "desktopApp"
                    && s.external.as_ref().map(|e| &e.identity)
                        == service.external.as_ref().map(|e| &e.identity)
            });
            if !still_same {
                continue;
            }
            if next.status == "stopped" {
                if let Some(external) = &service.external {
                    state.unlocked.remove(&external.identity);
                }
            }
            state.runtimes.insert(service.id.clone(), next.clone());
            self.emit(&next);
        }
    }
    fn inspect_runtime(
        &self,
        service: &ServiceConfig,
        previous: Option<ServiceRuntime>,
        unlocked: bool,
        ports: &Result<Vec<PortInfo>, String>,
        identities: &HashMap<u32, ProcessIdentity>,
    ) -> ServiceRuntime {
        let mut next = previous.unwrap_or_else(|| ServiceRuntime::stopped(&service.id));
        let Some(external) = &service.external else {
            return next;
        };
        next.run_id = Some(format!(
            "external:{}:{}",
            external.identity.pid, external.identity.created_at
        ));
        next.can_start = false;
        next.can_restart = false;
        next.can_stop = false;
        next.can_unlock = false;
        next.can_convert = false;
        next.protection_unlocked = unlocked;
        let launch_error = self.runtime.validate_launch(service).err();
        let launch_ready = launch_error.is_none();
        match self.discovery.inspect(&external.identity) {
            Ok(None) => {
                next.status = "stopped".into();
                next.pid = None;
                next.ports.clear();
                next.conflicts.clear();
                next.stale = false;
                next.port_error = None;
                next.error = None;
                next.control_reason = Some(match &launch_error {
                    Some(error) => {
                        format!("原实例已退出；新出现的进程需要重新关联。转换配置不可用：{error}")
                    }
                    None => "原实例已退出；新出现的进程需要重新关联。".into(),
                });
                next.can_convert = launch_ready && !external.shared_host;
                next.updated_at = Some(now());
                next.protection_unlocked = false;
            }
            Ok(Some(current)) => {
                next.status = "running".into();
                next.pid = Some(current.identity.pid);
                next.error = None;
                next.can_unlock = current.controllable
                    && !current.shared_host
                    && current.category != "nonSystem"
                    && !unlocked;
                next.can_stop = current.controllable
                    && !current.shared_host
                    && (current.category == "nonSystem" || unlocked);
                next.can_convert = next.can_stop && launch_ready;
                next.control_reason = if current.shared_host {
                    Some("共享宿主仅支持观察，不能通过终止整个进程停止其中一项服务。".into())
                } else if !current.controllable {
                    current.control_reason.clone()
                } else if !next.can_stop {
                    Some("系统或待确认实例默认受保护，可对当前实例主动解除保护。".into())
                } else {
                    launch_error
                        .as_ref()
                        .map(|error| format!("转换配置不可用：{error}"))
                };
                match ports {
                    Ok(ports) => {
                        next.ports = ports
                            .iter()
                            .filter(|p| {
                                p.pid == Some(current.identity.pid)
                                    && identities.get(&current.identity.pid)
                                        == Some(&current.identity)
                            })
                            .cloned()
                            .collect();
                        next.conflicts = ports
                            .iter()
                            .filter(|p| {
                                p.pid != Some(current.identity.pid)
                                    && service.expected_ports.contains(&p.port)
                            })
                            .cloned()
                            .collect();
                        next.stale = false;
                        next.port_error = None;
                        next.updated_at = Some(now());
                    }
                    Err(error) => {
                        next.stale = true;
                        next.port_error = Some(error.clone());
                    }
                }
            }
            Err(error) => {
                next.status = "unknown".into();
                next.stale = true;
                next.error = Some(error);
                next.control_reason = Some("无法重新核验进程身份，暂时禁止停止和转换。".into());
            }
        }
        next
    }
    pub fn unlock(&self, service_id: &str) -> Result<ServiceRuntime, String> {
        let guard = self.busy(service_id)?;
        let service = self.store.service(service_id)?;
        let external = external_ref(&service)?;
        let current = self
            .discovery
            .inspect(&external.identity)?
            .ok_or("原实例已退出，请重新关联。")?;
        if current.shared_host || !current.controllable {
            return Err(current
                .control_reason
                .unwrap_or("该实例仅支持观察，无法解除保护。".into()));
        }
        if current.category == "nonSystem" {
            return Err("该非系统实例无需解除保护，可直接在当前权限内停止。".into());
        }
        self.state
            .lock()
            .map_err(|_| "外部实例状态不可用")?
            .unlocked
            .insert(current.identity);
        drop(guard);
        self.refresh();
        self.runtimes()
            .into_iter()
            .find(|r| r.service_id == service_id)
            .ok_or("无法读取外部实例状态。".into())
    }
    fn verify_control(&self, service: &ServiceConfig) -> Result<Option<ExternalInstance>, String> {
        let external = external_ref(service)?;
        let Some(current) = self.discovery.inspect(&external.identity)? else {
            return Ok(None);
        };
        if current.shared_host || !current.controllable {
            return Err(current
                .control_reason
                .clone()
                .unwrap_or("该实例仅支持观察。".into()));
        }
        if current.category != "nonSystem"
            && !self
                .state
                .lock()
                .map_err(|_| "外部实例状态不可用")?
                .unlocked
                .contains(&current.identity)
        {
            return Err("该实例受保护，请先主动解除当前实例的保护。".into());
        }
        Ok(Some(current))
    }
    pub fn stop(&self, service_id: &str) -> Result<(), String> {
        let guard = self.busy(service_id)?;
        let service = self.store.service(service_id)?;
        let result = self.stop_inner(&service);
        drop(guard);
        self.refresh();
        if let Err(error) = &result {
            self.record_error(service_id, error);
        }
        result
    }
    fn stop_inner(&self, service: &ServiceConfig) -> Result<(), String> {
        let Some(current) = self.verify_control(service)? else {
            return Ok(());
        };
        {
            let mut state = self.state.lock().map_err(|_| "外部实例状态不可用")?;
            let runtime = state
                .runtimes
                .entry(service.id.clone())
                .or_insert_with(|| ServiceRuntime::stopped(&service.id));
            runtime.status = "stopping".into();
            runtime.can_stop = false;
            runtime.can_convert = false;
            self.emit(runtime);
        }
        // 只将经创建身份核验的单个 PID 交给平台层，绝不沿父子关系或同组关系终止其他进程。
        self.discovery.terminate(&current.identity)
    }
    fn record_error(&self, service_id: &str, error: &str) {
        if let Some(runtime) = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .runtimes
            .get_mut(service_id)
        {
            runtime.error = Some(error.into());
            self.emit(runtime);
        }
    }
    pub fn convert(&self, service_id: &str) -> Result<TerminalSession, String> {
        let guard = self.busy(service_id)?;
        let mut service = self.store.service(service_id)?;
        let external = external_ref(&service)?.clone();
        if external.shared_host {
            return Err("共享宿主不能转换为普通托管进程。".into());
        }
        // 验证完整配方后才停止旧实例。启动命令绝不从进程参数直接拼回 shell 字符串。
        self.runtime.validate_launch(&service)?;
        let result = (|| {
            self.stop_inner(&service)?;
            if self.discovery.inspect(&external.identity)?.is_some() {
                return Err("原实例尚未退出，已取消转换。".into());
            }
            let ports = self.discovery.ports(true)?;
            let conflicts = ports
                .iter()
                .filter(|p| service.expected_ports.contains(&p.port))
                .map(|p| p.port.to_string())
                .collect::<HashSet<_>>();
            if !conflicts.is_empty() {
                return Err(format!(
                    "预期端口仍被占用（{}），可能被后台管理器重新拉起；已取消转换。",
                    conflicts.into_iter().collect::<Vec<_>>().join("、")
                ));
            }
            service.source = "managed".into();
            service.external = None;
            let session = self.runtime.start_service(service.clone())?;
            // 创建失败时不更改来源。若数据库提交失败，停止刚创建的实例，保留外部记录供修复。
            if let Err(error) = self.store.save_service(service) {
                let cleanup = self.runtime.stop_service(service_id);
                return Err(format!(
                    "保存托管来源失败：{error}{}",
                    cleanup
                        .err()
                        .map(|e| format!("；新实例清理失败：{e}"))
                        .unwrap_or_default()
                ));
            }
            let mut state = self.state.lock().map_err(|_| "外部实例状态不可用")?;
            state.runtimes.remove(service_id);
            state.unlocked.remove(&external.identity);
            Ok(session)
        })();
        drop(guard);
        self.refresh();
        if let Err(error) = &result {
            self.record_error(service_id, error);
        }
        result
    }
    /// 调用者已清理平台创建的关联终端并删除数据库记录；此处只有解除跟踪，没有终止动作。
    pub fn forget(&self, service_id: &str) {
        let mut state = self.state.lock().unwrap_or_else(|e| e.into_inner());
        state.runtimes.remove(service_id);
    }
}
fn external_ref(service: &ServiceConfig) -> Result<&ExternalInstance, String> {
    if service.source != "external" || service.kind == "desktopApp" {
        return Err("此操作只适用于外部接入的服务。".into());
    }
    service
        .external
        .as_ref()
        .ok_or("外部实例身份缺失，请重新接入。".into())
}
fn now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
