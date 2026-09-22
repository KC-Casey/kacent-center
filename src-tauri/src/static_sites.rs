//! 静态站点预览运行时。
//!
//! 每个站点都由本程序启动一个独立 helper 进程。helper 自己绑定 loopback socket，
//! 管理器只控制自己持有的 `Child` 句柄，因此站点停止与服务发现到的用户进程互不相干。

use crate::{
    discovery::DiscoveryManager,
    models::{LaunchConfig, PortInfo, ServiceConfig, ServiceLaunchResult, ServiceRuntime},
};
use axum::{
    body::Body,
    extract::State,
    http::{header, Request, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    Router,
};
use chrono::{SecondsFormat, Utc};
use std::{
    collections::{HashMap, HashSet},
    env,
    ffi::OsString,
    fs,
    io::{self, BufRead, Read, Write},
    net::SocketAddr,
    path::{Component, Path, PathBuf},
    process::{Child, ChildStdin, Command, Stdio},
    str::FromStr,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc::{self, RecvTimeoutError, SyncSender},
        Arc, Mutex, MutexGuard,
    },
    thread,
    time::{Duration, Instant},
};
use tauri::{AppHandle, Emitter};
use tokio::sync::watch;
use tower_http::services::ServeDir;
use uuid::Uuid;

const READY_PREFIX: &str = "KACENT_STATIC_READY ";
const ERROR_PREFIX: &str = "KACENT_STATIC_ERROR ";
const MAX_RUNTIME_MESSAGES: usize = 120;
const MAX_MESSAGE_CHARS: usize = 2_048;
const MAX_LOG_LINE_BYTES: usize = 8 * 1_024;
const START_TIMEOUT: Duration = Duration::from_secs(12);
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(5);

struct StaticSiteState {
    runtimes: HashMap<String, ServiceRuntime>,
    sites: HashMap<String, Arc<ManagedSite>>,
    busy: HashSet<String>,
}

/// helper 的所有权只保存在这个模块内，且不经过 PTY worker 或端口 PID 反查来决定停止范围。
struct ManagedSite {
    service_id: String,
    run_id: String,
    pid: u32,
    process: Mutex<ManagedProcess>,
    stop_requested: AtomicBool,
    exited: AtomicBool,
    #[cfg(windows)]
    kill_job: Mutex<Option<WindowsKillJob>>,
}

struct ManagedProcess {
    child: Child,
    /// 关闭这条专用管道会通知 helper 优雅关闭 Axum；stdin 从不承载用户命令。
    shutdown_pipe: Option<ChildStdin>,
}

struct OperationGuard<'a> {
    manager: &'a StaticSiteManager,
    service_id: String,
}

impl Drop for OperationGuard<'_> {
    fn drop(&mut self) {
        lock_recover(&self.manager.state)
            .busy
            .remove(&self.service_id);
    }
}

/// 静态站点管理器。`helper_override` 只供本地验收程序指向同一个编译产物。
pub struct StaticSiteManager {
    app: Option<AppHandle>,
    _discovery: Arc<DiscoveryManager>,
    helper_override: Option<PathBuf>,
    state: Mutex<StaticSiteState>,
}

#[derive(Clone, Debug)]
enum StartupSignal {
    Ready(SocketAddr),
    Error(String),
    Closed,
}

impl StaticSiteManager {
    pub fn new(discovery: Arc<DiscoveryManager>, app: Option<AppHandle>) -> Arc<Self> {
        Self::new_with_helper_override(discovery, app, None)
    }

    /// 验收程序可传入已构建的主程序路径；正式运行始终使用当前应用自身的可执行文件。
    pub fn new_with_helper_override(
        discovery: Arc<DiscoveryManager>,
        app: Option<AppHandle>,
        helper_override: Option<PathBuf>,
    ) -> Arc<Self> {
        Arc::new(Self {
            app,
            _discovery: discovery,
            helper_override,
            state: Mutex::new(StaticSiteState {
                runtimes: HashMap::new(),
                sites: HashMap::new(),
                busy: HashSet::new(),
            }),
        })
    }

    /// 启动或复用同一服务已经运行的静态预览。
    pub fn start(self: &Arc<Self>, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let _operation = self.acquire(&config.id)?;
        self.start_inner(config)
    }

    pub fn stop(self: &Arc<Self>, service_id: &str) -> Result<(), String> {
        let _operation = self.acquire(service_id)?;
        self.stop_inner(service_id)
    }

    /// 重启沿用服务操作锁，只有旧 helper 确认退出后才会创建新 runId。
    pub fn restart(self: &Arc<Self>, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let _operation = self.acquire(&config.id)?;
        self.stop_inner(&config.id)?;
        self.start_inner(config)
    }

    /// 删除静态站点时先关掉由本管理器启动的 helper，再移除它的运行态。
    pub fn remove_service(&self, service_id: &str) -> Result<(), String> {
        let _operation = self.acquire(service_id)?;
        self.stop_inner(service_id)?;
        let mut state = lock_recover(&self.state);
        state.sites.remove(service_id);
        state.runtimes.remove(service_id);
        drop(state);
        self.emit_runtime(&ServiceRuntime::stopped(service_id));
        Ok(())
    }

