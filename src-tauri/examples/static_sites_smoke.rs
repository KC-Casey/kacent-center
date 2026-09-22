//! 静态站点真实 HTTP 验收示例。
//!
//! 先在 src-tauri 目录执行 `cargo build --bin kacent-center`，再运行：
//! `cargo run --example static_sites_smoke -- target/debug/kacent-center`。
//! 示例只创建并清理自己临时目录中的站点，不操作服务发现中的其他进程。

use kacent_center_lib::{
    discovery::DiscoveryManager,
    models::{LaunchConfig, ServiceConfig},
    static_sites::StaticSiteManager,
};
use std::{
    collections::BTreeMap,
    env,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    thread,
    time::{Duration, Instant},
};

fn config(root: &Path, id: &str, port: u16) -> ServiceConfig {
    ServiceConfig {
        id: id.into(),
        kind: "staticSite".into(),
        launch: LaunchConfig::StaticSite {
            root: root.to_string_lossy().into_owned(),
            port,
            spa_fallback: true,
        },
        source: "managed".into(),
        external: None,
        name: id.into(),
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
}

fn request(port: u16, path: &str, headers: &str) -> Result<String, String> {
    let mut socket = TcpStream::connect(("127.0.0.1", port)).map_err(|error| error.to_string())?;
    socket
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    socket
        .write_all(
            format!(
                "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n{headers}\r\n"
            )
            .as_bytes(),
        )
        .map_err(|error| error.to_string())?;
    let mut response = String::new();
    socket
        .read_to_string(&mut response)
        .map_err(|error| error.to_string())?;
    Ok(response)
}

fn wait_until_unreachable(port: u16) -> bool {
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_err() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(Duration::from_millis(40));
    }
}

fn response_status(response: &str, status: &str) -> bool {
    response
        .lines()
        .next()
        .is_some_and(|line| line.contains(status))
}

