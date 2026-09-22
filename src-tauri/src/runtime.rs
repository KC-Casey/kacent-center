//! 本地运行时：把每个服务、交互终端与 Agent 放进独立 PTY 会话。
//!
//! 持久化配置由 `store` 管理；本模块只维护当前进程内的 PTY、事件缓冲和运行状态。

mod command;
#[cfg(unix)]
mod unix_process;
#[cfg(windows)]
mod windows_worker;

use crate::discovery::DiscoveryManager;
use crate::models::{
    AgentAvailability, AgentProfile, PortInfo, ServiceConfig, ServiceRuntime, TerminalEvent,
    TerminalSession,
};
use chrono::{SecondsFormat, Utc};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::fd::RawFd;
#[cfg(windows)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessesToUpdate, System};
use tauri::{ipc::Channel, AppHandle, Emitter};
use uuid::Uuid;

const MAX_EVENT_BUFFER_BYTES: usize = 2 * 1024 * 1024;
/// IPC 已发送但尚未被前端确认的数据最多保留 64 个事件，避免 webview 忙碌时 IPC 无限堆积。
const MAX_IN_FLIGHT_EVENTS: usize = 64;
const PTY_READ_CHUNK_BYTES: usize = 4096;
const SERVICE_PORT_REFRESH: Duration = Duration::from_secs(2);
#[cfg(unix)]
const PTY_DRAIN_MAX: Duration = Duration::from_secs(2);
#[cfg(unix)]
const PTY_DRAIN_IDLE: Duration = Duration::from_millis(250);

struct ManagerState {
    sessions: HashMap<String, Arc<ManagedSession>>,
    runtimes: HashMap<String, ServiceRuntime>,
    configs: HashMap<String, ServiceConfig>,
    shell_numbers: HashMap<String, usize>,
}

/// 所有进程和会话操作的入口。`None` 可用于不依赖 Tauri AppHandle 的真实进程调用。
pub struct RuntimeManager {
    app: Option<AppHandle>,
    state: Mutex<ManagerState>,
    /// 串行化快照与 emit，避免并发变更时旧终端列表晚于新列表抵达前端。
    session_emit_lock: Mutex<()>,
    gui_path: String,
    discovery: Arc<DiscoveryManager>,
}

struct ManagedSession {
    terminal: Mutex<TerminalSession>,
    terminal_changed: Condvar,
    writer: Arc<Mutex<Option<Box<dyn Write + Send>>>>,
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    #[cfg(windows)]
    killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
    pid: Option<u32>,
    events: Mutex<EventState>,
    event_changed: Condvar,
    stop_requested: AtomicBool,
    reader_drain_requested: AtomicBool,
    retired: AtomicBool,
    #[cfg(unix)]
    unix_tracker: Option<Arc<unix_process::SessionTracker>>,
    #[cfg(unix)]
    unix_pty_fd: RawFd,
}

struct EventState {
    queue: VecDeque<TerminalEvent>,
    queued_bytes: usize,
    last_seq: u64,
    sent_seq: u64,
    acked_seq: u64,
    pruned_through_seq: u64,
    pruned_bytes: usize,
    channel: Option<Channel<TerminalEvent>>,
    dispatcher_running: bool,
}

impl ManagedSession {
    fn new(
        terminal: TerminalSession,
        writer: Box<dyn Write + Send>,
        master: Box<dyn MasterPty + Send>,
        #[cfg(windows)] killer: Box<dyn portable_pty::ChildKiller + Send + Sync>,
        #[cfg(unix)] unix_tracker: Option<Arc<unix_process::SessionTracker>>,
        #[cfg(unix)] unix_pty_fd: RawFd,
    ) -> Self {
        Self {
            terminal: Mutex::new(terminal),
            terminal_changed: Condvar::new(),
            writer: Arc::new(Mutex::new(Some(writer))),
            master: Mutex::new(Some(master)),
            #[cfg(windows)]
            killer,
            pid: None,
            events: Mutex::new(EventState {
                queue: VecDeque::new(),
                queued_bytes: 0,
                last_seq: 0,
                sent_seq: 0,
                acked_seq: 0,
                pruned_through_seq: 0,
                pruned_bytes: 0,
                channel: None,
                dispatcher_running: false,
            }),
            event_changed: Condvar::new(),
            stop_requested: AtomicBool::new(false),
            reader_drain_requested: AtomicBool::new(false),
            retired: AtomicBool::new(false),
            #[cfg(unix)]
            unix_tracker,
            #[cfg(unix)]
            unix_pty_fd,
        }
    }

    fn snapshot(&self) -> TerminalSession {
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone()
    }

    fn is_exited(&self) -> bool {
        self.terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .state
            == "exited"
    }

    /// reader 完成有限排空后关闭 PTY 主端，避免脱离 session 的进程继续占用终端队列；
    /// 该操作只关闭终端通道，不向脱离的后台进程发送信号。
    fn close_io(&self) {
        // 先关 master，让任何正在写入的调用尽快收到 PTY 关闭错误；随后不等待 writer
        // 锁，避免脱离 session 的进程持有 slave 时把会话回收线程卡死。
        if let Ok(mut master) = self.master.lock() {
            master.take();
        }
        let writer = Arc::clone(&self.writer);
        let result = writer.try_lock();
        match result {
            Ok(mut writer) => {
                writer.take();
            }
            Err(std::sync::TryLockError::Poisoned(error)) => {
                let mut writer = error.into_inner();
                writer.take();
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                // write() 可能正在等待 Windows ConPTY 管道；关闭 master 后在独立线程回收，
                // 不让其锁持有时间影响退出事件或 stop_service 返回。
                let writer = Arc::clone(&writer);
                drop(thread::spawn(move || {
                    if let Ok(mut writer) = writer.lock() {
                        writer.take();
                    }
                }));
            }
        };
    }

    fn wait_for_exit(&self, duration: Duration) -> bool {
        let deadline = Instant::now() + duration;
        let mut terminal = self
            .terminal
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        while terminal.state != "exited" {
            let now = Instant::now();
            if now >= deadline {
                return false;
            }
            let remaining = deadline.saturating_duration_since(now);
            let result = self.terminal_changed.wait_timeout(terminal, remaining);
            match result {
                Ok((next, timeout)) => {
                    terminal = next;
                    if timeout.timed_out() && terminal.state != "exited" {
                        return false;
                    }
                }
                Err(error) => terminal = error.into_inner().0,
            }
        }
        true
    }