    /// 应用完全退出时停止所有本模块拥有的静态预览；单个失败不会阻止清理其余站点。
    pub fn stop_all(self: &Arc<Self>) -> Result<(), String> {
        let ids = lock_recover(&self.state)
            .sites
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        let mut first_error = None;
        for id in ids {
            if let Err(error) = self.stop(&id) {
                first_error.get_or_insert_with(|| format!("停止静态站点 {id} 失败：{error}"));
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    pub fn runtimes(&self) -> Vec<ServiceRuntime> {
        let mut values = lock_recover(&self.state)
            .runtimes
            .values()
            .cloned()
            .collect::<Vec<_>>();
        values.sort_by(|left, right| left.service_id.cmp(&right.service_id));
        values
    }

    /// 仅列出 helper 自己的根 PID，供服务发现标记归属；不会枚举或扩展到用户进程。
    pub fn managed_processes(&self) -> HashMap<u32, String> {
        lock_recover(&self.state)
            .sites
            .values()
            .filter(|site| !site.exited.load(Ordering::SeqCst))
            .map(|site| (site.pid, site.service_id.clone()))
            .collect()
    }

    fn acquire(&self, service_id: &str) -> Result<OperationGuard<'_>, String> {
        if service_id.trim().is_empty() {
            return Err("静态站点服务编号不能为空。".into());
        }
        let mut state = lock_recover(&self.state);
        if !state.busy.insert(service_id.into()) {
            return Err("该静态站点正在启动、停止或重启，请稍后重试。".into());
        }
        Ok(OperationGuard {
            manager: self,
            service_id: service_id.into(),
        })
    }

    fn start_inner(self: &Arc<Self>, config: ServiceConfig) -> Result<ServiceLaunchResult, String> {
        let (root, port, spa_fallback) = match (&config.kind[..], &config.launch) {
            (
                "staticSite",
                LaunchConfig::StaticSite {
                    root,
                    port,
                    spa_fallback,
                },
            ) => (root.clone(), *port, *spa_fallback),
            ("staticSite", _) => return Err("静态站点缺少有效的目录和端口配置。".into()),
            _ => return Err("该服务类型不是静态站点。".into()),
        };

        {
            let mut state = lock_recover(&self.state);
            if let Some(existing) = state.runtimes.get(&config.id) {
                match existing.status.as_str() {
                    "running" => {
                        if state
                            .sites
                            .get(&config.id)
                            .is_some_and(|site| !site.exited.load(Ordering::SeqCst))
                        {
                            return Ok(ServiceLaunchResult {
                                runtime: existing.clone(),
                                session: None,
                                candidates: vec![],
                            });
                        }
                    }
                    "starting" | "stopping" => {
                        return Err("静态站点正在启动或停止，请稍后重试。".into());
                    }
                    _ => {}
                }
            }

            // 旧 run 的 helper 必须已退出；这一步只释放此前由当前管理器拥有的句柄。
            if let Some(old_site) = state.sites.get(&config.id) {
                if !old_site.exited.load(Ordering::SeqCst) {
                    return Err("上一轮静态预览仍在运行，拒绝启动第二个实例。".into());
                }
            }
            state.sites.remove(&config.id);
            let run_id = Uuid::new_v4().to_string();
            state.runtimes.insert(
                config.id.clone(),
                ServiceRuntime {
                    run_id: Some(run_id),
                    status: "starting".into(),
                    started_at: Some(timestamp_now()),
                    updated_at: Some(timestamp_now()),
                    can_start: false,
                    can_restart: false,
                    ..ServiceRuntime::stopped(&config.id)
                },
            );
        }
        self.emit_runtime_for(&config.id);

        let validation = validate_site_root(Path::new(&root));
        let canonical_root = match validation {
            Ok(root) => root,
            Err(error) => {
                self.fail_start(&config.id, &error);
                return Err(error);
            }
        };
        let run_id = lock_recover(&self.state)
            .runtimes
            .get(&config.id)
            .and_then(|runtime| runtime.run_id.clone())
            .ok_or_else(|| "静态站点启动编号丢失。".to_owned())?;

        let helper = match self.helper_path() {
            Ok(path) => path,
            Err(error) => {
                self.fail_start_for_run(&config.id, &run_id, &error);
                return Err(error);
            }
        };
        let mut command = Command::new(&helper);
        command
            .arg("--static-server")
            .arg(canonical_root.as_os_str())
            .arg(port.to_string())
            .arg(if spa_fallback { "1" } else { "0" })
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            // helper 进入独立 session。退出清理只调用 Child 上的精确 PID 句柄，
            // 不向用户已有服务发送 session/group 信号。
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
            command.creation_flags(CREATE_NEW_PROCESS_GROUP);
        }

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                let detail = format!("无法启动静态预览 helper：{error}");
                self.fail_start_for_run(&config.id, &run_id, &detail);
                return Err(detail);
            }
        };
        let pid = child.id();
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "静态预览 helper 没有可读取的标准输出。".to_owned())?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| "静态预览 helper 没有可读取的错误输出。".to_owned())?;
        let shutdown_pipe = child.stdin.take();
        let site = Arc::new(ManagedSite {
            service_id: config.id.clone(),
            run_id: run_id.clone(),
            pid,
            process: Mutex::new(ManagedProcess {
                child,
                shutdown_pipe,
            }),
            stop_requested: AtomicBool::new(false),
            exited: AtomicBool::new(false),
            #[cfg(windows)]
            kill_job: Mutex::new(None),
        });

        #[cfg(windows)]
        self.try_attach_kill_job(&site);

        {
            let mut state = lock_recover(&self.state);
            // 在启动握手前就登记所有权，以便任何失败路径都能只清理由本模块创建的进程。
            state.sites.insert(config.id.clone(), Arc::clone(&site));
        }

        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        self.spawn_stdout_reader(stdout, &config.id, &run_id, ready_tx);
        self.spawn_stderr_reader(stderr, &config.id, &run_id);

        let address = match ready_rx.recv_timeout(START_TIMEOUT) {
            Ok(StartupSignal::Ready(address)) => address,
            Ok(StartupSignal::Error(error)) => {
                self.abort_start(&site);
                let detail = format!("静态预览启动失败：{error}");
                self.fail_start_for_run(&config.id, &run_id, &detail);
                return Err(detail);
            }
            Ok(StartupSignal::Closed) => {
                self.abort_start(&site);
                let detail = self.startup_early_exit_error(&config.id, &run_id);
                self.fail_start_for_run(&config.id, &run_id, &detail);
                return Err(detail);
            }
            Err(RecvTimeoutError::Timeout) => {
                self.abort_start(&site);
                let detail = format!(
                    "静态预览 helper 在 {} 秒内没有回报监听地址，已停止该 helper。",
                    START_TIMEOUT.as_secs()
                );
                self.fail_start_for_run(&config.id, &run_id, &detail);
                return Err(detail);
            }
            Err(RecvTimeoutError::Disconnected) => {
                self.abort_start(&site);
                let detail = "静态预览 helper 启动握手通道已关闭。".to_owned();
                self.fail_start_for_run(&config.id, &run_id, &detail);
                return Err(detail);
            }
        };

        let started_runtime = {
            let mut state = lock_recover(&self.state);
            match state.runtimes.get_mut(&config.id) {
                None => Err("静态站点运行态在启动期间被移除。".to_owned()),
                Some(runtime) if runtime.run_id.as_deref() != Some(run_id.as_str()) => {
                    // 旧 helper 的 ready 事件不能覆盖之后写入的新 runId。
                    Err("静态站点运行编号已变化，已拒绝接管旧 helper。".to_owned())
                }
                Some(runtime) => {
                    runtime.status = "running".into();
                    runtime.pid = Some(pid);
                    runtime.error = None;
                    runtime.port_error = None;
                    runtime.ports = vec![PortInfo {
                        address: "127.0.0.1".into(),
                        port: address.port(),
                        protocol: "TCP".into(),
                        pid: Some(pid),
                    }];
                    runtime.web_urls = vec![format!("http://127.0.0.1:{}/", address.port())];
                    runtime.can_stop = true;
                    runtime.can_start = false;
                    runtime.can_restart = true;
                    runtime.updated_at = Some(timestamp_now());
                    push_runtime_message(
                        runtime,
                        format!("静态预览已启动：{}", runtime.web_urls[0]),
                    );
                    Ok(runtime.clone())
                }
            }
        };
        let runtime = match started_runtime {
            Ok(runtime) => runtime,
            Err(error) => {
                // 先释放运行态锁再清理 helper，避免退出回调再次获取同一把状态锁时死锁。
                self.abort_start(&site);
                return Err(error);
            }
        };
        self.emit_runtime(&runtime);
        self.spawn_exit_monitor(Arc::clone(&site));
        Ok(ServiceLaunchResult {
            runtime,
            session: None,
            candidates: vec![],
        })
    }

    fn stop_inner(&self, service_id: &str) -> Result<(), String> {
        let (site, run_id) = {
            let mut state = lock_recover(&self.state);
            let Some(site) = state.sites.get(service_id).cloned() else {
                if let Some(runtime) = state.runtimes.get_mut(service_id) {
                    if runtime.status == "stopping" {
                        return Err("静态预览仍在停止，请稍后重试。".into());
                    }
                    runtime.status = "stopped".into();
                    runtime.pid = None;
                    runtime.ports.clear();
                    runtime.web_urls.clear();
                    runtime.can_start = true;
                    runtime.can_stop = false;
                    runtime.can_restart = false;
                    runtime.updated_at = Some(timestamp_now());
                }
                drop(state);
                self.emit_runtime_for(service_id);
                return Ok(());
            };
            if site.exited.load(Ordering::SeqCst) {
                state.sites.remove(service_id);
                drop(state);
                self.emit_runtime_for(service_id);
                return Ok(());
            }
            let Some(runtime) = state.runtimes.get_mut(service_id) else {
                return Err("静态站点运行态不存在，拒绝停止未登记进程。".into());
            };
            if runtime.run_id.as_deref() != Some(site.run_id.as_str()) {
                return Err("静态站点运行编号不匹配，已拒绝停止旧进程。".into());
            }
            runtime.status = "stopping".into();
            runtime.can_stop = true;
            runtime.can_start = false;
            runtime.can_restart = false;
            runtime.updated_at = Some(timestamp_now());
            (site.clone(), site.run_id.clone())
        };
        self.emit_runtime_for(service_id);

        site.stop_requested.store(true, Ordering::SeqCst);
        if let Ok(mut process) = site.process.lock() {
            // EOF 是专用关闭信号。helper 进入 Axum 的 graceful shutdown，不依赖网络端口猜测。
            process.shutdown_pipe.take();
        }

        let graceful_deadline = Instant::now() + GRACEFUL_STOP_TIMEOUT;
        loop {
            if site.exited.load(Ordering::SeqCst) {
                break;
            }
            let exit = {
                let mut process = site
                    .process
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                process.child.try_wait()
            };
            match exit {
                Ok(Some(status)) => {
                    self.finish_run(&site, status.code().map(|code| code as u32), None);
                    break;
                }
                Ok(None) if Instant::now() < graceful_deadline => {
                    thread::sleep(Duration::from_millis(80));
                }
                Ok(None) => break,
                Err(error) => {
                    let detail = format!("检查静态预览退出状态失败：{error}");
                    self.set_runtime_error_if_current(service_id, &run_id, &detail);
                    return Err(detail);
                }
            }
        }

        if !site.exited.load(Ordering::SeqCst) {
            let force_result = {
                let mut process = site
                    .process
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                // 退出监视器和最后一次优雅关闭检查之间可能刚好完成退出；只检查本 Child，
                // 已退出即视为完成，不能把 kill 的“进程不存在”当作新的停止失败。
                match process.child.try_wait() {
                    Ok(Some(status)) => Ok(status.code().map(|code| code as u32)),
                    Ok(None) => match process.child.kill() {
                        Ok(()) => process
                            .child
                            .wait()
                            .map(|status| status.code().map(|code| code as u32)),
                        Err(error) => match process.child.try_wait() {
                            Ok(Some(status)) => Ok(status.code().map(|code| code as u32)),
                            _ => Err(error),
                        },
                    },
                    Err(error) => Err(error),
                }
            };
            match force_result {
                Ok(code) => self.finish_run(
                    &site,
                    code,
                    Some("静态预览未在五秒内关闭，已结束其专属 helper 进程。"),
                ),
                Err(error) => {
                    let detail = format!("强制结束静态预览 helper 失败：{error}");
                    self.set_runtime_error_if_current(service_id, &run_id, &detail);
                    return Err(detail);
                }
            }
        }
        self.emit_runtime_for(service_id);
        Ok(())
    }

    fn abort_start(&self, site: &Arc<ManagedSite>) {
        site.stop_requested.store(true, Ordering::SeqCst);
        let status = {
            let mut process = site
                .process
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            process.shutdown_pipe.take();
            match process.child.try_wait() {
                Ok(Some(status)) => status.code().map(|code| code as u32),
                _ => {
                    let _ = process.child.kill();
                    process
                        .child
                        .wait()
                        .ok()
                        .and_then(|status| status.code().map(|code| code as u32))
                }
            }
        };
        self.finish_run(site, status, None);
    }

    fn fail_start(&self, service_id: &str, error: &str) {
        let run_id = lock_recover(&self.state)
            .runtimes
            .get(service_id)
            .and_then(|runtime| runtime.run_id.clone());
        if let Some(run_id) = run_id {
            self.fail_start_for_run(service_id, &run_id, error);
        }
    }

    fn fail_start_for_run(&self, service_id: &str, run_id: &str, error: &str) {
        let runtime = {
            let mut state = lock_recover(&self.state);
            if let Some(site) = state.sites.get(service_id) {
                if site.run_id == run_id && site.exited.load(Ordering::SeqCst) {
                    state.sites.remove(service_id);
                }
            }
            let Some(runtime) = state.runtimes.get_mut(service_id) else {
                return;
            };
            if runtime.run_id.as_deref() != Some(run_id) {
                return;
            }
            runtime.status = "failed".into();
            runtime.pid = None;
            runtime.ports.clear();
            runtime.web_urls.clear();
            runtime.error = Some(error.into());
            runtime.can_start = true;
            runtime.can_stop = false;
            runtime.can_restart = false;
            runtime.updated_at = Some(timestamp_now());
            push_runtime_message(runtime, error.to_owned());
            runtime.clone()
        };
        self.emit_runtime(&runtime);
    }

    fn finish_run(&self, site: &Arc<ManagedSite>, exit_code: Option<u32>, detail: Option<&str>) {
        site.exited.store(true, Ordering::SeqCst);
        let runtime = {
            let mut state = lock_recover(&self.state);
            let Some(runtime) = state.runtimes.get_mut(&site.service_id) else {
                state.sites.remove(&site.service_id);
                return;
            };
            if runtime.run_id.as_deref() != Some(site.run_id.as_str()) {
                return;
            }
            if site.stop_requested.load(Ordering::SeqCst) {
                runtime.status = "stopped".into();
                runtime.error = None;
            } else {
                runtime.status = "failed".into();
                runtime.error = Some(detail.map(str::to_owned).unwrap_or_else(|| {
                    exit_code.map_or_else(
                        || "静态预览 helper 因信号或其他原因意外退出。".to_owned(),
                        |code| format!("静态预览 helper 意外退出，退出码 {code}。"),
                    )
                }));
            }
            runtime.exit_code = exit_code;
            runtime.pid = None;
            runtime.ports.clear();
            runtime.web_urls.clear();
            runtime.can_start = true;
            runtime.can_stop = false;
            runtime.can_restart = false;
            runtime.updated_at = Some(timestamp_now());
            if let Some(message) = runtime.error.clone() {
                push_runtime_message(runtime, message);
            } else {
                push_runtime_message(runtime, "静态预览已停止。".into());
            }
            let snapshot = runtime.clone();
            state.sites.remove(&site.service_id);
            snapshot
        };
        #[cfg(windows)]
        if let Ok(mut job) = site.kill_job.lock() {
            job.take();
        }
        self.emit_runtime(&runtime);
    }

    fn set_runtime_error_if_current(&self, service_id: &str, run_id: &str, error: &str) {
        let runtime = {
            let mut state = lock_recover(&self.state);
            let Some(runtime) = state.runtimes.get_mut(service_id) else {
                return;
            };
            if runtime.run_id.as_deref() != Some(run_id) {
                return;
            }
            runtime.error = Some(error.into());
            runtime.updated_at = Some(timestamp_now());
            push_runtime_message(runtime, error.to_owned());
            runtime.clone()
        };
        self.emit_runtime(&runtime);
    }

    fn startup_early_exit_error(&self, service_id: &str, run_id: &str) -> String {
        let state = lock_recover(&self.state);
        state
            .runtimes
            .get(service_id)
            .filter(|runtime| runtime.run_id.as_deref() == Some(run_id))
            .and_then(|runtime| runtime.messages.last())
            .map(|message| format!("静态预览 helper 在回报监听地址前退出：{message}"))
            .unwrap_or_else(|| "静态预览 helper 在回报监听地址前退出。".into())
    }

    fn helper_path(&self) -> Result<PathBuf, String> {
        if let Some(path) = self.helper_override.as_ref() {
            if path.is_file() {
                return Ok(path.clone());
            }
            return Err(format!("验收 helper 可执行文件不存在：{}", path.display()));
        }
        env::current_exe().map_err(|error| format!("无法定位当前应用可执行文件：{error}"))
    }

    fn spawn_stdout_reader(
        self: &Arc<Self>,
        stdout: impl Read + Send + 'static,
        service_id: &str,
        run_id: &str,
        ready_tx: SyncSender<StartupSignal>,
    ) {
        let manager = Arc::downgrade(self);
        let service_id = service_id.to_owned();
        let run_id = run_id.to_owned();
        let _ = thread::Builder::new()
            .name(format!("static-site-out-{}", service_id))
            .spawn(move || {
                let mut reader = io::BufReader::new(stdout);
                let mut sent_startup = false;
                loop {
                    let line = match read_bounded_line(&mut reader, MAX_LOG_LINE_BYTES) {
                        Ok(Some(line)) => line,
                        Ok(None) | Err(_) => break,
                    };
                    let text = String::from_utf8_lossy(&line)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    if let Some(value) = text.strip_prefix(READY_PREFIX) {
                        if let Ok(address) = SocketAddr::from_str(value.trim()) {
                            let _ = ready_tx.send(StartupSignal::Ready(address));
                            sent_startup = true;
                            continue;
                        }
                    }
                    if let Some(value) = text.strip_prefix(ERROR_PREFIX) {
                        let _ = ready_tx.send(StartupSignal::Error(value.trim().to_owned()));
                        sent_startup = true;
                        continue;
                    }
                    if let Some(manager) = manager.upgrade() {
                        manager.push_message(&service_id, &run_id, format!("[stdout] {text}"));
                    } else {
                        break;
                    }
                }
                if !sent_startup {
                    let _ = ready_tx.send(StartupSignal::Closed);
                }
            });
    }

    fn spawn_stderr_reader(
        self: &Arc<Self>,
        stderr: impl Read + Send + 'static,
        service_id: &str,
        run_id: &str,
    ) {
        let manager = Arc::downgrade(self);
        let service_id = service_id.to_owned();
        let run_id = run_id.to_owned();
        let _ = thread::Builder::new()
            .name(format!("static-site-err-{}", service_id))
            .spawn(move || {
                let mut reader = io::BufReader::new(stderr);
                loop {
                    let line = match read_bounded_line(&mut reader, MAX_LOG_LINE_BYTES) {
                        Ok(Some(line)) => line,
                        Ok(None) | Err(_) => break,
                    };
                    let text = String::from_utf8_lossy(&line)
                        .trim_end_matches(['\r', '\n'])
                        .to_owned();
                    let Some(manager) = manager.upgrade() else {
                        break;
                    };
                    manager.push_message(&service_id, &run_id, format!("[stderr] {text}"));
                }
            });
    }

    fn spawn_exit_monitor(self: &Arc<Self>, site: Arc<ManagedSite>) {
        let manager = Arc::downgrade(self);
        let _ = thread::Builder::new()
            .name(format!("static-site-watch-{}", site.service_id))
            .spawn(move || loop {
                if site.exited.load(Ordering::SeqCst) {
                    break;
                }
                let exit = {
                    let mut process = site
                        .process
                        .lock()
                        .unwrap_or_else(|error| error.into_inner());
                    process.child.try_wait()
                };
                match exit {
                    Ok(Some(status)) => {
                        if let Some(manager) = manager.upgrade() {
                            manager.finish_run(&site, status.code().map(|code| code as u32), None);
                        }
                        break;
                    }
                    Err(error) => {
                        if let Some(manager) = manager.upgrade() {
                            manager.set_runtime_error_if_current(
                                &site.service_id,
                                &site.run_id,
                                &format!("检查静态预览退出状态失败：{error}"),
                            );
                        }
                    }
                    Ok(None) => {}
                }
                thread::sleep(Duration::from_millis(350));
            });
    }

    fn push_message(&self, service_id: &str, run_id: &str, message: String) {
        let runtime = {
            let mut state = lock_recover(&self.state);
            let Some(runtime) = state.runtimes.get_mut(service_id) else {
                return;
            };
            if runtime.run_id.as_deref() != Some(run_id) {
                return;
            }
            push_runtime_message(runtime, message);
            runtime.updated_at = Some(timestamp_now());
            runtime.clone()
        };
        self.emit_runtime(&runtime);
    }

    fn emit_runtime_for(&self, service_id: &str) {
        if let Some(runtime) = lock_recover(&self.state).runtimes.get(service_id).cloned() {
            self.emit_runtime(&runtime);
        }
    }

    fn emit_runtime(&self, runtime: &ServiceRuntime) {
        if let Some(app) = self.app.as_ref() {
            let _ = app.emit("service-runtime", runtime.clone());
        }
    }

    #[cfg(windows)]
    fn try_attach_kill_job(&self, site: &Arc<ManagedSite>) {
        match WindowsKillJob::attach(site.pid) {
            Ok(job) => *lock_recover(&site.kill_job) = Some(job),
            Err(error) => self.push_message(
                &site.service_id,
                &site.run_id,
                format!("[info] Windows kill-on-close Job 不可用，仍由精确子进程句柄管理：{error}"),
            ),
        }
    }
}

