//! MCP stdio 只是一座本机桥梁，不另建内容数据库，也不拥有服务控制权限。
//! 每次主中心启动生成新的随机凭据；外部配置只包含可执行文件和专用参数，不包含令牌。
use crate::content_control::{ActionPlan, ContentController};
use axum::{
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    io::{BufRead, BufReader, Read, Write},
    net::{Ipv4Addr, SocketAddrV4, TcpStream},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tauri::{Emitter, Manager};

#[derive(Clone, Serialize, Deserialize)]
struct Descriptor {
    port: u16,
    token: String,
    pid: u32,
}
#[derive(Clone)]
struct Bridge {
    controller: Arc<ContentController>,
    descriptor: Descriptor,
    app: Option<tauri::AppHandle>,
}
fn descriptor_path(data_dir: &Path) -> PathBuf {
    data_dir.join("content-control.json")
}

/// 原子替换凭据文件。Unix 在创建临时文件时即限定 0600，避免先写令牌再修权限的窗口。
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    let parent = path.parent().ok_or("运行目录无效")?;
    std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    let mut temp = tempfile::NamedTempFile::new_in(parent).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(std::fs::Permissions::from_mode(0o600))
            .map_err(|e| e.to_string())?;
    }
    #[cfg(windows)]
    restrict_windows_file(temp.path())?;
    temp.write_all(bytes).map_err(|e| e.to_string())?;
    temp.as_file().sync_all().map_err(|e| e.to_string())?;
    temp.persist(path)
        .map_err(|e| format!("保存内容控制凭据失败：{e}"))?;
    Ok(())
}
#[cfg(windows)]
fn restrict_windows_file(path: &Path) -> Result<(), String> {
    // 使用当前进程用户的 SID，而非可包含特殊字符的显示名称；icacls 接收独立 argv。
    let identity = std::process::Command::new("whoami.exe")
        .args(["/user", "/fo", "csv", "/nh"])
        .output()
        .map_err(|e| format!("无法读取当前用户身份：{e}"))?;
    let output = String::from_utf8_lossy(&identity.stdout);
    let sid = output
        .split(',')
        .nth(1)
        .map(|v| v.trim().trim_matches('"'))
        .filter(|v| {
            v.starts_with("S-1-")
                && v.chars()
                    .all(|c| c.is_ascii_digit() || c == 'S' || c == '-')
        })
        .ok_or("无法核验当前 Windows 用户 SID")?;
    let result = std::process::Command::new("icacls.exe")
        .arg(path)
        .args(["/inheritance:r", "/grant:r"])
        .arg(format!("*{sid}:(F)"))
        .output()
        .map_err(|e| format!("无法限制凭据文件访问权限：{e}"))?;
    if !result.status.success() {
        return Err("Windows 拒绝设置仅当前用户可读的凭据文件；内容 MCP 未启动".into());
    }
    Ok(())
}
pub fn start(
    controller: Arc<ContentController>,
    data_dir: &Path,
    app: Option<tauri::AppHandle>,
) -> Result<(), String> {
    let listener = std::net::TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 0))
        .map_err(|e| format!("本机内容控制通道启动失败：{e}"))?;
    listener.set_nonblocking(true).map_err(|e| e.to_string())?;
    let descriptor = Descriptor {
        port: listener.local_addr().map_err(|e| e.to_string())?.port(),
        token: format!(
            "{}{}",
            uuid::Uuid::new_v4().simple(),
            uuid::Uuid::new_v4().simple()
        ),
        pid: std::process::id(),
    };
    write_private(
        &descriptor_path(data_dir),
        &serde_json::to_vec(&descriptor).map_err(|e| e.to_string())?,
    )?;
    let bridge = Bridge {
        controller,
        descriptor,
        app,
    };
    std::thread::Builder::new()
        .name("content-mcp-bridge".into())
        .spawn(move || {
            let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            runtime.block_on(async move {
                let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
                    return;
                };
                let router = Router::new()
                    .route("/rpc", post(handle))
                    .layer(DefaultBodyLimit::max(128 * 1024))
                    .with_state(bridge);
                let _ = axum::serve(listener, router).await;
            });
        })
        .map_err(|e| format!("内容控制线程启动失败：{e}"))?;
    Ok(())
}
async fn handle(
    State(bridge): State<Bridge>,
    headers: HeaderMap,
    Json(request): Json<Value>,
) -> (StatusCode, Json<Value>) {
    // 普通网页不允许借 localhost 发起内容操作；不开放 CORS，也不接受 query/cookie 中的凭据。
    let expected = format!("Bearer {}", bridge.descriptor.token);
    if headers.contains_key("origin")
        || headers.get("authorization").and_then(|v| v.to_str().ok()) != Some(expected.as_str())
    {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({"error":"内容控制认证失败"})),
        );
    }
    let result = tauri::async_runtime::spawn_blocking(move || dispatch(&bridge, &request)).await;
    (
        StatusCode::OK,
        Json(match result {
            Ok(Ok(value)) => json!({"result":value}),
            Ok(Err(error)) => json!({"error":error}),
            Err(error) => json!({"error":format!("内容控制任务失败：{error}")}),
        }),
    )
}
fn dispatch(bridge: &Bridge, request: &Value) -> Result<Value, String> {
    let args = &request["arguments"];
    match request["name"].as_str().ok_or("工具名称缺失")? {
        "content_list" => {
            reject_extra(args, &["query"])?;
            if args
                .get("query")
                .is_some_and(|v| !v.is_string() || v.as_str().is_some_and(|q| q.len() > 1000))
            {
                return Err("搜索词必须是有限长度的文字".into());
            }
            let items = bridge.controller.content.items()?;
            let query = args["query"].as_str().unwrap_or("").to_lowercase();
            let values = serde_json::to_value(items).map_err(|e| e.to_string())?;
            Ok(Value::Array(
                values
                    .as_array()
                    .ok_or("内容格式错误")?
                    .iter()
                    .filter(|v| {
                        query.is_empty()
                            || v["name"]
                                .as_str()
                                .unwrap_or("")
                                .to_lowercase()
                                .contains(&query)
                            || ["tags", "aliases"].iter().any(|k| {
                                v[*k].as_array().is_some_and(|a| {
                                    a.iter().any(|s| {
                                        s.as_str().unwrap_or("").to_lowercase().contains(&query)
                                    })
                                })
                            })
                    })
                    .cloned()
                    .collect(),
            ))
        }
        "content_status" => {
            reject_extra(args, &["contentId"])?;
            bridge
                .controller
                .content
                .execute(required(args, "contentId")?, "status")
        }
        "content_submit" => {
            reject_extra(args, &["requestId", "plan"])?;
            let plan: ActionPlan = serde_json::from_value(args["plan"].clone())
                .map_err(|e| format!("操作计划无效：{e}"))?;
            let task =
                bridge
                    .controller
                    .submit(required(args, "requestId")?.into(), "mcp", plan)?;
            if let Some(app) = &bridge.app {
                let _ = app.emit("content-task", &task);
                if matches!(
                    task.state.as_str(),
                    "awaitingConfirmation" | "awaitingSelection" | "awaitingForm"
                ) {
                    if let Some(window) = app.get_webview_window("main") {
                        let _ = window.show();
                        let _ = window.unminimize();
                    }
                }
            }
            serde_json::to_value(task).map_err(|e| e.to_string())
        }
        "content_task" => {
            reject_extra(args, &["taskId"])?;
            serde_json::to_value(bridge.controller.task(required(args, "taskId")?)?)
                .map_err(|e| e.to_string())
        }
        "content_cancel" => {
            reject_extra(args, &["taskId"])?;
            serde_json::to_value(bridge.controller.cancel(required(args, "taskId")?)?)
                .map_err(|e| e.to_string())
        }
        _ => Err("此 MCP 仅开放内容库工具，不提供服务控制、确认或 shell 接口".into()),
    }
}
fn required<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 128)
        .ok_or_else(|| format!("字段 {key} 无效"))
}
fn reject_extra(value: &Value, allowed: &[&str]) -> Result<(), String> {
    if value
        .as_object()
        .ok_or("参数必须是对象")?
        .keys()
        .any(|key| !allowed.contains(&key.as_str()))
    {
        Err("不接受额外参数；确认只能在中心窗口完成".into())
    } else {
        Ok(())
    }
}
fn tool(name: &str, description: &str, properties: Value, required: Value) -> Value {
    json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false}})
}
fn tools_list() -> Value {
    json!({"tools":[
        tool("content_list","查找内容库收藏，不读取文件正文或外部进程环境。",json!({"query":{"type":"string"}}),json!([])),
        tool("content_status","查看一个内容条目的实时可用性和已核验应用身份。",json!({"contentId":{"type":"string"}}),json!(["contentId"])),
        tool("content_submit","提交内容操作计划。使用稳定 requestId 防止重试重复执行。退出、移除、强制结束、批量修改及目标歧义需要用户在中心处理，不能自行确认。",json!({"requestId":{"type":"string"},"plan":{"type":"object","properties":{"summary":{"type":"string"},"actions":{"type":"array","minItems":1,"maxItems":20,"items":{"type":"object","properties":{"action":{"enum":["search","status","open","start","focus","requestQuit","forceQuit","remove","edit","add"]},"contentId":{"type":["string","null"]},"query":{"type":["string","null"]},"changes":{"type":["object","null"]}},"required":["action"],"additionalProperties":false}}},"required":["summary","actions"],"additionalProperties":false}}),json!(["requestId","plan"])),
        tool("content_task","读取任务进度；执行结果来自中心而非模型推断。",json!({"taskId":{"type":"string"}}),json!(["taskId"])),
        tool("content_cancel","取消任务，已经完成的打开操作不会撤销，未执行步骤不再继续。",json!({"taskId":{"type":"string"}}),json!(["taskId"]))
    ]})
}
fn default_data_dir() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("KACENT_DATA_DIR") {
        return Ok(path.into());
    }
    #[cfg(target_os = "macos")]
    {
        return std::env::var_os("HOME")
            .map(|p| PathBuf::from(p).join("Library/Application Support/com.kacent.center"))
            .ok_or_else(|| "无法确定中心数据目录".into());
    }
    #[cfg(windows)]
    {
        return std::env::var_os("APPDATA")
            .map(|p| PathBuf::from(p).join("com.kacent.center"))
            .ok_or_else(|| "无法确定中心数据目录".into());
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
            .map(|p| p.join("com.kacent.center"))
            .ok_or_else(|| "无法确定中心数据目录".into())
    }
}
fn call_main(request: Value) -> Result<Value, String> {
    let path = descriptor_path(&default_data_dir()?);
    let bytes =
        std::fs::read(&path).map_err(|_| "中心未运行或尚未启用内容控制，请先打开服务中心。")?;
    let descriptor: Descriptor =
        serde_json::from_slice(&bytes).map_err(|_| "内容控制运行文件无效，请重新打开中心。")?;
    let mut stream = TcpStream::connect_timeout(
        &SocketAddrV4::new(Ipv4Addr::LOCALHOST, descriptor.port).into(),
        Duration::from_secs(3),
    )
    .map_err(|_| "无法连接运行中的中心，请先打开服务中心。")?;
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    let body = serde_json::to_vec(&request).map_err(|e| e.to_string())?;
    write!(stream,"POST /rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",descriptor.token,body.len()).map_err(|e|e.to_string())?;
    stream.write_all(&body).map_err(|e| e.to_string())?;
    let mut response = Vec::new();
    stream
        .take(2 * 1024 * 1024)
        .read_to_end(&mut response)
        .map_err(|e| format!("中心响应超时或断开：{e}"))?;
    let header_end = response
        .windows(4)
        .position(|v| v == b"\r\n\r\n")
        .ok_or("本机通道响应无效")?;
    if !response.starts_with(b"HTTP/1.1 200") {
        return Err("本机通道拒绝请求；中心可能已重启，请重试。".into());
    }
    let value: Value = serde_json::from_slice(&response[header_end + 4..])
        .map_err(|_| "本机通道响应不是有效 JSON")?;
    if let Some(error) = value["error"].as_str() {
        Err(error.into())
    } else {
        Ok(value["result"].clone())
    }
}
/// 专用模式必须在 Tauri 初始化之前分流。stdout 仅写 JSON-RPC，任何错误都进入结构化结果。
pub fn run_stdio_if_requested() -> bool {
    if std::env::args().nth(1).as_deref() != Some("--content-mcp") {
        return false;
    }
    let mut reader = BufReader::new(std::io::stdin().lock());
    let mut output = std::io::stdout().lock();
    loop {
        let mut bytes = Vec::new();
        let count = reader
            .by_ref()
            .take(128 * 1024 + 1)
            .read_until(b'\n', &mut bytes);
        match count {
            Ok(0) | Err(_) => break,
            _ => {}
        }
        if bytes.len() > 128 * 1024 {
            break;
        }
        let request: Value = match serde_json::from_slice(&bytes) {
            Ok(value) => value,
            Err(_) => {
                let _ = writeln!(
                    output,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"JSON 格式无效"}})
                );
                let _ = output.flush();
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = match request["method"].as_str() {
            Some("initialize") => Ok(
                json!({"protocolVersion":match request["params"]["protocolVersion"].as_str(){Some(version @ ("2024-11-05"|"2025-03-26"|"2025-06-18"|"2025-11-25"))=>version,_=>"2025-11-25"},"capabilities":{"tools":{}},"serverInfo":{"name":"kacent-content","version":"0.1.0"},"instructions":"只管理中心内容库。需要确认时请让用户回到中心，不能用其他方式绕过确认。"}),
            ),
            Some("ping") => Ok(json!({})),
            Some("tools/list") => Ok(tools_list()),
            Some("tools/call") => {
                let result = call_main(request["params"].clone());
                Ok(match result {
                    Ok(value) => {
                        json!({"content":[{"type":"text","text":serde_json::to_string(&value).unwrap_or_default()}],"isError":false})
                    }
                    Err(error) => json!({"content":[{"type":"text","text":error}],"isError":true}),
                })
            }
            _ => Err(json!({"code":-32601,"message":"不支持的 MCP 方法"})),
        };
        let envelope = match response {
            Ok(value) => json!({"jsonrpc":"2.0","id":id,"result":value}),
            Err(error) => json!({"jsonrpc":"2.0","id":id,"error":error}),
        };
        if writeln!(output, "{envelope}")
            .and_then(|_| output.flush())
            .is_err()
        {
            break;
        }
    }
    true
}
pub fn connection_check() -> Result<Value, String> {
    call_main(json!({"name":"content_list","arguments":{"query":""}}))
        .map(|_| json!({"connected":true,"message":"内容库认证通道连接成功"}))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bridge_does_not_offer_confirmation_or_service_tools() {
        let names = tools_list()["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["name"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
        assert_eq!(names.len(), 5);
        assert!(!names
            .iter()
            .any(|v| v.contains("confirm") || v.contains("service")));
        assert!(reject_extra(
            &json!({"plan":{},"requestId":"x","confirmed":true}),
            &["plan", "requestId"]
        )
        .is_err());
    }
}

#[cfg(test)]
mod transport_tests {
    use super::*;
    use crate::{
        applications::ApplicationManager,
        content::ContentManager,
        content_models::{ContentItem, ContentKind, ContentTarget},
        discovery::DiscoveryManager,
        store::Store,
    };
    fn request(port: u16, token: &str, body: Value, origin: bool) -> (String, Value) {
        let mut stream = TcpStream::connect((Ipv4Addr::LOCALHOST, port)).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        let bytes = serde_json::to_vec(&body).unwrap();
        write!(stream,"POST /rpc HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {token}\r\n{}Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",if origin{"Origin: https://example.com\r\n"}else{""},bytes.len()).unwrap();
        stream.write_all(&bytes).unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        let (header, body) = response.split_once("\r\n\r\n").unwrap();
        (header.into(), serde_json::from_str(body).unwrap())
    }
    #[test]
    fn actual_loopback_transport_rejects_unauthorized_origin_and_forged_confirmation() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("test.sqlite3")).unwrap());
        let applications = ApplicationManager::new(store.clone(), DiscoveryManager::new(), None);
        let content = ContentManager::new(store, applications);
        let item = content
            .save(ContentItem::new(
                "验证目录",
                ContentKind::Folder,
                ContentTarget::Folder {
                    path: dir.path().to_string_lossy().into_owned(),
                },
            ))
            .unwrap();
        let controller = ContentController::new(content);
        start(controller.clone(), dir.path(), None).unwrap();
        let descriptor: Descriptor =
            serde_json::from_slice(&std::fs::read(descriptor_path(dir.path())).unwrap()).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(descriptor_path(dir.path()))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let list = json!({"name":"content_list","arguments":{}});
        assert!(request(descriptor.port, "wrong", list.clone(), false)
            .0
            .starts_with("HTTP/1.1 401"));
        assert!(
            request(descriptor.port, &descriptor.token, list.clone(), true)
                .0
                .starts_with("HTTP/1.1 401")
        );
        assert_eq!(
            request(descriptor.port, &descriptor.token, list, false).1["result"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        let remove = json!({"name":"content_submit","arguments":{"requestId":"external-remove","plan":{"summary":"移除收藏","actions":[{"action":"remove","contentId":item.id} ]}}});
        let response = request(descriptor.port, &descriptor.token, remove, false).1;
        assert_eq!(response["result"]["state"], "awaitingConfirmation");
        for name in ["content_confirm", "start_service", "shell"] {
            assert!(request(
                descriptor.port,
                &descriptor.token,
                json!({"name":name,"arguments":{}}),
                false
            )
            .1["error"]
                .is_string());
        }
        assert_eq!(controller.content.items().unwrap().len(), 1);
    }
}
