//! 在临时中文目录中运行自己的两个 Node HTTP 服务，不接入或停止用户已有项目。
use kacent_center_lib::{models::*, projects, runtime::RuntimeManager};
use std::{
    collections::BTreeMap,
    io::{Read, Write},
    net::TcpStream,
    path::Path,
    thread,
    time::{Duration, Instant},
};

fn config(root: &Path, id: &str) -> ServiceConfig {
    ServiceConfig {
        id: id.into(),
        kind: "frontend".into(),
        launch: LaunchConfig::Frontend {
            package_manager: "npm".into(),
            script: "dev".into(),
            args: vec![format!("参数 {id}")],
            url: String::new(),
        },
        source: "managed".into(),
        external: None,
        name: id.into(),
        description: String::new(),
        working_dir: root.to_string_lossy().into(),
        environment_type: "system".into(),
        environment_dir: String::new(),
        executable_dirs: vec![],
        env: BTreeMap::from([("项目标识".into(), id.into())]),
        command: String::new(),
        shell: String::new(),
        expected_ports: vec![],
        notes: String::new(),
        created_at: String::new(),
        updated_at: String::new(),
    }
}
fn wait_json(root: &Path) -> Result<serde_json::Value, String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        if let Ok(bytes) = std::fs::read(root.join("运行.json")) {
            if let Ok(json) = serde_json::from_slice(&bytes) {
                return Ok(json);
            }
        }
        if Instant::now() > deadline {
            return Err("等待前端启动超时".into());
        }
        thread::sleep(Duration::from_millis(100));
    }
}
fn responds(port: u16) -> bool {
    let Ok(mut socket) = TcpStream::connect(("127.0.0.1", port)) else {
        return false;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_secs(2)));
    if socket
        .write_all(b"GET / HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .is_err()
    {
        return false;
    }
    let mut result = String::new();
    socket.read_to_string(&mut result).is_ok() && result.contains("200 OK")
}
fn main() -> Result<(), String> {
    let temporary = tempfile::tempdir().map_err(|e| e.to_string())?;
    let runtime = RuntimeManager::new(None);
    let result = (|| {
        let mut ports = Vec::new();
        for name in ["前端 一", "前端 二"] {
            let root = temporary.path().join(name);
            std::fs::create_dir(&root).map_err(|e| e.to_string())?;
            let manager = if name == "前端 一" { "npm" } else { "pnpm" };
            let manifest = serde_json::json!({ "name": "frontend-smoke", "scripts": { "dev": "node server.cjs" }, "dependencies": { "kacent-deliberately-unavailable-fixture": "0.0.0" } });
            std::fs::write(root.join("package.json"), manifest.to_string())
                .map_err(|e| e.to_string())?;
            // 用稳定的内置模块名，避免验收结果取决于 PTY 恰好解析到的 Node 版本。
            std::fs::write(root.join("server.cjs"), r#"const http = require('http'); const fs = require('fs'); const server = http.createServer((req,res) => res.end(process.env['项目标识'])); server.listen(0,'127.0.0.1',()=>{ fs.writeFileSync('运行.json',JSON.stringify({port:server.address().port,arg:process.argv[2],label:process.env['项目标识'],cwd:process.cwd()})); console.log('中文前端已启动'); });"#).map_err(|e| e.to_string())?;
            assert_eq!(
                projects::inspect(root.to_str().unwrap())?
                    .suggested_script
                    .as_deref(),
                Some("dev")
            );
            let mut service = config(&root, name);
            if let LaunchConfig::Frontend {
                package_manager, ..
            } = &mut service.launch
            {
                *package_manager = manager.into();
            }
            let first = runtime.start_service(service.clone())?;
            let duplicate = runtime.start_service(service)?;
            assert_eq!(first.id, duplicate.id);
            let data = wait_json(&root)?;
            assert_eq!(data["arg"], format!("参数 {name}"));
            assert_eq!(data["label"], name);
            let port = data["port"].as_u64().ok_or("没有实际端口")? as u16;
            assert!(responds(port));
            ports.push(port);
            assert!(
                !root.join("node_modules").exists(),
                "启动脚本不能自动安装依赖"
            );
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let state = runtime.runtimes();
            if state.iter().all(|r| !r.web_urls.is_empty()) && state.len() == 2 {
                break;
            }
            if Instant::now() > deadline {
                return Err("实际端口未生成浏览器入口".into());
            }
            thread::sleep(Duration::from_millis(200));
        }
        runtime.stop_service("前端 一")?;
        assert!(!responds(ports[0]));
        assert!(responds(ports[1]));
        let root = temporary.path().join("前端 一");
        let mut invalid = config(&root, "工具缺失");
        invalid
            .env
            .insert("PATH".into(), "/不存在的工具目录".into());
        assert!(runtime.start_service(invalid).is_err());
        println!("通过：项目识别、两个前端独立启动、中文参数环境、重复启动幂等、实际端口入口、单独停止和工具缺失。");
        Ok(())
    })();
    let cleanup = runtime.stop_all();
    result.and(cleanup)
}
