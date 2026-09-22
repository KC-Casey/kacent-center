//! 在临时目录中运行真实服务，验证目录/环境/PTY/端口/子进程隔离，不接触用户服务。
use kacent_center_lib::{
    models::*,
    runtime::{self, RuntimeManager},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::{TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use tauri::ipc::{Channel, InvokeResponseBody};
use uuid::Uuid;

fn eventually(
    label: &str,
    seconds: u64,
    mut predicate: impl FnMut() -> bool,
) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline {
        if predicate() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("验证超时：{label}"))
}
fn quote(path: &Path) -> String {
    let path = path.to_string_lossy();
    if cfg!(windows) {
        format!("& '{}'", path.replace('\'', "''"))
    } else {
        format!("'{}'", path.replace('\'', "'\\''"))
    }
}
fn config(root: &Path, name: &str) -> ServiceConfig {
    ServiceConfig {
        id: String::new(),
        kind: "command".into(),
        launch: kacent_center_lib::models::LaunchConfig::Command,
        source: "managed".into(),
        external: None,
        name: name.into(),
        description: "独立临时验收服务".into(),
        working_dir: root.to_string_lossy().into(),
        environment_type: "system".into(),
        environment_dir: String::new(),
        executable_dirs: vec![],
        env: BTreeMap::from([("KACENT_SMOKE_LABEL".into(), name.into())]),
        command: format!("{} --fixture", quote(&std::env::current_exe().unwrap())),
        shell: String::new(),
        expected_ports: vec![],
        notes: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
    }
}
fn fixture() -> Result<(), String> {
    let listener = TcpListener::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--child")
        .spawn()
        .map_err(|e| e.to_string())?;
    let cwd = std::env::current_dir().unwrap();
    let metadata = json!({"port":listener.local_addr().unwrap().port(),"udpPort":udp.local_addr().unwrap().port(),"pid":std::process::id(),"childPid":child.id(),"cwd":cwd,"label":std::env::var("KACENT_SMOKE_LABEL").unwrap_or_default(),"virtualEnv":std::env::var("VIRTUAL_ENV").unwrap_or_default()});
    std::fs::write("fixture.json", metadata.to_string()).map_err(|e| e.to_string())?;
    println!("服务已启动：{}", metadata["label"]);
    // 让输出远大于后端缓存。没有任何前端订阅时，服务仍必须完成输出并响应 TCP。
    let output = format!("{}\r\n", "输出压力验证".repeat(100));
    for _ in 0..5000 {
        print!("{output}");
    }
    println!("压力输出完成");
    std::io::stdout().flush().unwrap();
    for connection in listener.incoming() {
        let mut stream = connection.map_err(|e| e.to_string())?;
        let _ = stream.write_all(b"ready");
    }
    Ok(())
}
fn metadata(dir: &Path) -> Option<Value> {
    std::fs::read_to_string(dir.join("fixture.json"))
        .ok()
        .and_then(|v| serde_json::from_str(&v).ok())
}
fn responds(port: u16) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{port}").parse().unwrap(),
        Duration::from_millis(300),
    )
    .ok()
    .is_some_and(|mut stream| {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
        let mut bytes = [0; 5];
        stream.read_exact(&mut bytes).is_ok() && bytes == *b"ready"
    })
}
fn process_alive(pid: u32) -> bool {
    let system = sysinfo::System::new_all();
    system
        .process(sysinfo::Pid::from_u32(pid))
        .is_some_and(|process| process.status() != sysinfo::ProcessStatus::Zombie)
}