    /// 每次 PTY 输出都先进入受限缓冲，再由独立 dispatcher 发送；慢速或隐藏的 webview
    /// 因此不会反压 PTY reader，服务仍会持续排空其输出。
    fn push_data(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let terminal = self.snapshot();
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let seq = events.last_seq.saturating_add(1);
        events.last_seq = seq;
        let output = TerminalEvent {
            session_id: terminal.id.clone(),
            service_id: terminal.service_id.clone(),
            run_id: terminal.run_id.clone(),
            seq,
            kind: "data".into(),
            data: bytes.to_vec(),
            exit_code: None,
            dropped_bytes: 0,
        };

        let mut dropped_bytes = 0usize;
        // 被移除旧 gap 中的字节已计入累计值；只额外累计本轮新移除的数据。
        let mut newly_dropped_bytes = 0usize;
        let mut dropped_through = 0u64;
        while events.queued_bytes.saturating_add(bytes.len()) > MAX_EVENT_BUFFER_BYTES {
            // 已发出的事件留给 ACK 窗口；只删尚未发送的数据，并把被删内容折成一个有序 gap。
            let Some(index) = events.queue.iter().position(|event| {
                event.seq > events.sent_seq && (event.kind == "data" || event.kind == "gap")
            }) else {
                break;
            };
            if let Some(removed) = events.queue.remove(index) {
                dropped_through = dropped_through.max(removed.seq);
                if removed.kind == "data" {
                    let count = removed.data.len();
                    events.queued_bytes = events.queued_bytes.saturating_sub(count);
                    dropped_bytes = dropped_bytes.saturating_add(count);
                    newly_dropped_bytes = newly_dropped_bytes.saturating_add(count);
                } else {
                    dropped_bytes = dropped_bytes.saturating_add(removed.dropped_bytes);
                }
            }
        }

        if dropped_bytes > 0 {
            events.pruned_through_seq = events.pruned_through_seq.max(dropped_through);
            events.pruned_bytes = events.pruned_bytes.saturating_add(newly_dropped_bytes);
            let gap = TerminalEvent {
                session_id: terminal.id.clone(),
                service_id: terminal.service_id.clone(),
                run_id: terminal.run_id.clone(),
                seq: dropped_through,
                kind: "gap".into(),
                data: Vec::new(),
                exit_code: None,
                dropped_bytes,
            };
            insert_by_seq(&mut events.queue, gap);
        }

        events.queued_bytes = events.queued_bytes.saturating_add(bytes.len());
        insert_by_seq(&mut events.queue, output);
        drop(events);
        self.event_changed.notify_all();
    }

    fn push_exit(&self, exit_code: u32) {
        let terminal = {
            let mut terminal = self
                .terminal
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            terminal.state = "exited".into();
            terminal.exit_code = Some(exit_code);
            self.terminal_changed.notify_all();
            terminal.clone()
        };
        let mut events = self
            .events
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let seq = events.last_seq.saturating_add(1);
        events.last_seq = seq;
        events.queue.push_back(TerminalEvent {
            session_id: terminal.id,
            service_id: terminal.service_id,
            run_id: terminal.run_id,
            seq,
            kind: "exit".into(),
            data: Vec::new(),
            exit_code: Some(exit_code),
            dropped_bytes: 0,
        });
        drop(events);
        self.event_changed.notify_all();
    }

    fn subscribe(self: &Arc<Self>, channel: Channel<TerminalEvent>, after_seq: u64) {
        {
            let mut events = self
                .events
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let after_seq = after_seq.min(events.last_seq);
            events.channel = Some(channel);
            events.sent_seq = after_seq;
            events.acked_seq = after_seq;
            prune_acked(&mut events, after_seq);

            // 回放游标早于仍被保留的最早内容时，用 gap 明确告知前端缓冲已裁剪。
            if after_seq < events.pruned_through_seq
                && !events
                    .queue
                    .iter()
                    .any(|event| event.kind == "gap" && event.seq == events.pruned_through_seq)
            {
                let terminal = self.snapshot();
                let gap = TerminalEvent {
                    session_id: terminal.id,
                    service_id: terminal.service_id,
                    run_id: terminal.run_id,
                    seq: events.pruned_through_seq,
                    kind: "gap".into(),
                    data: Vec::new(),
                    exit_code: None,
                    dropped_bytes: events.pruned_bytes,
                };
                insert_by_seq(&mut events.queue, gap);
            }

            if events.dispatcher_running {
                self.event_changed.notify_all();
                return;
            }
            events.dispatcher_running = true;
        }

        let session = Arc::clone(self);
        thread::spawn(move || dispatch_events(session));
    }

    fn acknowledge(&self, seq: u64) -> Result<(), String> {
        let mut events = self
            .events
            .lock()
            .map_err(|_| "终端事件队列不可用".to_owned())?;
        if seq > events.sent_seq {
            return Err("终端确认序号超出已发送范围".into());
        }
        if seq > events.acked_seq {
            events.acked_seq = seq;
            prune_acked(&mut events, seq);
        }
        drop(events);
        self.event_changed.notify_all();
        Ok(())
    }
}

fn insert_by_seq(queue: &mut VecDeque<TerminalEvent>, event: TerminalEvent) {
    let index = queue
        .iter()
        .position(|current| current.seq > event.seq)
        .unwrap_or(queue.len());
    queue.insert(index, event);
}

fn prune_acked(events: &mut EventState, through: u64) {
    while events
        .queue
        .front()
        .map(|event| event.seq <= through)
        .unwrap_or(false)
    {
        if let Some(event) = events.queue.pop_front() {
            if event.kind == "data" {
                events.queued_bytes = events.queued_bytes.saturating_sub(event.data.len());
                events.pruned_bytes = events.pruned_bytes.saturating_add(event.data.len());
                events.pruned_through_seq = events.pruned_through_seq.max(event.seq);
            }
        }
    }
}

fn dispatch_events(session: Arc<ManagedSession>) {
    loop {
        let (channel, event, channel_id) = {
            let mut events = session
                .events
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            loop {
                if session.retired.load(Ordering::SeqCst) {
                    events.dispatcher_running = false;
                    return;
                }
                let Some(channel) = events.channel.clone() else {
                    events.dispatcher_running = false;
                    return;
                };
                let in_flight = events
                    .queue
                    .iter()
                    .filter(|event| event.seq > events.acked_seq && event.seq <= events.sent_seq)
                    .count();
                if in_flight < MAX_IN_FLIGHT_EVENTS {
                    if let Some(event) = events
                        .queue
                        .iter()
                        .find(|event| event.seq > events.sent_seq)
                        .cloned()
                    {
                        events.sent_seq = event.seq;
                        break (channel.clone(), event, channel.id());
                    }
                }
                events = session
                    .event_changed
                    .wait(events)
                    .unwrap_or_else(|error| error.into_inner());
            }
        };

        if channel.send(event).is_err() {
            let mut events = session
                .events
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if events.channel.as_ref().map(Channel::id) == Some(channel_id) {
                events.channel = None;
                // 发生发送错误时只按客户端最后 ACK 回放，重复事件由 seq 去重。
                events.sent_seq = events.acked_seq;
                events.dispatcher_running = false;
            }
            return;
        }
    }
}

impl RuntimeManager {
    /// 创建新的运行时管理器。macOS GUI 从登录 shell 读取 PATH 时限时 2 秒，
    /// 且只捕获 PATH，不把登录 shell 的其他环境写入应用进程。
    pub fn new(app: Option<AppHandle>) -> Arc<Self> {
        Arc::new(Self {
            app,
            state: Mutex::new(ManagerState {
                sessions: HashMap::new(),
                runtimes: HashMap::new(),
                configs: HashMap::new(),
                shell_numbers: HashMap::new(),
            }),
            session_emit_lock: Mutex::new(()),
            gui_path: command::macos_gui_path(),
            discovery: DiscoveryManager::new(),
        })
    }

    pub fn discovery(&self) -> Arc<DiscoveryManager> {
        self.discovery.clone()
    }