/// 在 Tauri 构造和单实例插件初始化之前调用；命中专用参数时本进程只运行 helper。
pub fn run_worker_if_requested() -> bool {
    let args = env::args_os().collect::<Vec<_>>();
    let Some(index) = args.iter().position(|arg| arg == "--static-server") else {
        return false;
    };
    let code = run_worker(&args[index + 1..]);
    std::process::exit(code);
}

fn run_worker(args: &[OsString]) -> i32 {
    let fail = |message: &str| {
        let _ = writeln!(io::stdout(), "{ERROR_PREFIX}{message}");
        let _ = io::stdout().flush();
        2
    };
    if args.len() < 3 {
        return fail("helper 参数不完整。");
    }
    let root = PathBuf::from(&args[0]);
    let port = match args[1].to_string_lossy().parse::<u16>() {
        Ok(port) => port,
        Err(_) => return fail("端口不是有效的 0–65535 数值。"),
    };
    let spa_fallback = match args[2].to_string_lossy().as_ref() {
        "1" => true,
        "0" => false,
        _ => return fail("SPA 回退标志无效。"),
    };
    match run_static_worker(root, port, spa_fallback) {
        Ok(()) => 0,
        Err(error) => {
            let message = error.replace(['\r', '\n'], " ");
            fail(&message)
        }
    }
}

