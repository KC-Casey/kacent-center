//! 仅操作本验收程序创建的临时服务：发现、接入、身份拒绝、停止、重开和转换托管。
use kacent_center_lib::{
    external::ExternalManager,
    models::*,
    runtime::{self, RuntimeManager},
    store::Store,
};
use serde_json::{json, Value};
use std::{
    fs,
    net::{TcpListener, TcpStream, UdpSocket},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

struct Fixture(Child);
struct ManagedCleanup(Arc<RuntimeManager>);
impl Drop for ManagedCleanup {
    fn drop(&mut self) {
        let _ = self.0.stop_all();
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn fixture(path: &Path) -> Result<(), String> {
    let requested = std::env::args()
        .nth(3)
        .and_then(|s| s.parse::<u16>().ok())
        .unwrap_or(0);
    let tcp = TcpListener::bind(("127.0.0.1", requested)).map_err(|e| e.to_string())?;
    let udp = UdpSocket::bind("127.0.0.1:0").map_err(|e| e.to_string())?;
    let v6 = TcpListener::bind("[::1]:0").ok();
    fs::write(path, json!({"pid":std::process::id(),"tcp":tcp.local_addr().unwrap().port(),"udp":udp.local_addr().unwrap().port(),"v6":v6.as_ref().and_then(|p| p.local_addr().ok()).map(|a|a.port())}).to_string()).map_err(|e|e.to_string())?;
    loop {
        thread::sleep(Duration::from_secs(1));
    }
}
fn eventually(label: &str, mut check: impl FnMut() -> bool) -> Result<(), String> {
    let end = Instant::now() + Duration::from_secs(15);
    while Instant::now() < end {
        if check() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    Err(format!("验证超时：{label}"))
}
fn spawn(path: &Path) -> Result<Fixture, String> {
    spawn_on(path, 0)
}
fn spawn_on(path: &Path, port: u16) -> Result<Fixture, String> {
    let child = Command::new(std::env::current_exe().unwrap())
        .arg("--fixture")
        .arg(path)
        .arg(port.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())?;
    let child = Fixture(child);
    eventually("临时服务启动", || path.is_file())?;
    Ok(child)
}
fn metadata(path: &Path) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}
fn alive(meta: &Value) -> bool {
    TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", meta["tcp"]).parse().unwrap(),
        Duration::from_millis(200),
    )
    .is_ok()
}
fn quote(path: &Path) -> String {
    if cfg!(windows) {
        format!("& '{}'", path.display().to_string().replace('\'', "''"))
    } else {
        format!("'{}'", path.display().to_string().replace('\'', "'\\''"))
    }
}
fn argument(path: &Path) -> String {
    format!(
        "'{}'",
        path.display()
            .to_string()
            .replace('\'', if cfg!(windows) { "''" } else { "'\\''" })
    )
}
fn attach(
    manager: &ExternalManager,
    instance: &ExternalInstance,
    name: &str,
) -> Result<ServiceConfig, String> {
    manager.attach(AttachExternalRequest {
        identity: instance.identity.clone(),
        name: name.into(),
        notes: "临时验收记录".into(),
        working_dir: String::new(),
        existing_service_id: None,
    })
}

/// 该独立 Python 子进程只打开一个回环 TCP 监听端口；收到临时标志后关掉 socket，
/// 随后继续存活，以验证服务状态来自 PID 身份，而不是仅由端口是否还在决定。
const PYTHON_FIXTURE: &str = r#"
import json, os, socket, sys, time
metadata_path, close_path, closed_path = sys.argv[1:4]
listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
listener.bind(("127.0.0.1", 0))
listener.listen()
with open(metadata_path, "w") as output:
    json.dump({"pid": os.getpid(), "tcp": listener.getsockname()[1]}, output)
while not os.path.exists(close_path):
    time.sleep(0.02)
listener.close()
with open(closed_path, "w") as output:
    output.write("closed")
while True:
    time.sleep(1)
"#;

/// 找到并短暂验证可运行的解释器；找不到时只跳过 Python 专项，不影响 Rust fixture 验收。
fn available_python() -> Option<&'static str> {
    ["python3", "python"].into_iter().find(|program| {
        Command::new(program)
            .arg("-c")
            .arg("import sys; sys.exit(0)")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    })
}

/// 只启动自身创建的 Python fixture；Fixture 的 Drop 会在任意错误返回时 kill 并 wait，
/// 防止验收失败留下监听进程。命令行内容不会打印或持久化。
fn spawn_python_fixture(
    program: &str,
    metadata_path: &Path,
    close_path: &Path,
    closed_path: &Path,
) -> Result<Fixture, String> {
    let child = Command::new(program)
        .arg("-c")
        .arg(PYTHON_FIXTURE)
        .arg(metadata_path)
        .arg(close_path)
        .arg(closed_path)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| format!("启动 Python 验收进程失败：{error}"))?;
    let child = Fixture(child);
    eventually("Python 临时监听服务启动", || {
        metadata_path.is_file()
    })?;
    Ok(child)
}