    /// 只用于发现页归属标记；外部停止不会调用该方法，也不会沿父子关系扩大停止范围。
    pub fn managed_processes(&self) -> HashMap<u32, String> {
        let sessions = self
            .state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .sessions
            .values()
            .cloned()
            .collect::<Vec<_>>();
        let mut owners = HashMap::new();
        for session in sessions {
            let terminal = session.snapshot();
            if terminal.state != "running" {
                continue;
            }
            #[cfg(unix)]
            let members = session
                .unix_tracker
                .as_ref()
                .and_then(|tracker| tracker.member_pids().ok())
                .unwrap_or_default();
            #[cfg(windows)]
            let members = session.pid.map(descendant_pids).unwrap_or_default();
            for pid in members {
                owners.insert(pid, terminal.service_id.clone());
            }
        }
        owners
    }

    /// 转换外部实例时先解析 shell 和运行环境，尽量在停止旧实例之前报告配置错误。
    pub fn validate_launch(&self, config: &ServiceConfig) -> Result<(), String> {
        crate::store::validate_service_for_launch(config)?;
        let target = command::build_target(config, SessionKind::Service, None, &self.gui_path)?;
        if command::resolve_executable_in_environment(&target.executable, &target.environment)
            .is_none()
        {
            return Err(format!("找不到用于启动服务的 Shell：{}", target.executable));
        }
        Ok(())
    }

    /// 每个 service_id 最多有一个 service-kind 运行实例；普通 shell 和 Agent 不受该限制。
    pub fn start_service(
        self: &Arc<Self>,
        config: ServiceConfig,
    ) -> Result<TerminalSession, String> {
        if config.source != "managed" || !matches!(config.kind.as_str(), "command" | "frontend") {
            return Err("外部实例不能直接启动，请使用“转换为平台托管并启动”。".into());
        }
        let run_id = Uuid::new_v4().to_string();
        let mut removed_stale_service_output = false;
        {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "运行时状态不可用".to_owned())?;
            if let Some(runtime) = state.runtimes.get(&config.id) {
                if matches!(runtime.status.as_str(), "starting" | "running" | "stopping") {
                    if let Some(existing) = state.sessions.values().find(|session| {
                        let terminal = session.snapshot();
                        terminal.service_id == config.id
                            && terminal.kind == "service"
                            && terminal.state == "running"
                    }) {
                        return Ok(existing.snapshot());
                    }
                    return Err("服务正在启动或停止，请稍后再试。".into());
                }
            }

            // 每次重启后只保留当前服务输出标签；旧 service PTY 已退出才会被回收，
            // 仍在运行的普通 shell 与 Agent 会继续保留。
            let stale_ids = state
                .sessions
                .iter()
                .filter_map(|(id, session)| {
                    let terminal = session.snapshot();
                    (terminal.service_id == config.id
                        && terminal.kind == "service"
                        && terminal.state == "exited")
                        .then(|| id.clone())
                })
                .collect::<Vec<_>>();
            for id in stale_ids {
                if let Some(session) = state.sessions.remove(&id) {
                    session.retired.store(true, Ordering::SeqCst);
                    session.event_changed.notify_all();
                    removed_stale_service_output = true;
                }
            }

            state.configs.insert(config.id.clone(), config.clone());
            state.runtimes.insert(
                config.id.clone(),
                ServiceRuntime {
                    service_id: config.id.clone(),
                    run_id: Some(run_id.clone()),
                    status: "starting".into(),
                    pid: None,
                    ports: Vec::new(),
                    conflicts: Vec::new(),
                    port_error: None,
                    exit_code: None,
                    error: None,
                    started_at: Some(timestamp_now()),
                    ..ServiceRuntime::stopped(&config.id)
                },
            );
        }
        if removed_stale_service_output {
            self.emit_sessions();
        }
        self.emit_runtime_for(&config.id);

        // 即使路径校验或命令检查失败，也先为这次启动尝试建立 runId 和 starting 状态，
        // 再将同一个 runId 收敛到 failed。前端因此能显示本次失败原因，而不会把失效配置
        // 误认为上一次留下的 stopped 状态。活动中的重复启动已在上面幂等返回，不会覆盖
        // 仍在运行实例的状态。
        if let Err(error) = crate::store::validate_service_for_launch(&config) {
            self.fail_start(&config.id, &run_id, &error);
            return Err(error);
        }
        if config.kind == "command" && config.command.trim().is_empty() {
            let error = "服务启动命令不能为空。".to_owned();
            self.fail_start(&config.id, &run_id, &error);
            return Err(error);
        }

        let target =
            match command::build_target(&config, SessionKind::Service, None, &self.gui_path) {
                Ok(target) => target,
                Err(error) => {
                    self.fail_start(&config.id, &run_id, &error);
                    return Err(error);
                }
            };
        let (session, child, reader) = match self.spawn_session(
            &config,
            SessionKind::Service,
            "服务输出".into(),
            Some(run_id.clone()),
            target,
        ) {
            Ok(result) => result,
            Err(error) => {
                self.fail_start(&config.id, &run_id, &error);
                return Err(error);
            }
        };