fn run_static_worker(root: PathBuf, port: u16, spa_fallback: bool) -> Result<(), String> {
    let root = validate_site_root(&root)?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("创建静态预览异步运行时失败：{error}"))?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .map_err(|error| {
                if port == 0 {
                    format!("无法绑定 127.0.0.1 上的自动端口：{error}")
                } else {
                    format!("端口 {port} 在 127.0.0.1 上不可用：{error}")
                }
            })?;
        let address = listener
            .local_addr()
            .map_err(|error| format!("读取静态预览实际监听地址失败：{error}"))?;
        let worker_state = WorkerState {
            root: Arc::new(root),
            spa_fallback,
        };
        let app = Router::new()
            .fallback_service(
                ServeDir::new(worker_state.root.as_path()).append_index_html_on_directories(true),
            )
            .layer(middleware::from_fn_with_state(
                worker_state,
                validate_request_and_spa_fallback,
            ));

        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        thread::Builder::new()
            .name("static-site-shutdown-pipe".into())
            .spawn(move || {
                let mut stdin = io::stdin().lock();
                let mut buffer = [0_u8; 128];
                loop {
                    match stdin.read(&mut buffer) {
                        Ok(0) | Err(_) => break,
                        // 该管道只负责在关闭时通知退出，写入字节不会被解析成命令。
                        Ok(_) => continue,
                    }
                }
                let _ = shutdown_tx.send(true);
            })
            .map_err(|error| format!("创建静态预览关闭监听线程失败：{error}"))?;

        println!("{READY_PREFIX}{address}");
        io::stdout()
            .flush()
            .map_err(|error| format!("回报静态预览监听地址失败：{error}"))?;
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                while !*shutdown_rx.borrow() {
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
            .map_err(|error| format!("静态预览 HTTP 服务失败：{error}"))
    })
}

