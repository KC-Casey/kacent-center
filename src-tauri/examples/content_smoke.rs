//! 内容库真实链路验收。
//!
//! 示例只使用临时 SQLite、临时中文目录和本验收程序自己编译的 Cocoa `.app`。
//! 它不读取默认用户数据库，不按名称或 PID 搜索并控制已有用户应用；清理时也只
//! 对路径位于本次临时目录内的 fixture 发送退出请求。非 macOS 平台只报告跳过，
//! 因为当前原生 `ApplicationManager` 仅在 macOS/Windows 提供 GUI 应用适配层。

#[cfg(target_os = "macos")]
mod smoke {
    use kacent_center_lib::{
        applications::ApplicationManager,
        content::ContentManager,
        content_control::{ActionPlan, ContentController, PlannedAction},
        content_models::{ContentItem, ContentKind, ContentStatus, ContentTarget},
        discovery::DiscoveryManager,
        models::{
            ApplicationBinding, ExternalInstance, LaunchConfig, ProcessIdentity, ServiceConfig,
        },
        store::Store,
    };
    use rusqlite::{params, Connection};
    use std::{
        collections::BTreeMap,
        path::{Path, PathBuf},
        process::Command,
        sync::Arc,
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
        NSDictionary *data = @{
            @"pid": @([[NSProcessInfo processInfo] processIdentifier]),
            @"arguments": args,
            @"lastArgument": [args lastObject]
        };
        NSData *encoded = [NSJSONSerialization dataWithJSONObject:data options:0 error:nil];
        [encoded writeToFile:args[1] atomically:YES];
    }
}
- (NSApplicationTerminateReply)applicationShouldTerminate:(NSApplication *)sender {
    return NSTerminateNow;
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

    fn eventually(
        label: &str,
        seconds: u64,
        mut check: impl FnMut() -> bool,
    ) -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        while Instant::now() < deadline {
            if check() {
                return Ok(());
            }
            thread::sleep(Duration::from_millis(80));
        }
        Err(format!("验证超时：{label}"))
    }

    fn build_fixture(root: &Path) -> Result<PathBuf, String> {
        let app = root.join("内容验收 中文应用.app");
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).map_err(|error| error.to_string())?;
        let source = root.join("content_fixture.m");
        std::fs::write(&source, FIXTURE).map_err(|error| error.to_string())?;
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?><plist version="1.0"><dict><key>CFBundleExecutable</key><string>kacent-content-probe</string><key>CFBundleIdentifier</key><string>local.kacent.content.{}</string><key>CFBundleName</key><string>内容库验收</string><key>CFBundlePackageType</key><string>APPL</string><key>LSUIElement</key><true/></dict></plist>"#,
                uuid::Uuid::new_v4().simple()
            ),
        )
        .map_err(|error| error.to_string())?;
        let output = Command::new("/usr/bin/clang")
            .args(["-fobjc-arc", "-framework", "Cocoa"])
            .arg(&source)
            .arg("-o")
            .arg(contents.join("MacOS/kacent-content-probe"))
            .output()
            .map_err(|error| format!("编译内容应用 fixture 失败：{error}"))?;
        if !output.status.success() {
            return Err(format!(
                "编译内容应用 fixture 失败：{}",
                String::from_utf8_lossy(&output.stderr)
            ));
        }
        std::fs::canonicalize(app).map_err(|error| error.to_string())
    }

    fn folder_item(name: &str, path: &Path) -> ContentItem {
        ContentItem::new(
            name,
            ContentKind::Folder,
            ContentTarget::Folder {
                path: path.to_string_lossy().into_owned(),
            },
        )
    }

    fn app_item(name: &str, path: &Path, receipt: &Path) -> ContentItem {
        ContentItem::new(
            name,
            ContentKind::Application,
            ContentTarget::LocalApp {
                path: path.to_string_lossy().into_owned(),
                args: vec![receipt.to_string_lossy().into_owned(), "中文 参数".into()],
                start_dir: String::new(),
            },
        )
    }

    fn command_service(root: &Path) -> ServiceConfig {
        ServiceConfig {
            id: String::new(),
            kind: "command".into(),
            launch: LaunchConfig::Command,
            source: "managed".into(),
            external: None,
            name: "错误类别服务".into(),
            description: String::new(),
            working_dir: root.to_string_lossy().into_owned(),
            environment_type: "system".into(),
            environment_dir: String::new(),
            executable_dirs: Vec::new(),
            env: BTreeMap::new(),
            command: "true".into(),
            shell: "/bin/sh".into(),
            expected_ports: Vec::new(),
            notes: String::new(),
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    fn action(action: &str, content_id: Option<String>, query: Option<String>) -> ActionPlan {
        ActionPlan {
            summary: "内容库 smoke 验收".into(),
            actions: vec![PlannedAction {
                action: action.into(),
                content_id,
                query,
                changes: None,
            }],
        }
    }

    /// 清理只针对本次 fixture 路径。内容记录可能已删除，因此先解除仍存在的内容
    /// 关联，再临时接入一个服务记录以复用 ApplicationManager 的安全退出校验。
    struct FixtureCleanup {
        content: Arc<ContentManager>,
        applications: Arc<ApplicationManager>,
        fixture_path: PathBuf,
    }

    impl Drop for FixtureCleanup {
        fn drop(&mut self) {
            for item in self.content.items().unwrap_or_default() {
                let owned = match &item.target {
                    ContentTarget::LocalApp { path, .. } => {
                        path == self.fixture_path.to_string_lossy().as_ref()
                    }
                    _ => false,
                };
                if owned {
                    let _ = self.content.remove(&item.id);
                }
            }
            for application in
                self.applications
                    .refresh()
                    .applications
                    .into_iter()
                    .filter(|application| {
                        application.path == self.fixture_path.to_string_lossy().as_ref()
                    })
            {
                let service_id = application.managed_service_id.or_else(|| {
                    self.applications
                        .attach(kacent_center_lib::models::AttachApplicationRequest {
                            identity: application.instance.identity,
                            path: application.path,
                            name: "内容 smoke 清理".into(),
                            notes: String::new(),
                            working_dir: String::new(),
                            existing_service_id: None,
                        })
                        .ok()
                        .map(|service| service.id)
                });
                if let Some(service_id) = service_id {
                    let _ = self.applications.unlock(&service_id);
                    let _ = self.applications.force_quit(&service_id);
                }
            }
        }
    }

    fn run() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|error| error.to_string())?;
        let database = root.path().join("内容迁移.sqlite3");
        let fixture_path = build_fixture(root.path())?;
        let legacy_service_id = "legacy-desktop-app";
        // 以旧版 user_version=4 的临时数据库启动，Store::open 应只补内容表并升到当前
        // schema；这里不创建真实用户表，也不读取默认数据目录。
        {
            let connection = Connection::open(&database).map_err(|error| error.to_string())?;
            // Store v4 已经存在完整 services 表；这里仅建立 smoke 需要的最小旧版形状，
            // 让后续的非 desktopApp serviceLink 校验也走真实 Store 保存路径。
            connection
                .execute_batch(
                    "CREATE TABLE services (
                        id TEXT PRIMARY KEY NOT NULL,
                        name TEXT NOT NULL,
                        description TEXT NOT NULL,
                        working_dir TEXT NOT NULL,
                        environment_type TEXT NOT NULL,
                        environment_dir TEXT NOT NULL,
                        executable_dirs_json TEXT NOT NULL,
                        env_json TEXT NOT NULL,
                        command TEXT NOT NULL,
                        shell TEXT NOT NULL,
                        expected_ports_json TEXT NOT NULL,
                        notes TEXT NOT NULL,
                        created_at TEXT NOT NULL,
                        updated_at TEXT NOT NULL,
                        source TEXT NOT NULL DEFAULT 'managed',
                        external_json TEXT,
                        kind TEXT NOT NULL DEFAULT 'command',
                        launch_json TEXT NOT NULL DEFAULT '{\"type\":\"command\"}'
                    );",
                )
                .map_err(|error| error.to_string())?;
            connection
                .execute_batch(
                    "CREATE TABLE application_bindings (
                        service_id TEXT PRIMARY KEY NOT NULL
                            REFERENCES services(id) ON DELETE CASCADE,
                        binding_json TEXT NOT NULL
                    );
                    CREATE TABLE documents (
                        id TEXT PRIMARY KEY NOT NULL,
                        service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                        title TEXT NOT NULL,
                        content TEXT NOT NULL,
                        updated_at TEXT NOT NULL
                    );
                    CREATE INDEX documents_service_id_idx ON documents(service_id);",
                )
                .map_err(|error| error.to_string())?;
            let launch_json = serde_json::to_string(&LaunchConfig::DesktopApp {
                path: fixture_path.to_string_lossy().into_owned(),
                args: vec!["旧参数".into()],
                start_dir: String::new(),
            })
            .map_err(|error| error.to_string())?;
            connection
                .execute(
                    "INSERT INTO services (
                        id, name, description, working_dir, environment_type, environment_dir,
                        executable_dirs_json, env_json, command, shell, expected_ports_json,
                        notes, created_at, updated_at, source, external_json, kind, launch_json
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                    params![
                        legacy_service_id,
                        "旧桌面应用",
                        "旧服务说明",
                        root.path().to_string_lossy().as_ref(),
                        "system",
                        "",
                        "[]",
                        "{}",
                        "",
                        "",
                        "[]",
                        "旧服务备注",
                        "2025-01-01T00:00:00Z",
                        "2025-01-02T00:00:00Z",
                        "managed",
                        None::<String>,
                        "desktopApp",
                        launch_json,
                    ],
                )
                .map_err(|error| error.to_string())?;
            connection
                .execute(
                    "INSERT INTO documents (id, service_id, title, content, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        "legacy-desktop-document",
                        legacy_service_id,
                        "旧应用说明",
                        "迁移后仍保留的桌面应用文档。",
                        "2025-01-03T00:00:00Z",
                    ],
                )
                .map_err(|error| error.to_string())?;
            let legacy_binding = ApplicationBinding {
                instance: ExternalInstance {
                    identity: ProcessIdentity {
                        pid: 424_242,
                        created_at: "legacy-desktop-generation".into(),
                    },
                    name: "旧桌面应用实例".into(),
                    executable: Some(fixture_path.to_string_lossy().into_owned()),
                    working_dir: Some(root.path().to_string_lossy().into_owned()),
                    category: "nonSystem".into(),
                    reasons: vec!["迁移验收虚拟身份".into()],
                    shared_host: false,
                    controllable: true,
                    control_reason: None,
                },
                target_path: fixture_path.to_string_lossy().into_owned(),
                bundle_id: Some("local.kacent.legacy".into()),
                origin: "restored".into(),
                bound_at: "2025-01-02T00:00:00Z".into(),
            };
            connection
                .execute(
                    "INSERT INTO application_bindings (service_id, binding_json)
                     VALUES (?1, ?2)",
                    params![
                        legacy_service_id,
                        serde_json::to_string(&legacy_binding).map_err(|error| error.to_string())?
                    ],
                )
                .map_err(|error| error.to_string())?;
            connection
                .pragma_update(None, "user_version", 4_i64)
                .map_err(|error| error.to_string())?;
        }
        let store = Arc::new(Store::open(&database)?);
        let discovery = DiscoveryManager::new();
        let applications = ApplicationManager::new(store.clone(), discovery, None);
        let content = ContentManager::new(store.clone(), applications.clone());
        let controller = ContentController::new(content.clone());
        let _cleanup = FixtureCleanup {
            content: content.clone(),
            applications: applications.clone(),
            fixture_path: fixture_path.clone(),
        };

        let legacy_item = content
            .items()?
            .into_iter()
            .find(|item| {
                matches!(
                    &item.target,
                    ContentTarget::ServiceLink { service_id } if service_id == legacy_service_id
                )
            })
            .ok_or("v4 desktopApp 未自动生成 serviceLink 收藏")?;
        assert_eq!(legacy_item.name, "旧桌面应用");
        assert_eq!(legacy_item.description, "旧服务说明");
        assert_eq!(legacy_item.notes, "旧服务备注");
        assert_eq!(store.service(legacy_service_id)?.name, "旧桌面应用");
        assert_eq!(
            store.documents(legacy_service_id)?[0].content,
            "迁移后仍保留的桌面应用文档。"
        );
        assert_eq!(
            store
                .application_binding(legacy_service_id)?
                .ok_or("旧桌面应用身份关联丢失")?
                .instance
                .identity
                .created_at,
            "legacy-desktop-generation"
        );
        // 删除迁移出的收藏不能删除原服务、文档或身份关联；版本已经到 5，重开也不能
        // 重新把用户刚删掉的收藏回填。
        content.remove(&legacy_item.id)?;
        assert!(content
            .items()?
            .iter()
            .all(|item| item.id != legacy_item.id));
        assert!(store.service(legacy_service_id).is_ok());
        assert_eq!(store.documents(legacy_service_id)?.len(), 1);
        assert!(store.application_binding(legacy_service_id)?.is_some());
        let reopened = Store::open(&database)?;
        assert!(reopened
            .content_items()?
            .iter()
            .all(|item| item.id != legacy_item.id));
        assert!(reopened.service(legacy_service_id).is_ok());
        assert_eq!(reopened.documents(legacy_service_id)?.len(), 1);
        assert!(reopened.application_binding(legacy_service_id)?.is_some());
        drop(reopened);
        println!(
            "通过：v4 desktopApp 一次性回填 serviceLink，旧配置/文档/身份保留，删除后重开不复活"
        );

        let folder_a = root.path().join("同名资料 甲 中文目录");
        let folder_b = root.path().join("同名资料 乙 中文目录");
        std::fs::create_dir_all(&folder_a).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(&folder_b).map_err(|error| error.to_string())?;
        let folder_a_item = content.save(folder_item("同名资料", &folder_a))?;
        let folder_b_item = content.save(folder_item("同名资料", &folder_b))?;
        assert_eq!(content.items()?.len(), 2);
        assert!(content
            .statuses()?
            .iter()
            .filter(|status| status.content_id == folder_a_item.id
                || status.content_id == folder_b_item.id)
            .all(|status| status.state == "available"));
        println!("通过：旧版临时数据库迁移、中文空格目录和同名不同目标保存/状态");

        let ambiguous = controller.submit(
            "folder-selection".into(),
            "mcp",
            action("status", None, Some("同名资料".into())),
        )?;
        assert_eq!(ambiguous.state, "awaitingSelection");
        assert_eq!(ambiguous.steps[0].candidates.len(), 2);
        let selected = controller.choose(&ambiguous.id, 0, &folder_b_item.id)?;
        assert_eq!(selected.state, "completed");

        let steam = content.save(ContentItem::new(
            "Steam 验收游戏",
            ContentKind::Game,
            ContentTarget::Steam {
                app_id: "steam://rungameid/440".into(),
            },
        ))?;
        assert_eq!(
            steam.target.canonical_uri().as_deref(),
            Some("steam://run/440")
        );
        for invalid in [
            "steam://run/440?args=unsafe",
            "steam://run/440#fragment",
            "steam://run/440/extra",
            "steam://open/440",
            "https://example.invalid/440",
        ] {
            assert!(
                content
                    .save(ContentItem::new(
                        "非法 Steam",
                        ContentKind::Game,
                        ContentTarget::Steam {
                            app_id: invalid.into(),
                        },
                    ))
                    .is_err(),
                "非法 Steam 目标不应写入：{invalid}"
            );
        }
        println!("通过：Steam 固定 URI 规范化和非法 URI 拒绝，未启动用户游戏");

        let wrong_service = store.save_service(command_service(root.path()))?;
        assert!(content
            .save(ContentItem::new(
                "错误服务关联",
                ContentKind::Application,
                ContentTarget::ServiceLink {
                    service_id: wrong_service.id,
                },
            ))
            .is_err());
        println!("通过：serviceLink 拒绝非 desktopApp 服务");

        let removable_path = root.path().join("待移除 中文目录");
        std::fs::create_dir_all(&removable_path).map_err(|error| error.to_string())?;
        let removable = content.save(folder_item("待移除收藏", &removable_path))?;
        let remove_plan = action("remove", Some(removable.id.clone()), None);
        let pending_remove =
            controller.submit("remove-old-stamp".into(), "mcp", remove_plan.clone())?;
        assert_eq!(pending_remove.state, "awaitingConfirmation");
        let mut edited = removable.clone();
        edited.notes = "确认前发生编辑".into();
        content.save(edited)?;
        let refreshed = controller.confirm(&pending_remove.id, 0)?;
        assert_eq!(refreshed.state, "awaitingConfirmation");
        assert!(refreshed.steps[0].error.is_some());
        let completed = controller.confirm(&pending_remove.id, 0)?;
        assert_eq!(completed.state, "completed");
        assert!(removable_path.is_dir(), "删除收藏不能删除原文件夹");
        assert!(content.items()?.iter().all(|item| item.id != removable.id));
        let repeated = controller.submit("remove-old-stamp".into(), "mcp", remove_plan)?;
        assert_eq!(repeated.id, pending_remove.id);
        assert_eq!(repeated.state, "completed");

        let cancellable_path = root.path().join("取消后仍存在 中文目录");
        std::fs::create_dir_all(&cancellable_path).map_err(|error| error.to_string())?;
        let cancellable = content.save(folder_item("取消删除", &cancellable_path))?;
        let pending_cancel = controller.submit(
            "cancel-remove".into(),
            "mcp",
            action("remove", Some(cancellable.id.clone()), None),
        )?;
        assert_eq!(pending_cancel.state, "awaitingConfirmation");
        let cancelled = controller.cancel(&pending_cancel.id)?;
        assert_eq!(cancelled.state, "cancelled");
        assert!(controller.confirm(&pending_cancel.id, 0).is_err());
        assert!(cancellable_path.is_dir());
        assert!(content
            .items()?
            .iter()
            .any(|item| item.id == cancellable.id));
        println!("通过：旧确认戳二次确认、请求幂等和取消不执行");

        let receipt = root.path().join("应用启动回执 中文.json");
        let application = content.save(app_item("临时原生应用", &fixture_path, &receipt))?;
        let duplicate = content.save(app_item("同实例第二收藏", &fixture_path, &receipt))?;
        let started: ContentStatus =
            serde_json::from_value(content.execute(&application.id, "start")?)
                .map_err(|error| format!("启动状态格式无效：{error}"))?;
        let identity = started
            .identity
            .clone()
            .ok_or("内容启动后没有完整原生身份")?;
        assert_eq!(started.state, "running");
        eventually("Cocoa fixture 写入中文启动参数", 10, || {
            receipt.is_file()
        })?;
        let receipt_value: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&receipt).map_err(|error| error.to_string())?,
        )
        .map_err(|error| error.to_string())?;
        assert_eq!(receipt_value["lastArgument"], "中文 参数");
        let repeated: ContentStatus =
            serde_json::from_value(content.execute(&application.id, "start")?)
                .map_err(|error| format!("重复启动状态格式无效：{error}"))?;
        assert_eq!(repeated.identity, Some(identity.clone()));
        assert!(
            content.execute(&duplicate.id, "start").is_err(),
            "同一实例不能被第二收藏接管"
        );

        content.remove(&application.id)?;
        assert!(applications
            .refresh()
            .applications
            .iter()
            .any(|candidate| candidate.instance.identity == identity));
        content.associate(&duplicate.id, identity.clone())?;
        let quit: ContentStatus =
            serde_json::from_value(content.execute(&duplicate.id, "requestQuit")?)
                .map_err(|error| format!("退出状态格式无效：{error}"))?;
        assert!(matches!(quit.state.as_str(), "stopped" | "unavailable"));
        eventually("内容关联应用退出", 10, || {
            !applications
                .refresh()
                .applications
                .iter()
                .any(|candidate| candidate.instance.identity == identity)
        })?;
        assert!(content.items()?.iter().any(|item| item.id == duplicate.id));
        println!("通过：原生内容应用启动、同实例复用/去重、删除收藏保留进程、重新关联后请求退出");
        Ok(())
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
        let (sender, receiver) = std::sync::mpsc::channel();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(run)
                .map_err(|_| "内容库 smoke 断言失败".to_owned())
                .and_then(|result| result);
            let _ = sender.send(result);
        });
        loop {
            match receiver.try_recv() {
                Ok(result) => return result,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    return Err("内容库 smoke 验收线程提前退出".into())
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => {}
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
    println!("内容库原生应用 smoke 仅在 macOS 执行；临时 DB/Steam/控制器验收由 macOS CI 运行。");
}