        {
            let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
            if let Some(runtime) = state.runtimes.get_mut(&config.id) {
                if runtime.run_id.as_deref() == Some(run_id.as_str()) {
                    runtime.status = "running".into();
                    runtime.pid = session.pid;
                }
            }
        }
        self.register_session(session.clone(), child, reader);
        self.emit_runtime_for(&config.id);
        self.start_port_monitor(config.id.clone(), run_id);
        Ok(session.snapshot())
    }

    pub fn stop_service(self: &Arc<Self>, service_id: &str) -> Result<(), String> {
        let session = {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "运行时状态不可用".to_owned())?;
            let runtime = state.runtimes.get_mut(service_id);
            if let Some(runtime) = runtime {
                if runtime.status == "stopped" || runtime.status == "failed" {
                    return Ok(());
                }
                runtime.status = "stopping".into();
            }
            state
                .sessions
                .values()
                .find(|session| {
                    let terminal = session.snapshot();
                    terminal.service_id == service_id
                        && terminal.kind == "service"
                        && terminal.state == "running"
                })
                .cloned()
        };
        self.emit_runtime_for(service_id);

        let result = if let Some(session) = session {
            terminate_session(&session).and_then(|()| {
                if session.wait_for_exit(Duration::from_secs(2)) {
                    Ok(())
                } else {
                    Err("服务进程仍未退出，运行时保留停止中状态。".into())
                }
            })
        } else {
            Ok(())
        };

        if let Err(error) = result {
            let mut state = self
                .state
                .lock()
                .map_err(|_| "运行时状态不可用".to_owned())?;
            if let Some(runtime) = state.runtimes.get_mut(service_id) {
                // 保留 stopping 与 runId，让 UI 可重试；失败信息随 runtime 事件持久可见，
                // 不伪造 failed/exit 状态，也不清空仍可用于重试的 PID。
                if runtime.status != "stopped" {
                    runtime.status = "stopping".into();
                }
                runtime.error = Some(error.clone());
            }
            drop(state);
            self.emit_runtime_for(service_id);
            return Err(error);
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| "运行时状态不可用".to_owned())?;
        if let Some(runtime) = state.runtimes.get_mut(service_id) {
            runtime.status = "stopped".into();
            runtime.pid = None;
            runtime.ports.clear();
            runtime.conflicts.clear();
            runtime.port_error = None;
            runtime.error = None;
        }
        drop(state);
        self.emit_runtime_for(service_id);
        Ok(())
    }

    pub fn restart_service(
        self: &Arc<Self>,
        config: ServiceConfig,
    ) -> Result<TerminalSession, String> {
        // 先确认新配置仍然有效，防止路径失效时先停掉正在工作的旧实例。
        crate::store::validate_service_paths(&config)?;
        self.stop_service(&config.id)?;
        self.start_service(config)
    }

    pub fn create_terminal(
        self: &Arc<Self>,
        config: ServiceConfig,
        agent: Option<AgentProfile>,
    ) -> Result<TerminalSession, String> {
        crate::store::validate_service_paths(&config)?;
        let kind = if agent.is_some() {
            SessionKind::Agent
        } else {
            SessionKind::Shell
        };
        if let Some(agent) = agent.as_ref() {
            if command::resolve_agent(agent, Some(&config), Some(&self.gui_path)).is_none() {
                return Err(format!("找不到 Agent 可执行文件：{}", agent.executable));
            }
        }
        let title = if let Some(agent) = agent.as_ref() {
            agent.name.clone()
        } else {
            let number = {
                let mut state = self
                    .state
                    .lock()
                    .map_err(|_| "运行时状态不可用".to_owned())?;
                let next = state.shell_numbers.entry(config.id.clone()).or_insert(0);
                *next += 1;
                *next
            };
            format!("终端 {number}")
        };
        let run_id = Some(Uuid::new_v4().to_string());
        let target = command::build_target(&config, kind, agent.as_ref(), &self.gui_path)?;
        let (session, child, reader) = self.spawn_session(&config, kind, title, run_id, target)?;
        self.register_session(session.clone(), child, reader);
        Ok(session.snapshot())
    }

    pub fn close_terminal(self: &Arc<Self>, session_id: &str) -> Result<(), String> {
        let session = self.lookup_session(session_id)?;
        terminate_session(&session)?;
        if !session.wait_for_exit(Duration::from_secs(2)) {
            return Err("终端进程仍未退出。".into());
        }
        session.retired.store(true, Ordering::SeqCst);
        session.event_changed.notify_all();
        let service_id = session.snapshot().service_id;
        self.state
            .lock()
            .map_err(|_| "运行时状态不可用".to_owned())?
            .sessions
            .remove(session_id);
        self.emit_sessions();
        self.emit_runtime_for(&service_id);
        Ok(())
    }

    pub fn stop_all(self: &Arc<Self>) -> Result<(), String> {
        let sessions = {
            let state = self
                .state
                .lock()
                .map_err(|_| "运行时状态不可用".to_owned())?;
            state.sessions.values().cloned().collect::<Vec<_>>()
        };
        let mut workers = Vec::with_capacity(sessions.len());
        for session in sessions.iter().cloned() {
            workers.push(thread::spawn(move || terminate_session(&session)));
        }
        let mut first_error: Option<String> = None;
        for worker in workers {
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    first_error.get_or_insert(error);
                }
                Err(_) => {
                    first_error.get_or_insert("清理终端会话时发生线程错误".into());
                }
            };
        }

        if first_error.is_none() {
            if let Some(session) = sessions.iter().find(|session| !session.is_exited()) {
                first_error = Some(format!(
                    "终端会话 {} 尚未退出，保留运行时句柄供重试。",
                    session.snapshot().id
                ));
            }
        }
        if let Some(error) = first_error {
            // 任一会话仍未清理时保留全部管理句柄，防止丢失仍运行进程的重试入口。
            return Err(error);
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| "运行时状态不可用".to_owned())?;
        for session in state.sessions.values() {
            session.retired.store(true, Ordering::SeqCst);
            session.event_changed.notify_all();
        }
        state.sessions.clear();
        for runtime in state.runtimes.values_mut() {
            runtime.status = "stopped".into();
            runtime.pid = None;
            runtime.ports.clear();
            runtime.conflicts.clear();
        }
        let runtime_ids = state.runtimes.keys().cloned().collect::<Vec<_>>();
        drop(state);
        self.emit_sessions();
        for id in runtime_ids {
            self.emit_runtime_for(&id);
        }
        Ok(())
    }

    pub fn remove_service(self: &Arc<Self>, service_id: &str) -> Result<(), String> {
        let sessions = {
            let state = self
                .state
                .lock()
                .map_err(|_| "运行时状态不可用".to_owned())?;
            state
                .sessions
                .values()
                .filter(|session| session.snapshot().service_id == service_id)
                .cloned()
                .collect::<Vec<_>>()
        };
        let mut first_error = None;
        for session in &sessions {
            if let Err(error) = terminate_session(session) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(error) = first_error {
            return Err(error);
        }

        let mut state = self
            .state
            .lock()
            .map_err(|_| "运行时状态不可用".to_owned())?;
        for session in sessions {
            session.retired.store(true, Ordering::SeqCst);
            session.event_changed.notify_all();
            state.sessions.remove(&session.snapshot().id);
        }
        state.configs.remove(service_id);
        state.runtimes.remove(service_id);
        state.shell_numbers.remove(service_id);
        drop(state);
        self.emit_sessions();
        self.emit_runtime(ServiceRuntime::stopped(service_id));
        Ok(())
    }

    pub fn runtimes(&self) -> Vec<ServiceRuntime> {
        let mut runtimes = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtimes
            .values()
            .cloned()
            .collect::<Vec<_>>();
        for runtime in &mut runtimes {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            decorate_managed_runtime(runtime, state.configs.get(&runtime.service_id));
        }
        runtimes.sort_by(|left, right| left.service_id.cmp(&right.service_id));
        runtimes
    }

    pub fn sessions(&self) -> Vec<TerminalSession> {
        let mut sessions = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .values()
            .map(|session| session.snapshot())
            .collect::<Vec<_>>();
        sessions.sort_by(|left, right| {
            left.created_at
                .cmp(&right.created_at)
                .then_with(|| left.id.cmp(&right.id))
        });
        sessions
    }

    pub fn write(&self, session_id: &str, data: &str) -> Result<(), String> {
        let session = self.lookup_session(session_id)?;
        if session.is_exited() {
            return Err("终端会话已结束，无法继续输入。".into());
        }
        #[cfg(unix)]
        {
            let mut writer = session
                .writer
                .lock()
                .map_err(|_| "终端输入通道不可用".to_owned())?;
            let writer = writer
                .as_mut()
                .ok_or_else(|| "终端输入通道已关闭".to_owned())?;
            write_unix_input(&session, writer.as_mut(), data.as_bytes())
        }
        #[cfg(windows)]
        {
            write_windows_input(&session, data.as_bytes())
        }
    }

    pub fn resize(&self, session_id: &str, cols: u16, rows: u16) -> Result<(), String> {
        if cols == 0 || rows == 0 {
            return Err("终端行列尺寸必须大于零。".into());
        }
        let session = self.lookup_session(session_id)?;
        let mut master = session
            .master
            .lock()
            .map_err(|_| "终端尺寸通道不可用".to_owned())?;
        let master = master
            .as_mut()
            .ok_or_else(|| "终端尺寸通道已关闭".to_owned())?;
        let result = master.resize(PtySize {
            rows,
            cols,
            pixel_width: 0,
            pixel_height: 0,
        });
        result.map_err(|error| format!("调整终端尺寸失败：{error}"))
    }

    pub fn subscribe(
        &self,
        session_id: &str,
        channel: Channel<TerminalEvent>,
        after_seq: u64,
    ) -> Result<(), String> {
        let session = self.lookup_session(session_id)?;
        session.subscribe(channel, after_seq);
        Ok(())
    }

    pub fn ack(&self, session_id: &str, seq: u64) -> Result<(), String> {
        self.lookup_session(session_id)?.acknowledge(seq)
    }

    fn lookup_session(&self, id: &str) -> Result<Arc<ManagedSession>, String> {
        self.state
            .lock()
            .map_err(|_| "运行时状态不可用".to_owned())?
            .sessions
            .get(id)
            .cloned()
            .ok_or_else(|| format!("找不到终端会话：{id}"))
    }

    fn spawn_session(
        &self,
        config: &ServiceConfig,
        kind: SessionKind,
        title: String,
        run_id: Option<String>,
        target: command::ProcessTarget,
    ) -> Result<
        (
            Arc<ManagedSession>,
            Box<dyn Child + Send + Sync>,
            Box<dyn Read + Send>,
        ),
        String,
    > {
        #[cfg(unix)]
        unix_process::preflight()?;
        #[cfg(windows)]
        let target = command::wrap_windows_worker(target, &config.working_dir)?;
        let system = native_pty_system();
        let pair = system
            .openpty(PtySize {
                rows: 24,
                cols: 80,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|error| format!("无法创建 PTY 终端：{error}"))?;
        let mut builder = CommandBuilder::new(&target.executable);
        for argument in &target.args {
            builder.arg(argument);
        }
        builder.cwd(&config.working_dir);
        builder.env_clear();
        for (key, value) in &target.environment {
            builder.env(key, value);
        }
        let reader = pair
            .master
            .try_clone_reader()
            .map_err(|error| format!("无法读取终端输出：{error}"))?;
        #[cfg(unix)]
        let unix_pty_fd = pair
            .master
            .as_raw_fd()
            .ok_or_else(|| "PTY 未提供可轮询的 Unix 主端描述符。".to_owned())?;
        #[cfg(unix)]
        set_pty_nonblocking(unix_pty_fd)?;
        let writer = pair
            .master
            .take_writer()
            .map_err(|error| format!("无法打开终端输入：{error}"))?;
        let mut child = pair
            .slave
            .spawn_command(builder)
            .map_err(|error| format!("无法启动终端进程：{error}"))?;
        let pid = child.process_id();
        #[cfg(windows)]
        let killer = child.clone_killer();
        #[cfg(unix)]
        let unix_tracker = match unix_process::SessionTracker::new(pid) {
            Ok(tracker) => Some(tracker),
            Err(error) => match child.try_wait() {
                // 短命命令可能在第一次进程快照前已退出。保留 Child 状态，由 monitor 读取
                // 它的真实退出码并排空 PTY，而不是把正常的快速退出误报成启动失败。
                Ok(Some(_)) => None,
                Ok(None) | Err(_) => {
                    // 仍在运行却无法建立 session 身份时拒绝放弃进程管理；先终止根进程，
                    // 再返回清晰错误，调用方会把本次运行标记为 failed。
                    // child.try_wait()==None 保证 PID 仍属于该 Child；安全确认 session
                    // leader 后优先杀初始进程组，避免 tracker 异常时留下同组子进程。
                    unix_process::emergency_kill(pid);
                    let _ = child.wait();
                    return Err(error);
                }
            },
        };
        let terminal = TerminalSession {
            id: Uuid::new_v4().to_string(),
            service_id: config.id.clone(),
            run_id,
            kind: kind.as_str().into(),
            title,
            pid,
            state: "running".into(),
            exit_code: None,
            created_at: timestamp_now(),
        };
        let mut session = ManagedSession::new(
            terminal,
            writer,
            pair.master,
            #[cfg(windows)]
            killer,
            #[cfg(unix)]
            unix_tracker,
            #[cfg(unix)]
            unix_pty_fd,
        );
        session.pid = pid;
        Ok((Arc::new(session), child, reader))
    }

    fn register_session(
        self: &Arc<Self>,
        session: Arc<ManagedSession>,
        mut child: Box<dyn Child + Send + Sync>,
        reader: Box<dyn Read + Send>,
    ) {
        let id = session.snapshot().id;
        self.state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .sessions
            .insert(id, Arc::clone(&session));
        self.emit_sessions();

        let reader_session = Arc::clone(&session);
        #[cfg(unix)]
        let pty_fd = session.unix_pty_fd;
        let reader_thread = thread::spawn(move || {
            Self::run_reader(
                reader_session,
                reader,
                #[cfg(unix)]
                pty_fd,
            )
        });

        let weak = Arc::downgrade(self);
        let monitor_session = Arc::clone(&session);
        thread::spawn(move || {
            let mut exit_code = 1;
            loop {
                #[cfg(unix)]
                // 先记录 PTY session 中的子进程身份，随后才 try_wait/reap 根进程。
                if let Some(tracker) = monitor_session.unix_tracker.as_ref() {
                    let _ = tracker.refresh();
                }
                match child.try_wait() {
                    Ok(Some(status)) => {
                        exit_code = status.exit_code();
                        break;
                    }
                    Ok(None) => thread::sleep(Duration::from_millis(100)),
                    Err(_) => match child.wait() {
                        Ok(status) => {
                            exit_code = status.exit_code();
                            break;
                        }
                        Err(_) => break,
                    },
                }
            }
            #[cfg(unix)]
            // shell 可以先退出而同一 session 中的子进程仍持有 PTY；清理同 session 的
            // 进程组后 reader 才会 EOF。setsid 脱离的 daemon 不会被 tracker 认领。
            if let Some(tracker) = monitor_session.unix_tracker.as_ref() {
                let _ = tracker.terminate();
            }
            #[cfg(windows)]
            // ConPTY 的 HPCON 需要由 master.drop 触发 ClosePseudoConsole 才会令 reader EOF；
            // 先关闭 master 时 reader 线程仍在排空最终输出，符合旧版 Windows 的关闭要求。
            monitor_session.close_io();
            // 请求 reader 在无数据时退出；最多少量排空 2 秒后继续发送 exit，避免
            // setsid 脱离的 daemon 持有 PTY 时永久卡住监视线程。脱离进程不会被杀死。
            monitor_session
                .reader_drain_requested
                .store(true, Ordering::SeqCst);
            let completion = reader_thread
                .join()
                .unwrap_or(ReaderCompletion::DrainTimeout);
            if completion == ReaderCompletion::DrainTimeout {
                monitor_session.push_data(
                    "\r\n[主进程已退出；后台进程仍持有 PTY，已停止读取其后续输出，未终止该脱离会话的进程。]\r\n"
                        .as_bytes(),
                );
            }
            #[cfg(windows)]
            if exit_code == windows_worker::WORKER_BOOTSTRAP_ERROR_EXIT_CODE {
                monitor_session.push_data(
                    "\r\n[Windows PTY 引导失败：目标进程未启动。请确认 Tauri GUI PTY 已附着到有效 ConPTY，并检查进程隔离引导错误。]\r\n"
                        .as_bytes(),
                );
            }
            monitor_session.close_io();
            monitor_session.push_exit(exit_code);
            if let Some(manager) = weak.upgrade() {
                manager.on_session_exit(&monitor_session, exit_code);
            }
        });
    }

    /// 让 PTY reader 与进程监视分离：运行期间持续排空；根进程清理后，即使脱离 session
    /// 的 daemon 仍持有 slave 端，也只在有限静默窗口内排空，然后关闭 PTY 并报告 exit。
    fn run_reader(
        session: Arc<ManagedSession>,
        mut reader: Box<dyn Read + Send>,
        #[cfg(unix)] pty_fd: RawFd,
    ) -> ReaderCompletion {
        let mut chunk = vec![0; PTY_READ_CHUNK_BYTES];
        #[cfg(unix)]
        let mut drain_started: Option<Instant> = None;
        #[cfg(unix)]
        let mut idle_started: Option<Instant> = None;

        loop {
            #[cfg(unix)]
            {
                if session.reader_drain_requested.load(Ordering::SeqCst) && drain_started.is_none()
                {
                    drain_started = Some(Instant::now());
                }
                if drain_started
                    .map(|started| started.elapsed() >= PTY_DRAIN_MAX)
                    .unwrap_or(false)
                {
                    return ReaderCompletion::DrainTimeout;
                }

                let mut descriptor = libc::pollfd {
                    fd: pty_fd,
                    events: libc::POLLIN,
                    revents: 0,
                };
                let ready = unsafe { libc::poll(&mut descriptor, 1, 100) };
                if ready == 0 {
                    if drain_started.is_some() {
                        let idle = idle_started.get_or_insert_with(Instant::now);
                        if idle.elapsed() >= PTY_DRAIN_IDLE {
                            return ReaderCompletion::DrainTimeout;
                        }
                    }
                    continue;
                }
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    let detail = format!("\r\n[终端输出轮询失败：{error}]\r\n");
                    session.push_data(detail.as_bytes());
                    return ReaderCompletion::EndOfStream;
                }
                if descriptor.revents & libc::POLLNVAL != 0 {
                    return ReaderCompletion::DrainTimeout;
                }
                idle_started = None;
                if descriptor.revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) == 0 {
                    continue;
                }
            }

            match reader.read(&mut chunk) {
                Ok(0) => return ReaderCompletion::EndOfStream,
                Ok(count) => session.push_data(&chunk[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(error) if reader_error_is_eof(&error) => return ReaderCompletion::EndOfStream,
                Err(error) => {
                    let detail = format!("\r\n[终端输出读取失败：{error}]\r\n");
                    session.push_data(detail.as_bytes());
                    return ReaderCompletion::EndOfStream;
                }
            }
        }
    }

    fn fail_start(&self, service_id: &str, run_id: &str, error: &str) {
        if let Ok(mut state) = self.state.lock() {
            if let Some(runtime) = state.runtimes.get_mut(service_id) {
                if runtime.run_id.as_deref() == Some(run_id) {
                    runtime.status = "failed".into();
                    runtime.pid = None;
                    runtime.error = Some(error.to_owned());
                }
            }
        }
        self.emit_runtime_for(service_id);
    }

    fn on_session_exit(&self, session: &ManagedSession, exit_code: u32) {
        self.emit_sessions();
        let terminal = session.snapshot();
        if terminal.kind != "service" {
            return;
        }
        if let Ok(mut state) = self.state.lock() {
            if let Some(runtime) = state.runtimes.get_mut(&terminal.service_id) {
                if runtime.run_id == terminal.run_id {
                    runtime.status =
                        if session.stop_requested.load(Ordering::SeqCst) || exit_code == 0 {
                            "stopped".into()
                        } else {
                            runtime.error = Some(describe_service_exit(exit_code));
                            "failed".into()
                        };
                    runtime.pid = None;
                    runtime.exit_code = Some(exit_code);
                    runtime.ports.clear();
                    runtime.conflicts.clear();
                    runtime.port_error = None;
                }
            }
        }
        self.emit_runtime_for(&terminal.service_id);
    }

    fn start_port_monitor(self: &Arc<Self>, service_id: String, run_id: String) {
        let weak = Arc::downgrade(self);
        thread::spawn(move || loop {
            let Some(manager) = weak.upgrade() else {
                break;
            };
            let (config, root_pid, service_session) = {
                let state = manager
                    .state
                    .lock()
                    .unwrap_or_else(|error| error.into_inner());
                let active = state
                    .runtimes
                    .get(&service_id)
                    .map(|runtime| {
                        runtime.run_id.as_deref() == Some(run_id.as_str())
                            && matches!(runtime.status.as_str(), "running" | "stopping")
                    })
                    .unwrap_or(false);
                if !active {
                    break;
                }
                let config = state.configs.get(&service_id).cloned();
                let root_pid = state
                    .runtimes
                    .get(&service_id)
                    .and_then(|runtime| runtime.pid);
                let service_session = state
                    .sessions
                    .values()
                    .find(|session| {
                        let terminal = session.snapshot();
                        terminal.service_id == service_id
                            && terminal.kind == "service"
                            && terminal.run_id.as_deref() == Some(run_id.as_str())
                    })
                    .cloned();
                (config, root_pid, service_session)
            };
            if let (Some(config), Some(root_pid)) = (config, root_pid) {
                #[cfg(unix)]
                let managed_pids = service_session
                    .as_ref()
                    .and_then(|session| session.unix_tracker.as_ref())
                    .map(|tracker| tracker.member_pids())
                    .transpose();
                #[cfg(unix)]
                let probe = match managed_pids {
                    Ok(managed_pids) => probe_ports(
                        &manager.discovery,
                        root_pid,
                        &config.expected_ports,
                        managed_pids,
                    ),
                    Err(error) => Err(error),
                };
                #[cfg(windows)]
                let probe = probe_ports(&manager.discovery, root_pid, &config.expected_ports, None);
                match probe {
                    Ok((ports, conflicts)) => {
                        if let Ok(mut state) = manager.state.lock() {
                            if let Some(runtime) = state.runtimes.get_mut(&service_id) {
                                if runtime.run_id.as_deref() == Some(run_id.as_str()) {
                                    runtime.ports = ports;
                                    runtime.conflicts = conflicts;
                                    runtime.port_error = None;
                                    runtime.updated_at = Some(timestamp_now());
                                    runtime.stale = false;
                                }
                            }
                        }
                    }
                    Err(error) => {
                        // 探测失败保留最后一次成功结果，只更新错误，避免短暂权限问题抹掉端口。
                        if let Ok(mut state) = manager.state.lock() {
                            if let Some(runtime) = state.runtimes.get_mut(&service_id) {
                                if runtime.run_id.as_deref() == Some(run_id.as_str()) {
                                    runtime.port_error = Some(error);
                                    runtime.stale = true;
                                }
                            }
                        }
                    }
                }
                manager.emit_runtime_for(&service_id);
            }
            drop(manager);
            thread::sleep(SERVICE_PORT_REFRESH);
        });
    }

    fn emit_runtime_for(&self, service_id: &str) {
        let runtime = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .runtimes
            .get(service_id)
            .cloned();
        if let Some(runtime) = runtime {
            self.emit_runtime(runtime);
        }
    }

    fn emit_runtime(&self, mut runtime: ServiceRuntime) {
        {
            let state = self.state.lock().unwrap_or_else(|e| e.into_inner());
            let config = state.configs.get(&runtime.service_id);
            decorate_managed_runtime(&mut runtime, config);
        }
        if let Some(app) = self.app.as_ref() {
            let _ = app.emit("service-runtime", runtime);
        }
    }

    fn emit_sessions(&self) {
        if let Some(app) = self.app.as_ref() {
            let _emission = self
                .session_emit_lock
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let _ = app.emit("terminal-sessions", self.sessions());
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReaderCompletion {
    EndOfStream,
    DrainTimeout,
}

#[derive(Clone, Copy)]
enum SessionKind {
    Service,
    Shell,
    Agent,
}

impl SessionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Shell => "shell",
            Self::Agent => "agent",
        }
    }
}

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(unix)]
fn set_pty_nonblocking(fd: RawFd) -> Result<(), String> {
    // portable-pty 的 master、reader 与 writer 都是同一 PTY open-file-description 的副本，
    // O_NONBLOCK 会避免满输入队列把 Rust 的 write_all 卡住。reader 先 poll 再读，仍会
    // 持续排空输出；WouldBlock 由读写调用方按各自策略处理。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(format!(
            "无法读取 PTY 文件状态标志：{}",
            std::io::Error::last_os_error()
        ));
    }
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(format!(
            "无法启用 PTY 非阻塞输入：{}",
            std::io::Error::last_os_error()
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn reader_error_is_eof(error: &std::io::Error) -> bool {
    // macOS PTY 在 slave 端最后一个描述符关闭时以 EIO 表示 EOF。
    error.raw_os_error() == Some(libc::EIO)
}

#[cfg(windows)]
fn describe_service_exit(exit_code: u32) -> String {
    if exit_code == windows_worker::WORKER_BOOTSTRAP_ERROR_EXIT_CODE {
        "Windows PTY 引导失败，目标进程未启动。请确认 ConPTY 已附着，并检查终端引导输出。".into()
    } else {
        format!("服务进程异常退出，退出代码：{exit_code}")
    }
}

#[cfg(not(windows))]
fn describe_service_exit(exit_code: u32) -> String {
    format!("服务进程异常退出，退出代码：{exit_code}")
}

#[cfg(not(unix))]
fn reader_error_is_eof(_: &std::io::Error) -> bool {
    false
}

#[cfg(unix)]
fn write_unix_input(
    session: &ManagedSession,
    writer: &mut dyn Write,
    bytes: &[u8],
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut written = 0usize;
    while written < bytes.len() {
        if session.stop_requested.load(Ordering::SeqCst)
            || session.reader_drain_requested.load(Ordering::SeqCst)
            || session.is_exited()
        {
            return Err(format!(
                "终端已开始停止；本次输入已写入 {written}/{} 字节。",
                bytes.len()
            ));
        }
        match writer.write(&bytes[written..]) {
            Ok(0) => {
                return Err(format!(
                    "终端输入通道已关闭；本次输入已写入 {written}/{} 字节。",
                    bytes.len()
                ));
            }
            Ok(count) => written = written.saturating_add(count),
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(format!(
                        "终端输入拥塞超时；本次输入已写入 {written}/{} 字节，剩余内容未完整发送。",
                        bytes.len()
                    ));
                }
                let mut descriptor = libc::pollfd {
                    fd: session.unix_pty_fd,
                    events: libc::POLLOUT,
                    revents: 0,
                };
                let wait_ms = deadline
                    .saturating_duration_since(now)
                    .min(Duration::from_millis(50))
                    .as_millis()
                    .max(1) as i32;
                let ready = unsafe { libc::poll(&mut descriptor, 1, wait_ms) };
                if ready < 0 {
                    let error = std::io::Error::last_os_error();
                    if error.kind() == std::io::ErrorKind::Interrupted {
                        continue;
                    }
                    return Err(format!(
                        "等待终端输入缓冲失败（已写入 {written}/{} 字节）：{error}",
                        bytes.len()
                    ));
                }
                if descriptor.revents & (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) != 0 {
                    return Err(format!(
                        "终端输入通道已关闭（已写入 {written}/{} 字节）。",
                        bytes.len()
                    ));
                }
            }
            Err(error) => {
                return Err(format!(
                    "写入终端失败（已写入 {written}/{} 字节）：{error}",
                    bytes.len()
                ));
            }
        }
    }
    writer
        .flush()
        .map_err(|error| format!("刷新终端输入失败（已写入 {written} 字节）：{error}"))
}