#[derive(Clone)]
struct WorkerState {
    root: Arc<PathBuf>,
    spa_fallback: bool,
}

/// 在 ServeDir 打开文件前检查解码路径和每一条已存在路径的 canonical root 归属。
/// 缺失资源保持 404；只有明确的 HTML 文档导航且 URL 没有文件扩展名才走 SPA 回退。
/// canonicalize 与 ServeDir 真正 open 之间仍有同用户并发替换 symlink 的微小窗口；
/// 该检查阻止经 HTTP 请求读取当前指向目录之外的文件，不构成对本机同目录写入者的沙箱。
async fn validate_request_and_spa_fallback(
    State(state): State<WorkerState>,
    mut request: Request<Body>,
    next: Next,
) -> Response {
    let relative = match safe_relative_path(request.uri().path()) {
        Ok(path) => path,
        Err(()) => return StatusCode::BAD_REQUEST.into_response(),
    };
    let candidate = state.root.join(&relative);
    match ensure_existing_ancestors_stay_in_root(&state.root, &candidate).await {
        Ok(()) => {}
        Err(PathSafetyError::OutsideRoot) => return StatusCode::FORBIDDEN.into_response(),
        Err(PathSafetyError::Io) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }

    // ServeDir 会在目录请求上隐式追加 index.html；该文件也必须属于所选目录，
    // 否则目录自身在根内仍可能借助一个越界 index symlink 暴露目录外文件。
    if tokio::fs::metadata(&candidate)
        .await
        .is_ok_and(|metadata| metadata.is_dir())
    {
        match ensure_existing_ancestors_stay_in_root(&state.root, &candidate.join("index.html"))
            .await
        {
            Ok(()) => {}
            Err(PathSafetyError::OutsideRoot) => return StatusCode::FORBIDDEN.into_response(),
            Err(PathSafetyError::Io) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }

    // 仅把无后缀页面导航的 404 改写到项目入口；JS/CSS/图片等缺失文件仍由 ServeDir 返回 404。
    let should_fallback_to_root_index = if state.spa_fallback
        && !relative.as_os_str().is_empty()
        && !has_file_extension(&relative)
        && is_html_navigation(&request)
    {
        match tokio::fs::metadata(&candidate).await {
            Ok(metadata) if metadata.is_file() => false,
            Ok(metadata) if metadata.is_dir() => {
                let index = candidate.join("index.html");
                match tokio::fs::metadata(index).await {
                    Ok(_) => false,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => true,
                    Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
                }
            }
            Ok(_) => false,
            Err(error) if error.kind() == io::ErrorKind::NotFound => true,
            Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    } else {
        false
    };
    if should_fallback_to_root_index {
        // index.html 可能在站点启动后被替换；每次 SPA 回退前重新核验真实目标。
        match ensure_existing_ancestors_stay_in_root(&state.root, &state.root.join("index.html"))
            .await
        {
            Ok(()) => rewrite_to_index(&mut request),
            Err(PathSafetyError::OutsideRoot) => return StatusCode::FORBIDDEN.into_response(),
            Err(PathSafetyError::Io) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
        }
    }
    next.run(request).await
}

fn rewrite_to_index(request: &mut Request<Body>) {
    let query = request.uri().query().map(|query| format!("?{query}"));
    let value = format!("/index.html{}", query.unwrap_or_default());
    if let Ok(uri) = Uri::from_str(&value) {
        *request.uri_mut() = uri;
    }
}

fn is_html_navigation(request: &Request<Body>) -> bool {
    if request.method() != axum::http::Method::GET {
        return false;
    }
    let accepts_html = request
        .headers()
        .get(header::ACCEPT)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            value
                .split(',')
                .any(|item| item.trim().starts_with("text/html"))
        });
    if !accepts_html {
        return false;
    }
    let fetch_mode = request
        .headers()
        .get("sec-fetch-mode")
        .and_then(|value| value.to_str().ok());
    let fetch_dest = request
        .headers()
        .get("sec-fetch-dest")
        .and_then(|value| value.to_str().ok());
    // 现代浏览器提供 Fetch Metadata 时必须是 document/navigation；curl 等客户端可用 Accept 表示导航。
    match (fetch_mode, fetch_dest) {
        (None, None) => true,
        (Some("navigate"), None) | (None, Some("document")) => true,
        (Some("navigate"), Some("document")) => true,
        _ => false,
    }
}