/// 验证 unknown 默认受保护、无端口时仍按身份报告运行，以及解保护只存在于当前管理器内存。
fn python_protection_smoke(
    temporary: &Path,
    store_path: &Path,
    store: Arc<Store>,
) -> Result<(), String> {
    let Some(program) = available_python() else {
        println!("跳过 Python 分类与停止验收：未找到可运行的 python3 或 python；Rust fixture 验收不受影响");
        return Ok(());
    };

    let metadata_path = temporary.join("python-listener.json");
    let close_path = temporary.join("python-close-socket.flag");
    let closed_path = temporary.join("python-socket-closed.flag");
    let mut python = spawn_python_fixture(program, &metadata_path, &close_path, &closed_path)?;
    let python_meta = metadata(&metadata_path);
    let pid = python.0.id();
    assert_eq!(python_meta["pid"].as_u64(), Some(u64::from(pid)));
    assert!(alive(&python_meta), "Python 临时监听 socket 尚不可连接");

    let runtime = RuntimeManager::new(None);
    let manager = ExternalManager::new(store.clone(), runtime, None);
    let snapshot = manager.snapshot(true);
    if snapshot.stale {
        return Err(snapshot
            .error
            .unwrap_or("Python fixture 端口扫描失败".into()));
    }
    let discovered = snapshot
        .processes
        .iter()
        .find(|process| process.instance.identity.pid == pid)
        .ok_or("未发现本验收创建的 Python 监听进程")?;
    assert_eq!(
        discovered.instance.category, "unknown",
        "-c 内嵌代码来源不可验证，应归入默认受保护的 unknown"
    );
    let port = python_meta["tcp"]
        .as_u64()
        .ok_or("Python fixture 未返回 TCP 端口")? as u16;
    assert!(discovered
        .ports
        .iter()
        .any(|socket| socket.port == port && socket.protocol == "TCP"));

    let service = attach(&manager, &discovered.instance, "Python 临时监听验收")?;
    assert!(
        manager.stop(&service.id).is_err(),
        "未解除保护不应停止 unknown 实例"
    );
    assert!(python
        .0
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_none());
    assert!(
        alive(&python_meta),
        "拒绝停止后 Python fixture 必须仍在监听"
    );

    // 关掉唯一监听 socket，但让同一个 Python PID 继续运行。
    fs::write(&close_path, b"close").map_err(|error| error.to_string())?;
    eventually("Python fixture 关闭监听 socket", || {
        closed_path.is_file()
    })?;
    assert!(python
        .0
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_none());
    assert!(!alive(&python_meta), "关闭 socket 后端口仍能建立连接");
    let no_port_snapshot = manager.snapshot(true);
    if no_port_snapshot.stale {
        return Err(no_port_snapshot
            .error
            .unwrap_or("关闭 socket 后端口扫描失败".into()));
    }
    manager.refresh();
    let runtime = manager
        .runtimes()
        .into_iter()
        .find(|runtime| runtime.service_id == service.id)
        .ok_or("关闭 socket 后没有 Python 外部实例状态")?;
    assert_eq!(
        runtime.status, "running",
        "PID 仍存活时不能仅因端口关闭而标记 stopped"
    );
    assert!(
        runtime.ports.is_empty(),
        "关闭唯一 socket 后运行状态应没有监听端口"
    );

    manager.unlock(&service.id)?;
    let unlocked = manager
        .runtimes()
        .into_iter()
        .find(|runtime| runtime.service_id == service.id)
        .ok_or("无法读取解除保护后的 Python 状态")?;
    assert!(unlocked.protection_unlocked && unlocked.can_stop);

    // 新管理器和新 Store 连接只恢复持久身份，不恢复旧管理器内存中的解保护许可。
    let reopened = ExternalManager::new(
        Arc::new(Store::open(store_path)?),
        RuntimeManager::new(None),
        None,
    );
    reopened.refresh();
    let reopened_runtime = reopened
        .runtimes()
        .into_iter()
        .find(|runtime| runtime.service_id == service.id)
        .ok_or("重开管理器未恢复 Python 外部实例")?;
    assert_eq!(reopened_runtime.status, "running");
    assert!(!reopened_runtime.protection_unlocked);
    assert!(!reopened_runtime.can_stop && reopened_runtime.can_unlock);
    assert!(
        reopened.stop(&service.id).is_err(),
        "重开管理器不应继承旧解保护许可"
    );
    assert!(python
        .0
        .try_wait()
        .map_err(|error| error.to_string())?
        .is_none());

    // 在后台执行停止的同时由 Child::try_wait 回收退出子进程，避免 Unix 上的 zombie
    // 让身份轮询误以为进程仍活着。管理器仍只按已核验的 Python PID 执行停止。
    let stop_manager = manager.clone();
    let service_id = service.id.clone();
    let stop_worker = thread::spawn(move || stop_manager.stop(&service_id));
    let exited = eventually("解除保护后停止 Python fixture", || {
        python
            .0
            .try_wait()
            .map(|status| status.is_some())
            .unwrap_or(false)
    });
    if exited.is_err() {
        let _ = python.0.kill();
        let _ = python.0.wait();
    }
    let stop_result = stop_worker
        .join()
        .map_err(|_| "Python 外部停止线程异常退出".to_owned())?;
    exited?;
    stop_result?;
    assert!(!alive(&python_meta));

    store.delete_service(&service.id)?;
    manager.forget(&service.id);
    reopened.forget(&service.id);
    println!("通过：Python -c 分类 unknown、保护/解保护、重开不继承许可，以及无端口仍运行");
    Ok(())
}