#[cfg(windows)]
fn write_windows_input(session: &Arc<ManagedSession>, bytes: &[u8]) -> Result<(), String> {
    const INPUT_WRITE_TIMEOUT: Duration = Duration::from_secs(2);
    let writer = Arc::clone(&session.writer);
    let session = Arc::clone(session);
    let input = bytes.to_vec();
    let progress = Arc::new(AtomicUsize::new(0));
    let worker_progress = Arc::clone(&progress);
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);

    // Windows ConPTY 的同步输入管道可能在慢消费者前阻塞；将写入隔离到工作线程，
    // Tauri 命令有界等待，stop 即可独立等待五秒并关闭 Job Object。
    drop(thread::spawn(move || {
        let result = (|| {
            let mut writer = match writer.try_lock() {
                Ok(writer) => writer,
                Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
                Err(std::sync::TryLockError::WouldBlock) => {
                    return Err("另一笔终端输入仍在处理中，请稍后重试。".to_owned());
                }
            };
            let writer = writer
                .as_mut()
                .ok_or_else(|| "终端输入通道已关闭".to_owned())?;
            let deadline = Instant::now() + INPUT_WRITE_TIMEOUT;
            while worker_progress.load(Ordering::SeqCst) < input.len() {
                let offset = worker_progress.load(Ordering::SeqCst);
                if session.stop_requested.load(Ordering::SeqCst)
                    || session.reader_drain_requested.load(Ordering::SeqCst)
                    || session.is_exited()
                {
                    return Err(format!(
                        "终端已开始停止；本次输入已写入 {offset}/{} 字节。",
                        input.len()
                    ));
                }
                if Instant::now() >= deadline {
                    return Err(format!(
                        "终端输入拥塞超时；本次输入已写入 {offset}/{} 字节，余下内容未完整发送。",
                        input.len()
                    ));
                }
                match writer.write(&input[offset..]) {
                    Ok(0) => {
                        return Err(format!(
                            "终端输入通道已关闭；本次输入已写入 {offset}/{} 字节。",
                            input.len()
                        ));
                    }
                    Ok(count) => {
                        worker_progress.store(offset.saturating_add(count), Ordering::SeqCst)
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(10));
                    }
                    Err(error) => {
                        return Err(format!(
                            "写入 Windows 终端失败（已写入 {offset}/{} 字节）：{error}",
                            input.len()
                        ));
                    }
                }
            }
            writer
                .flush()
                .map_err(|error| format!("刷新 Windows 终端输入失败：{error}"))
        })();
        let _ = sender.send(result);
    }));

    match receiver.recv_timeout(INPUT_WRITE_TIMEOUT) {
        Ok(result) => result,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "Windows 终端输入超过两秒仍未完成；已写入 {}/{} 字节，其余内容可能未发送。",
            progress.load(Ordering::SeqCst),
            bytes.len()
        )),
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            Err("Windows 终端输入线程已意外结束。".into())
        }
    }
}

