//! 仅编译并操作本验收程序在临时目录中创建的 Cocoa 应用，不控制用户已有应用。
//! 主线程维持 CFRunLoop，使 NSWorkspace 的异步启动回调可正常投递。
#[cfg(target_os = "macos")]
mod smoke {
    use kacent_center_lib::{
        applications::ApplicationManager, discovery::DiscoveryManager, models::*, store::Store,
    };
    use std::{
        collections::BTreeMap,
        path::Path,
        process::Command,
        sync::{mpsc, Arc},
        thread,
        time::{Duration, Instant},
    };

    const FIXTURE: &str = r#"
#import <Cocoa/Cocoa.h>
@interface ProbeDelegate : NSObject <NSApplicationDelegate>
@end
@implementation ProbeDelegate
- (void)applicationDidFinishLaunching:(NSNotification *)notification {
    NSArray *args = [[NSProcessInfo processInfo] arguments];
    if ([args count] > 1) {
        NSDictionary *env = [[NSProcessInfo processInfo] environment];
        NSDictionary *data = @{ @"pid": @([[NSProcessInfo processInfo] processIdentifier]), @"argument": [args lastObject], @"label": env[@"KACENT_APP_LABEL"] ?: @"" };
        [[NSJSONSerialization dataWithJSONObject:data options:0 error:nil] writeToFile:args[1] atomically:YES];
    }
}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    return [[[NSProcessInfo processInfo] environment][@"KACENT_REFUSE_QUIT"] isEqualToString:@"1"] ? NSTerminateCancel : NSTerminateNow;
}
@end
int main(void) { @autoreleasepool {
    NSApplication *app = [NSApplication sharedApplication];
    ProbeDelegate *delegate = [ProbeDelegate new];
    [app setDelegate:delegate];
    [app setActivationPolicy:NSApplicationActivationPolicyAccessory];
    [app run];
} return 0; }
"#;

    fn build_fixture(root: &Path) -> Result<std::path::PathBuf, String> {
        let app = root.join("中文应用 验收.app");
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).map_err(|e| e.to_string())?;
        let source = root.join("fixture.m");
        std::fs::write(&source, FIXTURE).map_err(|e| e.to_string())?;
        std::fs::write(contents.join("Info.plist"), format!(r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>kacent-app-probe</string><key>CFBundleIdentifier</key><string>local.kacent.fixture.{}</string><key>CFBundleName</key><string>应用验收</string><key>CFBundlePackageType</key><string>APPL</string><key>LSUIElement</key><true/></dict></plist>"#, uuid::Uuid::new_v4().simple())).map_err(|e| e.to_string())?;
        let output = Command::new("/usr/bin/clang")
            .args(["-fobjc-arc", "-framework", "Cocoa"])
            .arg(&source)
            .arg("-o")
            .arg(contents.join("MacOS/kacent-app-probe"))
            .output()
            .map_err(|e| e.to_string())?;
        if !output.status.success() {
            return Err(format!(
                "编译验收应用失败：{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        std::fs::canonicalize(app).map_err(|e| e.to_string())
    }
    fn service(path: &Path, receipt: &Path) -> ServiceConfig {
        ServiceConfig {
            id: String::new(),
            kind: "desktopApp".into(),
            launch: LaunchConfig::DesktopApp {
                path: path.to_string_lossy().into_owned(),
                args: vec![receipt.to_string_lossy().into_owned(), "中文 参数".into()],
                start_dir: String::new(),
            },
            source: "managed".into(),
            external: None,
            name: "无端口应用验收".into(),
            description: String::new(),
            working_dir: String::new(),
            environment_type: "system".into(),
            environment_dir: String::new(),
            executable_dirs: vec![],
            env: BTreeMap::from([("KACENT_APP_LABEL".into(), "独立应用环境".into())]),
            command: String::new(),
            shell: String::new(),
            expected_ports: vec![],
            notes: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }
    fn wait_receipt(path: &Path) -> Result<serde_json::Value, String> {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Ok(content) = std::fs::read(path) {
                if let Ok(value) = serde_json::from_slice(&content) {
                    return Ok(value);
                }
            }
            if Instant::now() >= deadline {
                return Err("应用未写出启动参数回执。".into());
            }
            thread::sleep(Duration::from_millis(80));
        }
    }
    fn unlock_if_needed(manager: &ApplicationManager, id: &str) -> Result<(), String> {
        manager.refresh();
        if manager
            .runtimes()
            .iter()
            .any(|r| r.service_id == id && r.can_unlock)
        {
            manager.unlock(id)?;
        }
        Ok(())
    }
    struct FixtureCleanup {
        manager: Arc<ApplicationManager>,
        path: std::path::PathBuf,
    }
    impl Drop for FixtureCleanup {
        fn drop(&mut self) {
            // 断言失败也只回收这个临时 app 的实例；完整路径与原生身份由接入接口再次核验。
            for application in self
                .manager
                .refresh()
                .applications
                .into_iter()
                .filter(|a| Path::new(&a.path) == self.path)
            {
                let id = application.managed_service_id.or_else(|| {
                    self.manager
                        .attach(AttachApplicationRequest {
                            identity: application.instance.identity,
                            path: application.path,
                            name: "验收清理".into(),
                            notes: String::new(),
                            working_dir: String::new(),
                            existing_service_id: None,
                        })
                        .ok()
                        .map(|s| s.id)
                });
                if let Some(id) = id {
                    let _ = unlock_if_needed(&self.manager, &id);
                    let _ = self.manager.force_quit(&id);
                }
            }
        }
    }
    struct OwnedFixture(std::process::Child);
    impl Drop for OwnedFixture {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    pub fn run() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|e| e.to_string())?;
        let path = build_fixture(root.path())?;
        let receipt = root.path().join("启动 回执.json");
        let store = Arc::new(Store::open(&root.path().join("应用.sqlite3"))?);
        let discovery = DiscoveryManager::new();
        let manager = ApplicationManager::new(store.clone(), discovery.clone(), None);
        let _cleanup = FixtureCleanup {
            manager: manager.clone(),
            path: path.clone(),
        };
        let mut tracked = None::<String>;
        let result = (|| {
            let config = store.save_service(service(&path, &receipt))?;
            tracked = Some(config.id.clone());
            let launched = manager.start(config.clone())?;
            assert!(launched.session.is_none(), "桌面应用不得创建服务 PTY");
            assert_eq!(launched.runtime.status, "running");
            let initial_pid = launched.runtime.pid.ok_or("应用没有关联 PID")?;
            let receipt_data = wait_receipt(&receipt)?;
            assert_eq!(receipt_data["argument"], "中文 参数");
            assert_eq!(receipt_data["label"], "独立应用环境");
            assert_eq!(
                manager.start(config.clone())?.runtime.pid,
                Some(initial_pid)
            );
            let snapshot = manager.refresh();
            let application = snapshot
                .applications
                .iter()
                .find(|a| a.instance.identity.pid == initial_pid)
                .ok_or("无端口应用没有出现在发现结果")?;
            assert_eq!(
                application.managed_service_id.as_deref(),
                Some(config.id.as_str())
            );
            assert!(!launched.runtime.can_focus, "无窗口应用不应允许显示");
            // 第二个实例由验收程序直接拥有；测试相同应用路径的多实例选择，不能按名称合并。
            let second_receipt = root.path().join("第二实例.json");
            let second = OwnedFixture(
                Command::new(path.join("Contents/MacOS/kacent-app-probe"))
                    .arg(&second_receipt)
                    .spawn()
                    .map_err(|e| e.to_string())?,
            );
            wait_receipt(&second_receipt)?;
            let deadline = Instant::now() + Duration::from_secs(10);
            let second_application = loop {
                let snapshot = manager.refresh();
                if let Some(app) = snapshot
                    .applications
                    .into_iter()
                    .find(|a| a.instance.identity.pid == second.0.id())
                {
                    break app;
                }
                if Instant::now() >= deadline {
                    return Err("第二个无端口应用实例未被发现".into());
                }
                thread::sleep(Duration::from_millis(100));
            };
            assert!(
                manager
                    .attach(AttachApplicationRequest {
                        identity: second_application.instance.identity,
                        path: second_application.path,
                        name: config.name.clone(),
                        notes: config.notes.clone(),
                        working_dir: String::new(),
                        existing_service_id: Some(config.id.clone())
                    })
                    .is_err(),
                "不能用第二个实例覆盖活动中的旧关联"
            );
            let candidate_config = store.save_service(service(&path, &receipt))?;
            let candidates = manager.start(candidate_config.clone())?;
            assert_eq!(candidates.candidates.len(), 2);
            assert_eq!(candidates.runtime.status, "unknown");
            store.delete_service(&candidate_config.id)?;
            manager.forget(&candidate_config.id);
            drop(second);
            let mut old_identity = application.instance.identity.clone();
            old_identity.created_at.push_str("过期");
            assert!(manager
                .attach(AttachApplicationRequest {
                    identity: old_identity,
                    path: path.to_string_lossy().into_owned(),
                    name: "旧身份".into(),
                    notes: String::new(),
                    working_dir: String::new(),
                    existing_service_id: Some(config.id.clone())
                })
                .is_err());

            // 模拟重新打开管理中心：数据库绑定恢复同一实例，不触发启动或退出。
            let restored = ApplicationManager::new(store.clone(), discovery.clone(), None);
            restored.refresh();
            assert_eq!(
                restored
                    .runtimes()
                    .iter()
                    .find(|r| r.service_id == config.id)
                    .and_then(|r| r.pid),
                Some(initial_pid)
            );
            drop(restored);
            store.delete_service(&config.id)?;
            manager.forget(&config.id);
            assert!(
                manager
                    .refresh()
                    .applications
                    .iter()
                    .any(|a| a.instance.identity.pid == initial_pid),
                "删除记录不应停止应用"
            );
            let fresh = store.save_service(service(&path, &receipt))?;
            tracked = Some(fresh.id.clone());
            assert_eq!(manager.start(fresh.clone())?.runtime.pid, Some(initial_pid));
            unlock_if_needed(&manager, &fresh.id)?;
            assert_eq!(manager.request_quit(&fresh.id)?.status, "stopped");

            let mut refusing = fresh.clone();
            refusing.env.insert("KACENT_REFUSE_QUIT".into(), "1".into());
            let refusing = store.save_service(refusing)?;
            let refused_pid = manager
                .start(refusing.clone())?
                .runtime
                .pid
                .ok_or("拒绝退出样例未启动")?;
            unlock_if_needed(&manager, &refusing.id)?;
            assert_eq!(manager.request_quit(&refusing.id)?.status, "running");
            assert!(
                manager.restart(refusing.clone()).is_err(),
                "拒绝退出时重启不能生成第二个实例"
            );
            assert_eq!(
                manager
                    .refresh()
                    .applications
                    .iter()
                    .filter(|a| a.path == path.to_string_lossy())
                    .count(),
                1
            );
            assert_eq!(
                manager
                    .runtimes()
                    .iter()
                    .find(|r| r.service_id == refusing.id)
                    .and_then(|r| r.pid),
                Some(refused_pid)
            );
            assert_eq!(manager.force_quit(&refusing.id)?.status, "stopped");
            println!("通过：自建无端口应用的启动、发现、唯一复用、中文参数环境、旧身份拒绝、恢复跟踪、删除保留、正常退出、拒绝退出取消重启及显式强制结束。");
            Ok(())
        })();
        if let Some(id) = tracked {
            let _ = unlock_if_needed(&manager, &id);
            if manager
                .runtimes()
                .iter()
                .any(|r| r.service_id == id && matches!(r.status.as_str(), "running" | "unknown"))
            {
                let _ = manager.force_quit(&id);
            }
        }
        result
    }
    pub fn main() -> Result<(), String> {
        use std::ffi::c_void;
        #[link(name = "CoreFoundation", kind = "framework")]
        unsafe extern "C" {
            static kCFRunLoopDefaultMode: *const c_void;
            fn CFRunLoopRunInMode(
                mode: *const c_void,
                seconds: f64,
                return_after_source: bool,
            ) -> i32;
        }
        let (tx, rx) = mpsc::channel();
        thread::spawn(move || {
            let _ = tx.send(
                std::panic::catch_unwind(run)
                    .map_err(|_| "应用验收断言失败".to_owned())
                    .and_then(|r| r),
            );
        });
        loop {
            match rx.try_recv() {
                Ok(result) => return result,
                Err(mpsc::TryRecvError::Disconnected) => return Err("应用验收线程提前退出".into()),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            unsafe {
                CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, true);
            }
        }
    }
}
#[cfg(target_os = "macos")]
fn main() -> Result<(), String> {
    smoke::main()
}
#[cfg(not(target_os = "macos"))]
fn main() {
    eprintln!("此验收夹具仅支持 macOS；Windows 请使用受控的原生 GUI 测试应用。");
}