fn has_file_extension(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            name.rsplit_once('.')
                .is_some_and(|(_, extension)| !extension.is_empty())
        })
}

/// 对 URI 做一次严格 percent decode；拒绝点目录、NUL 和 Windows 驱动器/ADS 写法。
fn safe_relative_path(uri_path: &str) -> Result<PathBuf, ()> {
    let bytes = uri_path.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(());
            }
            let high = hex_value(bytes[index + 1]).ok_or(())?;
            let low = hex_value(bytes[index + 2]).ok_or(())?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    let decoded = String::from_utf8(decoded).map_err(|_| ())?;
    if decoded.contains('\0') {
        return Err(());
    }
    // 统一反斜线后按 URI 分段，可同时拒绝 Windows 上伪装成 URL 分隔符的反斜线。
    let normalized = decoded.replace('\\', "/");
    let mut relative = PathBuf::new();
    for part in normalized.split('/') {
        if part.is_empty() {
            continue;
        }
        if part == "." || part == ".." || part.contains(':') {
            return Err(());
        }
        relative.push(part);
    }
    if relative
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(());
    }
    Ok(relative)
}

fn hex_value(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        b'A'..=b'F' => Some(value - b'A' + 10),
        _ => None,
    }
}

enum PathSafetyError {
    OutsideRoot,
    Io,
}