fn terminate_session(session: &ManagedSession) -> Result<(), String> {
    if session.is_exited() {
        return Ok(());
    }
    session.stop_requested.store(true, Ordering::SeqCst);
    send_ctrl_c_best_effort(session);

    #[cfg(unix)]
    {
        if let Some(tracker) = session.unix_tracker.as_ref() {
            tracker.terminate()?;
        } else if !session.wait_for_exit(Duration::from_millis(250)) {
            // 仅用于刚创建即退出、来不及获得 PID 启动标识的短命进程；若它并未退出，
            // 不以不安全的裸 PID 猜测进程组身份。
            return Err("短命 PTY 进程缺少安全 session 身份，无法确认其进程树已退出。".into());
        }
        if session.wait_for_exit(Duration::from_secs(2)) {
            return Ok(());
        }
        return Err("无法在温和停止和进程组清理后确认终端退出。".into());
    }

    #[cfg(windows)]
    {
        if session.wait_for_exit(Duration::from_secs(5)) {
            return Ok(());
        }
        // Windows 目标进程在 helper 创建时已经加入独立 Job；终止 helper 会关闭其最后
        // 一个 Job 句柄，JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE 会一并终止整个受管树。
        let kill_result = session.killer.clone_killer().kill();
        if session.wait_for_exit(Duration::from_secs(3)) {
            Ok(())
        } else if let Err(error) = kill_result {
            Err(format!("无法终止 Windows PTY 引导进程：{error}"))
        } else {
            Err("Windows 受管进程树仍未退出。".into())
        }
    }
}