fn smoke() -> Result<(), String> {
    let temporary = tempfile::tempdir().map_err(|e| e.to_string())?;
    let path1 = temporary.path().join("一号.json");
    let path2 = temporary.path().join("二号.json");
    let mut first = spawn(&path1)?;
    let mut second = spawn(&path2)?;
    let first_meta = metadata(&path1);
    let second_meta = metadata(&path2);
    let store = Arc::new(Store::open(&temporary.path().join("services.sqlite3"))?);
    let runtime = RuntimeManager::new(None);
    let _cleanup = ManagedCleanup(runtime.clone());
    let manager = ExternalManager::new(store.clone(), runtime.clone(), None);
    let snapshot = manager.snapshot(true);
    if snapshot.stale {
        return Err(snapshot.error.unwrap_or("端口扫描失败".into()));
    }
    let process = snapshot
        .processes
        .iter()
        .find(|p| p.instance.identity.pid == first.0.id())
        .ok_or("未发现一号服务")?;
    let other = snapshot
        .processes
        .iter()
        .find(|p| p.instance.identity.pid == second.0.id())
        .ok_or("未发现二号服务")?;
    for (key, protocol) in [("tcp", "TCP"), ("udp", "UDP"), ("v6", "TCP")] {
        if let Some(port) = first_meta[key].as_u64() {
            assert!(
                process
                    .ports
                    .iter()
                    .any(|p| p.port == port as u16 && p.protocol == protocol),
                "{key} 端口未归属到临时进程"
            );
        }
    }
    let mut service = attach(&manager, &process.instance, "一号外部服务")?;
    let other_service = attach(&manager, &other.instance, "二号外部服务")?;
    assert!(service.working_dir.is_empty() && service.command.is_empty());
    assert!(attach(&manager, &process.instance, "重复接入").is_err());
    assert!(manager
        .snapshot(false)
        .processes
        .iter()
        .any(|p| p.instance.identity == process.instance.identity
            && p.managed_service_id.as_deref() == Some(&service.id)));
    assert!(runtime.create_terminal(service.clone(), None).is_err());
    println!("通过：TCP/UDP/IPv6、多端口分组、接入、重复拒绝、缺配置保存及终端校验");

    let mut stale = service.clone();
    stale.id = String::new();
    stale.name = "旧身份快照".into();
    stale
        .external
        .as_mut()
        .unwrap()
        .identity
        .created_at
        .push_str("-expired");
    let stale = store.save_service(stale)?;
    manager.stop(&stale.id)?;
    assert!(alive(&first_meta));
    store.delete_service(&stale.id)?;
    manager.forget(&stale.id);
    println!("通过：同 PID 的不匹配创建身份不会终止当前进程");

    runtime.stop_all()?;
    assert!(alive(&first_meta) && alive(&second_meta));
    let reopened = ExternalManager::new(
        Arc::new(Store::open(&temporary.path().join("services.sqlite3"))?),
        RuntimeManager::new(None),
        None,
    );
    reopened.refresh();
    assert!(reopened
        .runtimes()
        .iter()
        .any(|r| r.service_id == service.id && r.status == "running"));
    runtime.remove_service(&other_service.id)?;
    store.delete_service(&other_service.id)?;
    manager.forget(&other_service.id);
    assert!(alive(&second_meta));
    println!("通过：退出受管运行时不停止外部服务，重开重新核验关联，删除仅解绑");

    service.command = "echo 不应执行".into();
    service.working_dir = temporary.path().join("不存在").display().to_string();
    service = store.save_service(service)?;
    assert!(manager.convert(&service.id).is_err());
    assert!(alive(&first_meta));
    manager.refresh();
    if manager
        .runtimes()
        .iter()
        .any(|r| r.service_id == service.id && r.can_unlock)
    {
        manager.unlock(&service.id)?;
    }
    manager.stop(&service.id)?;
    eventually("只停止选中的实例", || {
        first.0.try_wait().unwrap().is_some()
    })?;
    assert!(alive(&second_meta));
    manager.refresh();
    assert!(manager
        .runtimes()
        .iter()
        .any(|r| r.service_id == service.id && r.status == "stopped"));
    println!("通过：无效转换配置不会先停服务，停止单一外部进程不影响另一服务");

    // 同一个端口重新出现也不继承旧关联，必须用户主动选择新进程实例。
    let replacement_path = temporary.path().join("再次出现.json");
    let mut replacement = spawn_on(
        &replacement_path,
        first_meta["tcp"].as_u64().unwrap() as u16,
    )?;
    let replacement_snapshot = manager.snapshot(true);
    let replacement_process = replacement_snapshot
        .processes
        .iter()
        .find(|p| p.instance.identity.pid == replacement.0.id())
        .ok_or("未发现同端口新实例")?;
    assert!(replacement_process.managed_service_id.is_none());
    manager.refresh();
    assert!(manager
        .runtimes()
        .iter()
        .any(|r| r.service_id == service.id && r.status == "stopped"));
    service = manager.attach(AttachExternalRequest {
        identity: replacement_process.instance.identity.clone(),
        name: service.name.clone(),
        notes: service.notes.clone(),
        working_dir: String::new(),
        existing_service_id: Some(service.id.clone()),
    })?;
    if manager
        .runtimes()
        .iter()
        .any(|r| r.service_id == service.id && r.can_unlock)
    {
        manager.unlock(&service.id)?;
    }
    manager.stop(&service.id)?;
    eventually("停止重新关联的新实例", || {
        replacement.0.try_wait().unwrap().is_some()
    })?;
    println!("通过：同端口新实例不自动继承，明确重新关联后才可控制");

    let converted_path = temporary.path().join("转换后.json");
    service.working_dir = temporary.path().display().to_string();
    service.command = format!(
        "{} --fixture {}",
        quote(&std::env::current_exe().unwrap()),
        argument(&converted_path)
    );
    service.expected_ports = vec![first_meta["tcp"].as_u64().unwrap() as u16];
    service = store.save_service(service)?;
    let conflict =
        TcpListener::bind(("127.0.0.1", service.expected_ports[0])).map_err(|e| e.to_string())?;
    assert!(manager.convert(&service.id).is_err());
    assert_eq!(store.service(&service.id)?.source, "external");
    drop(conflict);
    let session = manager.convert(&service.id)?;
    assert_eq!(store.service(&service.id)?.source, "managed");
    assert_eq!(session.kind, "service");
    eventually("转换后新进程启动", || converted_path.is_file())?;
    assert!(alive(&metadata(&converted_path)));
    runtime.stop_all()?;
    assert!(alive(&second_meta));
    println!("通过：端口冲突取消转换，补齐配置后创建独立受管实例，托管退出保留其他外部服务");
    python_protection_smoke(
        temporary.path(),
        &temporary.path().join("services.sqlite3"),
        store.clone(),
    )?;
    let _ = second.0.kill();
    let _ = second.0.wait();
    Ok(())
}
fn main() {
    if runtime::run_worker_if_requested() {
        return;
    }
    if std::env::args().nth(1).as_deref() == Some("--fixture") {
        let path = PathBuf::from(std::env::args_os().nth(2).unwrap());
        if let Err(e) = fixture(&path) {
            eprintln!("{e}");
            std::process::exit(1);
        }
        return;
    }
    if let Err(error) = smoke() {
        eprintln!("外部服务验收失败：{error}");
        std::process::exit(1);
    }
}