/// 即使目标文件不存在，也逐级核验最长存在父路径，避免借助外部目录 symlink 绕过检查。
async fn ensure_existing_ancestors_stay_in_root(
    root: &Path,
    candidate: &Path,
) -> Result<(), PathSafetyError> {
    let mut ancestor = candidate;
    loop {
        match tokio::fs::canonicalize(ancestor).await {
            Ok(canonical) => {
                return if canonical.starts_with(root) {
                    Ok(())
                } else {
                    Err(PathSafetyError::OutsideRoot)
                };
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = ancestor.parent().ok_or(PathSafetyError::OutsideRoot)?;
                if !ancestor.starts_with(root) {
                    return Err(PathSafetyError::OutsideRoot);
                }
            }
            Err(_) => return Err(PathSafetyError::Io),
        }
    }
}

fn validate_site_root(root: &Path) -> Result<PathBuf, String> {
    let root = fs::canonicalize(root)
        .map_err(|error| format!("静态站点目录无法访问：{}（{error}）", root.display()))?;
    if !root.is_dir() {
        return Err(format!("静态站点路径不是目录：{}", root.display()));
    }
    let index = fs::canonicalize(root.join("index.html"))
        .map_err(|error| format!("静态站点必须包含可读取的 index.html：{error}"))?;
    if !index.starts_with(&root) {
        return Err("静态站点入口 index.html 指向所选目录之外，已拒绝启动。".into());
    }
    if !index.is_file() {
        return Err("静态站点入口 index.html 不是普通文件。".into());
    }
    Ok(root)
}