fn send_ctrl_c_best_effort(session: &ManagedSession) {
    #[cfg(unix)]
    {
        // Unix master 已启用 O_NONBLOCK；不等待其他输入调用，也不因满 PTY 队列阻塞停止。
        if let Ok(mut writer) = session.writer.try_lock() {
            if let Some(writer) = writer.as_mut() {
                let _ = writer.write(&[0x03]);
            }
        }
    }

    #[cfg(windows)]
    {
        // Windows ConPTY 管道可能受慢消费者影响。将单字节中断放在独立线程中尽力写入，
        // 主停止路径仍会按时等候五秒，然后通过 helper Job Object 强制清理目标进程树。
        let writer = Arc::clone(&session.writer);
        drop(thread::spawn(move || {
            let Ok(mut writer) = writer.try_lock() else {
                return;
            };
            if let Some(writer) = writer.as_mut() {
                let _ = writer.write(&[0x03]);
            }
        }));
    }
}

fn probe_ports(
    discovery: &DiscoveryManager,
    root_pid: u32,
    expected_ports: &[u16],
    managed_hint: Option<HashSet<u32>>,
) -> Result<(Vec<PortInfo>, Vec<PortInfo>), String> {
    let managed = managed_hint.unwrap_or_else(|| descendant_pids(root_pid));
    // 发现页、外部实例和平台托管进程复用同一份带 TTL 的快照；扫描失败不清空旧结果。
    let sockets = discovery.ports(false)?;
    let expected = expected_ports.iter().copied().collect::<HashSet<_>>();
    let mut owned = HashSet::new();
    let mut external = HashSet::new();
    for port in sockets {
        if port.pid.is_some_and(|pid| managed.contains(&pid)) {
            owned.insert(port);
        } else if expected.contains(&port.port) {
            external.insert(port);
        }
    }
    let mut ports = owned.into_iter().collect::<Vec<_>>();
    let mut conflicts = external.into_iter().collect::<Vec<_>>();
    sort_ports(&mut ports);
    sort_ports(&mut conflicts);
    Ok((ports, conflicts))
}