#[cfg(unix)]
struct EscapedFixtureGuard(u32);
#[cfg(unix)]
impl Drop for EscapedFixtureGuard {
    fn drop(&mut self) {
        // 只清理本验收程序主动创建的脱离会话子进程，并再次核实 executable，避免误触其他程序。
        // 这是验收清理，不扩大正式 RuntimeManager 对 daemon 的管理范围。
        let system = sysinfo::System::new_all();
        if system
            .process(sysinfo::Pid::from_u32(self.0))
            .is_some_and(|process| process.exe() == std::env::current_exe().ok().as_deref())
        {
            unsafe {
                libc::kill(self.0 as i32, libc::SIGKILL);
            }
        }
    }
}
fn capture(
    manager: &Arc<RuntimeManager>,
    session: &TerminalSession,
) -> Result<Arc<Mutex<Vec<Value>>>, String> {
    let events = Arc::new(Mutex::new(Vec::new()));
    let output = events.clone();
    let channel = Channel::<TerminalEvent>::new(move |message| {
        if let InvokeResponseBody::Json(body) = message {
            if let Ok(value) = serde_json::from_str(&body) {
                output.lock().unwrap().push(value);
            }
        }
        Ok(())
    });
    manager.subscribe(&session.id, channel, 0)?;
    Ok(events)
}
fn acknowledge(
    manager: &RuntimeManager,
    session: &TerminalSession,
    events: &Arc<Mutex<Vec<Value>>>,
) {
    if let Some(seq) = events
        .lock()
        .unwrap()
        .last()
        .and_then(|event| event["seq"].as_u64())
    {
        let _ = manager.ack(&session.id, seq);
    }
}
fn text(events: &Arc<Mutex<Vec<Value>>>) -> String {
    let bytes: Vec<u8> = events
        .lock()
        .unwrap()
        .iter()
        .flat_map(|event| event["data"].as_array().cloned().unwrap_or_default())
        .filter_map(|v| v.as_u64().map(|v| v as u8))
        .collect();
    String::from_utf8_lossy(&bytes).to_string()
}
fn run_checks(manager: &Arc<RuntimeManager>, base: &Path) -> Result<(), String> {
    let dir_a = base.join("服务 A 中文目录");
    let dir_b = base.join("服务 B 中文目录");
    std::fs::create_dir_all(&dir_a).unwrap();
    std::fs::create_dir_all(&dir_b).unwrap();
    let store = Store::open(&base.join("验收.sqlite3"))?;
    assert!(store.services()?.is_empty());
    assert_eq!(store.agents()?.len(), 3);
    let mut config_a = config(&dir_a, "服务甲");
    let python = which::which("python3")
        .or_else(|_| which::which("python"))
        .ok();
    let venv = base.join("独立 Python 环境");
    let has_venv = if let Some(python) = python {
        let status = Command::new(python)
            .args(["-m", "venv", "--without-pip"])
            .arg(&venv)
            .status()
            .map_err(|e| format!("创建隔离 Python 验收环境失败：{e}"))?;
        if !status.success() {
            return Err("Python 虚拟环境创建失败，不能把系统 Python 当成虚拟环境验收".into());
        }
        config_a.environment_type = "venv".into();
        config_a.environment_dir = venv.to_string_lossy().into();
        true
    } else {
        println!("未验证：本机未安装 Python，跳过已有 venv 链路");
        false
    };
    let a = store.save_service(config_a)?;
    let b = store.save_service(config(&dir_b, "服务乙"))?;
    let document = store.save_document(ServiceDocument {
        id: String::new(),
        service_id: a.id.clone(),
        title: "中文文档".into(),
        content: "# 验收\n\n文档内容保存成功。".into(),
        updated_at: String::new(),
    })?;
    assert_eq!(store.document(&document.id)?.content, document.content);
    println!("通过：空数据库、默认 Agent 配置、服务与中文文档持久化");
    let session_a = manager.start_service(a.clone())?;
    let session_b = manager.start_service(b.clone())?;
    if let Ok(repeated) = manager.start_service(a.clone()) {
        assert_eq!(repeated.id, session_a.id, "重复启动只能复用已有实例");
    }
    assert_eq!(
        manager
            .sessions()
            .iter()
            .filter(|session| session.service_id == a.id
                && session.kind == "service"
                && session.state == "running")
            .count(),
        1
    );
    eventually("两个服务写出启动信息", 15, || {
        metadata(&dir_a).is_some() && metadata(&dir_b).is_some()
    })?;
    let ma = metadata(&dir_a).unwrap();
    let mb = metadata(&dir_b).unwrap();
    assert_eq!(ma["label"], "服务甲");
    assert_eq!(mb["label"], "服务乙");
    assert_eq!(
        Path::new(ma["cwd"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        dir_a.canonicalize().unwrap()
    );
    if has_venv {
        assert_eq!(
            ma["virtualEnv"].as_str(),
            Some(venv.to_string_lossy().as_ref())
        );
    }
    let port_a = ma["port"].as_u64().unwrap() as u16;
    let port_b = mb["port"].as_u64().unwrap() as u16;
    eventually(
        "无前端消费者的大量输出不阻塞服务",
        20,
        || responds(port_a) && responds(port_b),
    )?;
    println!("通过：两服务中文含空格目录、环境隔离、重复启动保护、大量输出不中断服务");
    eventually("实际端口归属", 15, || {
        let snapshots = manager.runtimes();
        snapshots.iter().any(|r| {
            r.service_id == a.id
                && r.ports
                    .iter()
                    .any(|p| p.port == port_a && p.protocol == "TCP")
                && r.ports.iter().any(|p| {
                    p.port == ma["udpPort"].as_u64().unwrap() as u16 && p.protocol == "UDP"
                })
        }) && snapshots
            .iter()
            .any(|r| r.service_id == b.id && r.ports.iter().any(|p| p.port == port_b))
    })?;
    println!("通过：两个进程树的 TCP 与 UDP 端口识别互不串扰");
    let mut conflicting = config(&dir_b, "端口冲突校验");
    conflicting.id = Uuid::new_v4().to_string();
    conflicting.command = format!("{} --child", quote(&std::env::current_exe().unwrap()));
    conflicting.expected_ports = vec![port_b];
    manager.start_service(conflicting.clone())?;
    eventually("其他服务占用的端口只能计入冲突", 12, || {
        manager.runtimes().iter().any(|r| {
            r.service_id == conflicting.id
                && r.ports.is_empty()
                && r.conflicts.iter().any(|p| p.port == port_b)
        })
    })?;
    manager.stop_service(&conflicting.id)?;
    let mut invalid = config(&base.join("不存在的目录"), "目录失效校验");
    invalid.id = Uuid::new_v4().to_string();
    assert!(manager.start_service(invalid.clone()).is_err());
    assert!(manager
        .runtimes()
        .iter()
        .any(|r| r.service_id == invalid.id && r.status == "failed" && r.error.is_some()));
    let mut failed = config(&dir_b, "命令失败校验");
    failed.id = Uuid::new_v4().to_string();
    failed.command = "exit 7".into();
    let _ = manager.start_service(failed.clone());
    eventually("快速异常退出不会回写为运行中", 8, || {
        manager
            .runtimes()
            .iter()
            .any(|r| r.service_id == failed.id && r.status == "failed" && r.exit_code == Some(7))
    })?;
    println!("通过：端口冲突与归属区分、失效目录、快速命令失败状态");
    let events_a = capture(manager, &session_a)?;
    let events_b = capture(manager, &session_b)?;
    let restored = eventually("有界缓存恢复输出", 20, || {
        acknowledge(manager, &session_a, &events_a);
        acknowledge(manager, &session_b, &events_b);
        text(&events_a).contains("压力输出完成") && text(&events_b).contains("压力输出完成")
    });
    if let Err(error) = restored {
        for (name, events) in [("甲", &events_a), ("乙", &events_b)] {
            let guard = events.lock().unwrap();
            eprintln!(
                "缓存诊断{name}：收到{}个事件；末尾序号={:?}，类型={:?}",
                guard.len(),
                guard.last().map(|event| &event["seq"]),
                guard.last().map(|event| &event["kind"])
            );
            drop(guard);
            let output = text(events);
            let tail: String = output
                .chars()
                .rev()
                .take(100)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            eprintln!("缓存末尾{name}：{tail:?}");
        }
        return Err(error);
    }
    for (events, session) in [(&events_a, &session_a), (&events_b, &session_b)] {
        let guard = events.lock().unwrap();
        assert!(guard
            .iter()
            .all(|event| event["serviceId"] == session.service_id
                && event["sessionId"] == session.id));
        assert!(guard
            .windows(2)
            .all(|pair| pair[0]["seq"].as_u64() < pair[1]["seq"].as_u64()));
        assert!(
            guard.iter().any(|event| event["kind"] == "gap"),
            "缓存溢出必须明确报告缺口"
        );
    }
    println!("通过：终端消息身份、有序消费、缓存截断提示");
    let shell = manager.create_terminal(a.clone(), None)?;
    let shell_events = capture(manager, &shell)?;
    manager.write(
        &shell.id,
        if cfg!(windows) {
            "Write-Output ('独立' + '终端已就绪')\r"
        } else {
            "printf '独立%s\\n' '终端已就绪'\r"
        },
    )?;
    eventually("交互终端中文输入", 8, || {
        acknowledge(manager, &shell, &shell_events);
        text(&shell_events).contains("独立终端已就绪")
    })?;
    if has_venv {
        manager.write(&shell.id, "python -c \"import sys; print(sys.prefix)\"\r")?;
        eventually("交互 shell 保留已配置 Python 环境", 8, || {
            acknowledge(manager, &shell, &shell_events);
            let output = text(&shell_events);
            output.contains(venv.to_string_lossy().as_ref())
                || output.contains(venv.canonicalize().unwrap().to_string_lossy().as_ref())
        })?;
        println!("通过：服务及独立交互终端均使用已有 Python venv");
    }
    manager.resize(&shell.id, 120, 32)?;
    manager.stop_service(&a.id)?;
    eventually("停止甲不影响乙", 8, || {
        !responds(port_a) && responds(port_b)
    })?;
    eventually("回收服务甲及其子进程", 8, || {
        !process_alive(ma["pid"].as_u64().unwrap() as u32)
            && !process_alive(ma["childPid"].as_u64().unwrap() as u32)
    })?;
    assert!(manager
        .sessions()
        .iter()
        .any(|session| session.id == shell.id && session.state == "running"));
    println!("通过：服务停止不影响其他服务或该服务的独立终端");
    let old_run = session_a.run_id;
    let restart = manager.start_service(a.clone())?;
    assert_ne!(restart.run_id, old_run);
    manager.stop_service(&a.id)?;
    manager.close_terminal(&shell.id)?;
    #[cfg(unix)]
    {
        let mut stubborn = config(&dir_a, "强制清理校验");
        stubborn.id = Uuid::new_v4().to_string();
        stubborn.command = format!("{} --stubborn", quote(&std::env::current_exe().unwrap()));
        manager.start_service(stubborn.clone())?;
        eventually("忽略软中断的测试进程已准备", 8, || {
            dir_a.join("stubborn.pid").is_file()
        })?;
        let pid: u32 = std::fs::read_to_string(dir_a.join("stubborn.pid"))
            .unwrap()
            .parse()
            .unwrap();
        // 子进程将 PTY 设为 raw 且不读取 stdin。大段粘贴填满输入缓冲后，停止操作
        // 仍必须到达五秒后的进程清理，不能卡在与输入请求共享的 writer 锁上。
        let writer_manager = manager.clone();
        let stubborn_session = manager
            .sessions()
            .into_iter()
            .find(|session| session.service_id == stubborn.id)
            .unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let result = writer_manager.write(&stubborn_session.id, &"x".repeat(512 * 1024));
            let _ = sent.send(result);
        });
        thread::sleep(Duration::from_millis(150));
        let started = Instant::now();
        manager.stop_service(&stubborn.id)?;
        assert!(
            started.elapsed() >= Duration::from_secs(5),
            "强制清理前应保留五秒温和退出时间"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "输入拥塞不能阻止停止流程"
        );
        let _ = received
            .recv_timeout(Duration::from_secs(3))
            .map_err(|_| "停止后终端输入请求仍未释放".to_owned())?;
        eventually("五秒后强制清理残留进程", 5, || {
            !process_alive(pid)
        })?;
        println!("通过：输入拥塞不阻止停止，忽略中断的进程在等待五秒后被强制清理");
        let mut detached = config(&dir_a, "终端排空边界校验");
        detached.id = Uuid::new_v4().to_string();
        detached.command = format!(
            "{} --detached-parent",
            quote(&std::env::current_exe().unwrap())
        );
        manager.start_service(detached.clone())?;
        eventually("脱离会话的子进程已准备", 8, || {
            dir_a.join("escaped.pid").is_file()
        })?;
        let pid: u32 = std::fs::read_to_string(dir_a.join("escaped.pid"))
            .unwrap()
            .parse()
            .unwrap();
        let cleanup = EscapedFixtureGuard(pid);
        let started = Instant::now();
        manager.stop_service(&detached.id)?;
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "脱离会话但持有PTY不能导致无界等待"
        );
        assert!(
            process_alive(pid),
            "正式运行时不能越过管理范围误杀已脱离会话的进程"
        );
        drop(cleanup);
        println!("通过：脱离会话的后台进程不会阻止终端结束，且未被越界终止");
    }
    let available = store
        .agents()?
        .into_iter()
        .find(|agent| runtime::check_agent(agent, Some(&b)).available);
    if let Some(mut agent) = available {
        agent.args = vec!["--version".into()];
        let version = manager.create_terminal(b.clone(), Some(agent.clone()))?;
        let events = capture(manager, &version)?;
        eventually("真实 Agent CLI 版本调用", 20, || {
            acknowledge(manager, &version, &events);
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "exit")
        })?;
        assert!(!text(&events).trim().is_empty());
        assert!(
            events
                .lock()
                .unwrap()
                .iter()
                .any(|event| event["kind"] == "exit" && event["exitCode"] == 0),
            "真实 Agent 版本调用必须成功退出"
        );
        println!(
            "通过：真实 {} CLI 在独立 PTY 中执行 --version（未进行模型请求）",
            agent.name
        );
    } else {
        println!("未验证：本机没有找到可用于检查的 Agent CLI");
    }
    manager.stop_all()?;
    eventually("退出停止全部服务", 8, || !responds(port_b))?;
    assert!(manager
        .sessions()
        .iter()
        .all(|session| session.state == "exited"));
    store.delete_service(&a.id)?;
    assert!(store.documents(&a.id).unwrap_or_default().is_empty());
    println!("通过：统一退出清理、服务记录级联删除");
    Ok(())
}
fn main() {
    if runtime::run_worker_if_requested() {
        return;
    }
    let args: Vec<_> = std::env::args().collect();
    #[cfg(unix)]
    if args.iter().any(|arg| arg == "--detached-parent") {
        use std::os::unix::process::CommandExt;
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.arg("--escaped-child");
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() < 0 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        let _child = command.spawn().unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    #[cfg(unix)]
    if args.iter().any(|arg| arg == "--escaped-child") {
        unsafe {
            libc::signal(libc::SIGHUP, libc::SIG_IGN);
        }
        std::fs::write("escaped.pid", std::process::id().to_string()).unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    #[cfg(unix)]
    if args.iter().any(|arg| arg == "--stubborn") {
        // 仅此验收子进程忽略信号，用来验证超时升级为 SIGKILL 的真实路径。
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_IGN);
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        }
        unsafe {
            let mut attributes: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut attributes) == 0 {
                libc::cfmakeraw(&mut attributes);
                libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &attributes);
            }
        }
        std::fs::write("stubborn.pid", std::process::id().to_string()).unwrap();
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    if args.iter().any(|arg| arg == "--child") {
        loop {
            thread::sleep(Duration::from_secs(1));
        }
    }
    if args.iter().any(|arg| arg == "--fixture") {
        if let Err(error) = fixture() {
            eprintln!("{error}");
            std::process::exit(1);
        }
        return;
    }
    let base: PathBuf =
        std::env::temp_dir().join(format!("kacent-runtime-smoke-{}", Uuid::new_v4()));
    let manager = RuntimeManager::new(None);
    let result =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_checks(&manager, &base)))
            .unwrap_or_else(|_| Err("验收断言未通过，请查看上方具体信息".into()));
    // 包括断言失败在内的所有路径都清理本次创建的进程，避免验收本身残留服务。
    let cleanup = manager.stop_all();
    match result.and(cleanup) {
        Ok(()) => {
            let _ = std::fs::remove_dir_all(&base);
            println!("真实进程验收完成");
        }
        Err(error) => {
            eprintln!("验收失败：{error}\n隔离数据保留在 {}", base.display());
            std::process::exit(1);
        }
    }
}