/// 读取有上限的日志行，持续消耗超长行剩余部分，避免子进程输出撑大管理器内存。
fn read_bounded_line(reader: &mut impl BufRead, max_bytes: usize) -> io::Result<Option<Vec<u8>>> {
    let mut output = Vec::with_capacity(max_bytes.min(1_024));
    let mut saw_any = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            return if saw_any { Ok(Some(output)) } else { Ok(None) };
        }
        saw_any = true;
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(buffer.len(), |position| position + 1);
        let remaining = max_bytes.saturating_sub(output.len());
        let copied = consumed.min(remaining);
        output.extend_from_slice(&buffer[..copied]);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(Some(output));
        }
    }
}

fn push_runtime_message(runtime: &mut ServiceRuntime, message: String) {
    let message = message.chars().take(MAX_MESSAGE_CHARS).collect::<String>();
    if runtime.messages.len() >= MAX_RUNTIME_MESSAGES {
        let excess = runtime.messages.len() + 1 - MAX_RUNTIME_MESSAGES;
        runtime.messages.drain(0..excess);
    }
    runtime.messages.push(message);
}

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

#[cfg(windows)]
struct WindowsKillJob(usize);

#[cfg(windows)]
unsafe impl Send for WindowsKillJob {}

#[cfg(windows)]
impl WindowsKillJob {
    fn attach(pid: u32) -> Result<Self, String> {
        use windows_sys::Win32::{
            Foundation::{CloseHandle, HANDLE},
            System::{
                JobObjects::{
                    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
                    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
                    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                },
                Threading::{OpenProcess, PROCESS_SET_QUOTA, PROCESS_TERMINATE},
            },
        };

        // Job 只包含刚由本模块创建的 helper PID，关闭最后一个句柄会清理该 helper。
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            return Err("CreateJobObjectW 失败。".into());
        }
        let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        let set = unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                &limits as *const _ as *const _,
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        };
        if set == 0 {
            unsafe { CloseHandle(job) };
            return Err("配置 Job 的 kill-on-close 限制失败。".into());
        }
        let process = unsafe { OpenProcess(PROCESS_SET_QUOTA | PROCESS_TERMINATE, 0, pid) };
        if process.is_null() {
            unsafe { CloseHandle(job) };
            return Err("OpenProcess 无法取得 helper 的进程句柄。".into());
        }
        let assigned = unsafe { AssignProcessToJobObject(job, process) };
        unsafe { CloseHandle(process as HANDLE) };
        if assigned == 0 {
            unsafe { CloseHandle(job) };
            return Err("AssignProcessToJobObject 被系统拒绝。".into());
        }
        Ok(Self(job as usize))
    }
}

#[cfg(windows)]
impl Drop for WindowsKillJob {
    fn drop(&mut self) {
        use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
        unsafe { CloseHandle(self.0 as HANDLE) };
    }
}