fn run(helper: PathBuf) -> Result<(), String> {
    let temporary = tempfile::tempdir().map_err(|error| error.to_string())?;
    let first_root = temporary.path().join("静态站点 一");
    let second_root = temporary.path().join("静态站点 二");
    std::fs::create_dir_all(first_root.join("empty")).map_err(|error| error.to_string())?;
    std::fs::create_dir_all(&second_root).map_err(|error| error.to_string())?;
    std::fs::write(
        first_root.join("index.html"),
        "<!doctype html><title>站点一</title><main>第一站内容</main>",
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(first_root.join("asset.js"), "console.log('site-one');")
        .map_err(|error| error.to_string())?;
    std::fs::write(
        second_root.join("index.html"),
        "<!doctype html><title>站点二</title><main>第二站内容</main>",
    )
    .map_err(|error| error.to_string())?;
    std::fs::write(first_root.join("empty/hidden.txt"), "不应列出此文件")
        .map_err(|error| error.to_string())?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let outside = temporary.path().join("站点外.txt");
        std::fs::write(&outside, "目录外的机密内容").map_err(|error| error.to_string())?;
        symlink(&outside, first_root.join("越界链接.txt")).map_err(|error| error.to_string())?;
        // 入口文件的越界链接也必须在服务启动前被拒绝。
        let bad_root = temporary.path().join("错误入口");
        std::fs::create_dir_all(&bad_root).map_err(|error| error.to_string())?;
        symlink(&outside, bad_root.join("index.html")).map_err(|error| error.to_string())?;
        let invalid_manager = StaticSiteManager::new_with_helper_override(
            DiscoveryManager::new(),
            None,
            Some(helper.clone()),
        );
        assert!(invalid_manager
            .start(config(&bad_root, "越界入口", 0))
            .is_err());
    }

    let manager =
        StaticSiteManager::new_with_helper_override(DiscoveryManager::new(), None, Some(helper));
    let result = (|| {
        let first = manager.start(config(&first_root, "站点一", 0))?;
        let second = manager.start(config(&second_root, "站点二", 0))?;
        let first_port = first
            .runtime
            .ports
            .first()
            .ok_or("站点一没有回报实际监听端口")?
            .port;
        let second_port = second
            .runtime
            .ports
            .first()
            .ok_or("站点二没有回报实际监听端口")?
            .port;
        assert_ne!(first_port, second_port, "自动分配的端口必须互相独立");
        assert_eq!(first.runtime.web_urls.len(), 1);
        assert_eq!(second.runtime.web_urls.len(), 1);
        assert_eq!(manager.managed_processes().len(), 2);

        let first_page = request(first_port, "/", "Accept: text/html\r\n")?;
        assert!(response_status(&first_page, "200 OK"));
        assert!(first_page.contains("第一站内容"));
        let second_page = request(second_port, "/", "Accept: text/html\r\n")?;
        assert!(second_page.contains("第二站内容"));

        let route = request(first_port, "/client/route", "Accept: text/html\r\n")?;
        assert!(response_status(&route, "200 OK"));
        assert!(route.contains("第一站内容"), "SPA 页面路由应回到入口文件");
        let missing_asset = request(first_port, "/missing.js", "Accept: text/html\r\n")?;
        assert!(response_status(&missing_asset, "404 Not Found"));
        let missing_file = request(first_port, "/missing.txt", "Accept: */*\r\n")?;
        assert!(response_status(&missing_file, "404 Not Found"));
        let listing = request(first_port, "/empty/", "Accept: text/html\r\n")?;
        assert!(
            !listing.contains("不应列出此文件"),
            "不能通过目录入口读取内容"
        );
        assert!(!listing.contains("hidden.txt"), "不能生成目录文件列表");

        #[cfg(unix)]
        {
            let escaped = request(first_port, "/越界链接.txt", "Accept: */*\r\n")?;
            assert!(response_status(&escaped, "403 Forbidden"));
            assert!(!escaped.contains("目录外的机密内容"));
        }

        manager.stop("站点一")?;
        assert!(
            wait_until_unreachable(first_port),
            "停止后站点一的监听端口必须关闭"
        );
        assert!(request(second_port, "/", "Accept: text/html\r\n")?.contains("第二站内容"));
        assert_eq!(manager.managed_processes().len(), 1);

        let restarted = manager.restart(config(&second_root, "站点二", 0))?;
        assert_ne!(restarted.runtime.run_id, second.runtime.run_id);
        let restarted_port = restarted.runtime.ports[0].port;
        assert!(request(restarted_port, "/", "Accept: text/html\r\n")?.contains("第二站内容"));

        // 固定端口冲突必须明确失败，helper 退出后也不能泄漏运行态进程。
        let occupied = TcpListener::bind(("127.0.0.1", 0)).map_err(|error| error.to_string())?;
        let occupied_port = occupied
            .local_addr()
            .map_err(|error| error.to_string())?
            .port();
        let conflict = manager.start(config(&first_root, "端口冲突", occupied_port));
        assert!(conflict.is_err());
        assert!(manager
            .runtimes()
            .iter()
            .any(|runtime| runtime.service_id == "端口冲突" && runtime.status == "failed"));

        manager.remove_service("站点一")?;
        assert!(!manager
            .runtimes()
            .iter()
            .any(|runtime| runtime.service_id == "站点一"));
        Ok(())
    })();
    let cleanup = manager.stop_all();
    result.and(cleanup)
}

fn main() -> Result<(), String> {
    let helper = env::args_os()
        .nth(1)
        .or_else(|| env::var_os("KACENT_STATIC_HELPER"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            "请传入 kacent-center helper 路径或设置 KACENT_STATIC_HELPER。".to_owned()
        })?;
    run(helper)?;
    println!("通过：独立站点、实际端口、SPA 路由、缺失资源 404、目录列表防护、越界 symlink 防护、端口冲突和关闭清理。");
    Ok(())
}