fn decorate_managed_runtime(runtime: &mut ServiceRuntime, config: Option<&ServiceConfig>) {
    runtime.can_stop = matches!(runtime.status.as_str(), "starting" | "running" | "stopping");
    runtime.can_start = matches!(runtime.status.as_str(), "stopped" | "failed");
    runtime.can_restart = runtime.status == "running";
    runtime.web_urls.clear();
    if let Some(ServiceConfig {
        launch: crate::models::LaunchConfig::Frontend { url, .. },
        ..
    }) = config
    {
        if !url.is_empty() {
            runtime.web_urls.push(url.clone());
        } else if runtime.status == "running" && !runtime.stale {
            // 只使用本实例的实际 TCP 监听结果；预期端口和其他进程的冲突端口不能生成入口。
            let urls = runtime
                .ports
                .iter()
                .filter(|p| p.protocol.eq_ignore_ascii_case("tcp"))
                .filter_map(|port| {
                    let address = port.address.parse::<std::net::IpAddr>().ok()?;
                    let host = match address {
                        std::net::IpAddr::V4(ip) if ip.is_unspecified() => "127.0.0.1".into(),
                        std::net::IpAddr::V4(ip) => ip.to_string(),
                        std::net::IpAddr::V6(ip) if ip.is_unspecified() => "[::1]".into(),
                        std::net::IpAddr::V6(ip) => format!("[{ip}]"),
                    };
                    Some(format!("http://{host}:{}", port.port))
                })
                .collect::<std::collections::BTreeSet<_>>();
            runtime.web_urls.extend(urls);
        }
    }
}

fn sort_ports(ports: &mut [PortInfo]) {
    ports.sort_by(|left, right| {
        left.port
            .cmp(&right.port)
            .then_with(|| left.protocol.cmp(&right.protocol))
            .then_with(|| left.address.cmp(&right.address))
            .then_with(|| left.pid.cmp(&right.pid))
    });
}

/// 以进程父子关系拓展根 PID，供端口所有权识别使用；根本身始终保留，即使系统快照漏掉它。
fn descendant_pids(root_pid: u32) -> HashSet<u32> {
    let mut system = System::new();
    system.refresh_processes(ProcessesToUpdate::All, true);
    let root = Pid::from_u32(root_pid);
    let mut managed = HashSet::from([root_pid]);
    let mut frontier = vec![root];
    while let Some(parent) = frontier.pop() {
        for (pid, process) in system.processes() {
            if process.parent() == Some(parent) {
                let child = pid.as_u32();
                if managed.insert(child) {
                    frontier.push(*pid);
                }
            }
        }
    }
    managed
}

/// 只检查 Agent 可执行文件是否存在且可执行；绝不启动 CLI 或触碰登录/安装流程。
pub fn check_agent(agent: &AgentProfile, config: Option<&ServiceConfig>) -> AgentAvailability {
    match command::resolve_agent(agent, config, None) {
        Some(path) => AgentAvailability {
            available: true,
            resolved_path: Some(path.to_string_lossy().into_owned()),
            error: None,
        },
        None => AgentAvailability {
            available: false,
            resolved_path: None,
            error: Some(format!("找不到可执行文件：{}", agent.executable)),
        },
    }
}

/// Tauri 主入口最先调用此函数。Windows PTY 引导进程只负责启动已隔离的目标程序，
/// 不初始化第二个 Tauri 实例，也不读取或打印 Agent 的环境变量。
pub fn run_worker_if_requested() -> bool {
    #[cfg(windows)]
    {
        if std::env::args_os().nth(1).as_deref()
            == Some(std::ffi::OsStr::new(windows_worker::WORKER_ARGUMENT))
        {
            let code = match windows_worker::run_from_environment() {
                Ok(code) => code,
                Err(error) => {
                    eprintln!("Windows 终端引导失败：{error}");
                    windows_worker::WORKER_BOOTSTRAP_ERROR_EXIT_CODE
                }
            };
            std::process::exit(code as i32);
        }
    }
    false
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct WorkerSpec {
    executable: String,
    args: Vec<String>,
    working_dir: String,
    environment: HashMap<String, String>,
}
