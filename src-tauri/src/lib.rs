pub mod applications;
pub mod content;
pub mod content_agent;
pub mod content_control;
pub mod content_mcp;
pub mod content_models;
pub mod discovery;
pub mod external;
pub mod models;
pub mod projects;
pub mod runtime;
pub mod speech;
pub mod static_sites;
pub mod store;

use models::*;
use runtime::RuntimeManager;
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, RwLock,
    },
};
use store::Store;
use tauri::{
    ipc::Channel,
    menu::{Menu, MenuItem},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
    Emitter, Manager, State, Url,
};

struct AppState {
    content: Arc<content_control::ContentController>,
    speech: Arc<speech::SpeechManager>,
    content_bridge_error: Option<String>,
    store: Arc<Store>,
    runtime: Arc<RuntimeManager>,
    external: Arc<external::ExternalManager>,
    applications: Arc<applications::ApplicationManager>,
    static_sites: Arc<static_sites::StaticSiteManager>,
    operations: Mutex<HashMap<String, Arc<Mutex<()>>>>,
    quitting: Arc<AtomicBool>,
    quit_requested: AtomicBool,
    lifecycle: Arc<RwLock<()>>,
}
impl AppState {
    /// 每个服务独立串行化破坏性操作，避免删除与启动交错后出现无记录进程。
    /// 不使用全局运行锁，等待某个服务停止时，其他服务仍可独立操作。
    fn gate(&self, id: &str) -> Result<Arc<Mutex<()>>, String> {
        Ok(self
            .operations
            .lock()
            .map_err(|_| "服务操作锁不可用")?
            .entry(id.into())
            .or_default()
            .clone())
    }
}

#[tauri::command]
async fn content_snapshot(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let content = state.content.content.clone();
    tauri::async_runtime::spawn_blocking(move || {
        Ok(serde_json::json!({"items":content.items()?,"statuses":content.statuses()?}))
    })
    .await
    .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn content_save(
    state: State<'_, AppState>,
    item: content_models::ContentItem,
    stamp: Option<String>,
) -> Result<content_models::ContentItem, String> {
    if state.quitting.load(Ordering::SeqCst) {
        return Err("中心正在退出".into());
    }
    let content = state.content.content.clone();
    tauri::async_runtime::spawn_blocking(move || content.save_checked(item, stamp.as_deref()))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn content_associate(
    state: State<'_, AppState>,
    content_id: String,
    identity: ProcessIdentity,
) -> Result<(), String> {
    if state.quitting.load(Ordering::SeqCst) {
        return Err("中心正在退出".into());
    }
    let content = state.content.content.clone();
    tauri::async_runtime::spawn_blocking(move || content.associate(&content_id, identity))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn content_tasks(state: State<'_, AppState>) -> Vec<content_control::ActionTask> {
    state.content.tasks()
}
#[tauri::command]
async fn content_submit(
    state: State<'_, AppState>,
    plan: content_control::ActionPlan,
    request_id: String,
) -> Result<content_control::ActionTask, String> {
    let controller = state.content.clone();
    tauri::async_runtime::spawn_blocking(move || controller.submit(request_id, "manual", plan))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn content_plan(
    app: tauri::AppHandle,
    state: State<'_, AppState>,
    input: String,
    agent_id: String,
    request_id: String,
) -> Result<content_control::ActionTask, String> {
    let agent = state.store.agent(&agent_id)?;
    let controller = state.content.clone();
    let (task, fresh) = controller.begin(request_id, "agent", input.clone())?;
    if fresh {
        let id = task.id.clone();
        let cancelled = task.cancelled.clone();
        // 模型任务独立运行，前端可立即展示进度并取消。只传名称、别名及分类，不传文件正文、备注或环境。
        tauri::async_runtime::spawn_blocking(move || {
            let outcome = (|| {
                let candidates=controller.content.items()?.into_iter().map(|item|serde_json::json!({"id":item.id,"name":item.name,"kind":item.kind,"aliases":item.aliases,"group":item.group,"tags":item.tags})).collect::<Vec<_>>();
                let plan =
                    content_agent::plan(&agent, &input, &serde_json::json!(candidates), cancelled)?;
                controller.accept_plan(&id, plan)
            })();
            if let Err(error) = outcome {
                controller.fail(&id, error);
            }
            if let Ok(task) = controller.task(&id) {
                let _ = app.emit("content-task", task);
            }
        });
    }
    Ok(task)
}
#[tauri::command]
async fn content_confirm(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    task_id: String,
    index: usize,
) -> Result<content_control::ActionTask, String> {
    if window.label() != "main" {
        return Err("确认只能来自中心主窗口".into());
    }
    let controller = state.content.clone();
    tauri::async_runtime::spawn_blocking(move || controller.confirm(&task_id, index))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn content_choose(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    task_id: String,
    index: usize,
    content_id: String,
) -> Result<content_control::ActionTask, String> {
    if window.label() != "main" {
        return Err("目标选择只能来自中心主窗口".into());
    }
    let controller = state.content.clone();
    tauri::async_runtime::spawn_blocking(move || controller.choose(&task_id, index, &content_id))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn content_cancel(
    state: State<'_, AppState>,
    task_id: String,
) -> Result<content_control::ActionTask, String> {
    state.content.cancel(&task_id)
}
#[tauri::command]
async fn content_save_form(
    window: tauri::WebviewWindow,
    state: State<'_, AppState>,
    task_id: String,
    index: usize,
    item: content_models::ContentItem,
) -> Result<content_models::ContentItem, String> {
    if window.label() != "main" {
        return Err("表单确认只能来自中心主窗口".into());
    }
    let controller = state.content.clone();
    tauri::async_runtime::spawn_blocking(move || controller.save_form(&task_id, index, item))
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn content_agent_capabilities(
    state: State<'_, AppState>,
) -> Result<serde_json::Value, String> {
    let profiles = state.store.agents()?;
    tauri::async_runtime::spawn_blocking(move || serde_json::Value::Array(content_agent::capabilities(&profiles).into_iter().map(|c|serde_json::json!({"agentId":c.id,"name":c.name,"terminalAvailable":c.available,"centerSupported":c.mode=="isolatedCli","centerAvailable":c.can_plan,"mcpSupported":c.mode=="isolatedCli"||c.mode=="externalMcp","reason":c.reason.or_else(||Some(c.limitations.join("；"))),"executable":c.resolved_path})).collect())).await.map_err(|e|e.to_string())
}
#[tauri::command]
fn content_preferences(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    let value = state.store.content_preferences()?;
    Ok(
        serde_json::json!({"agentId":value["agentId"].as_str().unwrap_or(""),"voiceEnabled":value["voiceEnabled"].as_bool().unwrap_or(false)}),
    )
}
#[tauri::command]
fn save_content_preferences(
    state: State<'_, AppState>,
    preferences: serde_json::Value,
) -> Result<serde_json::Value, String> {
    let agent_id = preferences["agentId"]
        .as_str()
        .ok_or("缺少默认 Agent 配置")?;
    if !agent_id.is_empty() {
        state.store.agent(agent_id)?;
    }
    let voice = preferences["voiceEnabled"]
        .as_bool()
        .ok_or("语音开关无效")?;
    state
        .store
        .save_content_preferences(serde_json::json!({"agentId":agent_id,"voiceEnabled":voice}))
}
#[tauri::command]
fn content_mcp_config(agent_id: String) -> Result<serde_json::Value, String> {
    let executable = std::env::current_exe().map_err(|e| e.to_string())?;
    let guide = content_agent::mcp_config_guide(&executable.to_string_lossy());
    Ok(guide.get(&agent_id).cloned().unwrap_or(guide))
}
#[tauri::command]
async fn content_mcp_check(state: State<'_, AppState>) -> Result<serde_json::Value, String> {
    if let Some(error) = &state.content_bridge_error {
        return Err(error.clone());
    }
    tauri::async_runtime::spawn_blocking(content_mcp::connection_check)
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
fn speech_status(state: State<'_, AppState>) -> serde_json::Value {
    let status = state.speech.status();
    serde_json::json!({"state":status.state,"modelReady":status.model_available,"progress":status.progress.unwrap_or(0),"durationSeconds":status.duration_ms.unwrap_or(0)/1000,"error":status.error})
}
#[tauri::command]
async fn speech_start(state: State<'_, AppState>) -> Result<(), String> {
    if !state.store.content_preferences()?["voiceEnabled"]
        .as_bool()
        .unwrap_or(false)
    {
        return Err("请先在设置中启用语音".into());
    }
    let speech = state.speech.clone();
    tauri::async_runtime::spawn_blocking(move || speech.start())
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn speech_cancel(state: State<'_, AppState>) -> Result<(), String> {
    let speech = state.speech.clone();
    tauri::async_runtime::spawn_blocking(move || speech.cancel())
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn speech_finish(state: State<'_, AppState>) -> Result<String, String> {
    let speech = state.speech.clone();
    tauri::async_runtime::spawn_blocking(move || speech.finish())
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn speech_download(state: State<'_, AppState>) -> Result<speech::SpeechModelInfo, String> {
    let speech = state.speech.clone();
    tauri::async_runtime::spawn_blocking(move || speech.download_model())
        .await
        .map_err(|e| e.to_string())?
}
#[tauri::command]
async fn speech_import(
    state: State<'_, AppState>,
    path: String,
) -> Result<speech::SpeechModelInfo, String> {
    let speech = state.speech.clone();
    tauri::async_runtime::spawn_blocking(move || speech.import_model(PathBuf::from(path)))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn bootstrap(state: State<'_, AppState>) -> Result<Bootstrap, String> {
    let external = state.external.clone();
    let applications = state.applications.clone();
    tauri::async_runtime::spawn_blocking(move || {
        external.refresh();
        applications.refresh();
    })
    .await
    .map_err(|e| format!("恢复外部实例失败：{e}"))?;
    let services = state.store.services()?;
    let mut runtimes = state
        .runtime
        .runtimes()
        .into_iter()
        .filter(|r| {
            services.iter().any(|s| {
                s.id == r.service_id
                    && s.source == "managed"
                    && matches!(s.kind.as_str(), "command" | "frontend")
            })
        })
        .collect::<Vec<_>>();
    runtimes.extend(state.external.runtimes());
    runtimes.extend(state.applications.runtimes());
    runtimes.extend(state.static_sites.runtimes().into_iter().filter(|r| {
        services
            .iter()
            .any(|s| s.id == r.service_id && s.kind == "staticSite")
    }));
    for service in &services {
        if !runtimes
            .iter()
            .any(|runtime| runtime.service_id == service.id)
        {
            runtimes.push(ServiceRuntime::stopped(&service.id));
        }
    }
    Ok(Bootstrap {
        services,
        agents: state.store.agents()?,
        preferences: state.store.preferences()?,
        runtimes,
        sessions: state.runtime.sessions(),
    })
}
#[tauri::command]
async fn save_service(
    state: State<'_, AppState>,
    mut service: ServiceConfig,
) -> Result<ServiceConfig, String> {
    if state.quitting.load(Ordering::SeqCst) {
        return Err("应用正在退出，请稍后操作".into());
    }
    let gate = state.gate(&service.id)?;
    let store = state.store.clone();
    let runtime = state.runtime.clone();
    let applications = state.applications.clone();
    let static_sites = state.static_sites.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _guard = gate.lock().map_err(|_| "服务操作锁不可用")?;
        let mut clear_binding = false;
        // 普通编辑只能修改启动配方和说明；来源及进程身份必须经接入/转换命令核验。
        if service.id.is_empty() {
            if service.source != "managed" || service.external.is_some() {
                return Err("请通过发现服务接入外部实例。".into());
            }
        } else {
            let previous = store.service(&service.id)?;
            let active = runtime
                .runtimes()
                .into_iter()
                .chain(applications.runtimes())
                .chain(static_sites.runtimes())
                .any(|r| {
                    r.service_id == service.id
                        && matches!(
                            r.status.as_str(),
                            "running" | "starting" | "stopping" | "unknown"
                        )
                });
            let target_changed = match (&previous.launch, &service.launch) {
                (
                    LaunchConfig::DesktopApp { path: old, .. },
                    LaunchConfig::DesktopApp { path: new, .. },
                ) => old != new,
                (
                    LaunchConfig::StaticSite { root: old, .. },
                    LaunchConfig::StaticSite { root: new, .. },
                ) => old != new,
                _ => previous.kind != service.kind,
            };
            if active && (previous.kind != service.kind || target_changed) {
                return Err("实例活动期间不能切换服务类型或运行目标，请先停止或解除关联。".into());
            }
            clear_binding =
                previous.kind == "desktopApp" && (previous.kind != service.kind || target_changed);
            if previous.source == "external" && previous.kind != service.kind {
                return Err("外部记录不能直接切换类型，请通过相应发现入口接入。".into());
            }
            if previous.source != service.source
                || previous.external.as_ref().map(|e| &e.identity)
                    != service.external.as_ref().map(|e| &e.identity)
            {
                return Err("不能通过编辑修改服务来源或进程身份，请使用接入或转换操作。".into());
            }
            service.external = if clear_binding {
                None
            } else {
                previous.external
            };
        }
        if let LaunchConfig::DesktopApp { path, .. } = &mut service.launch {
            *path = applications::inspect_path(path)?.path;
        }
        let saved = if service.kind == "desktopApp" {
            applications.save_service_config(service)?
        } else {
            store.save_service(service)?
        };
        if clear_binding {
            store.clear_application_binding(&saved.id)?;
            applications.forget(&saved.id);
        }
        Ok(saved)
    })
    .await
    .map_err(|e| format!("保存服务任务失败：{e}"))?
}
enum ServiceOperation {
    Start,
    Stop,
    Restart,
    Delete,
    Terminal(Option<String>),
    Convert,
    Focus,
    RequestQuit,
    ForceQuit,
    UnlockApp,
}
/// 所有服务共用操作串行锁和退出屏障，但不同类型只调用自己的生命周期实现。
async fn service_operation(
    state: State<'_, AppState>,
    service_id: String,
    operation: ServiceOperation,
) -> Result<ServiceLaunchResult, String> {
    if state.quitting.load(Ordering::SeqCst) {
        return Err("应用正在退出，请稍后操作".into());
    }
    let gate = state.gate(&service_id)?;
    let store = state.store.clone();
    let runtime = state.runtime.clone();
    let external = state.external.clone();
    let applications = state.applications.clone();
    let static_sites = state.static_sites.clone();
    let lifecycle = state.lifecycle.clone();
    let quitting = state.quitting.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _lifecycle = lifecycle.read().map_err(|_| "应用生命周期锁不可用")?;
        let _guard = gate.lock().map_err(|_| "服务操作锁不可用")?;
        if quitting.load(Ordering::SeqCst) {
            return Err("应用正在退出，请稍后操作".into());
        }
        let service = store.service(&service_id)?;
        let result = |session| {
            let current = match service.kind.as_str() {
                "desktopApp" => applications.runtimes(),
                "staticSite" => static_sites.runtimes(),
                _ if service.source == "external" => external.runtimes(),
                _ => runtime.runtimes(),
            }
            .into_iter()
            .find(|r| r.service_id == service_id)
            .unwrap_or_else(|| ServiceRuntime::stopped(&service_id));
            ServiceLaunchResult {
                runtime: current,
                session,
                candidates: vec![],
            }
        };
        match operation {
            ServiceOperation::Start if service.kind == "desktopApp" => {
                applications.start(service.clone())
            }
            ServiceOperation::Restart if service.kind == "desktopApp" => {
                applications.restart(service.clone())
            }
            ServiceOperation::Start if service.kind == "staticSite" => {
                static_sites.start(service.clone())
            }
            ServiceOperation::Restart if service.kind == "staticSite" => {
                static_sites.restart(service.clone())
            }
            ServiceOperation::Start | ServiceOperation::Restart if service.source == "external" => {
                Err("外部实例需补齐配置后使用“转换为平台托管并启动”。".into())
            }
            ServiceOperation::Start => runtime
                .start_service(service.clone())
                .map(|s| result(Some(s))),
            ServiceOperation::Restart => runtime
                .restart_service(service.clone())
                .map(|s| result(Some(s))),
            ServiceOperation::Stop => {
                match service.kind.as_str() {
                    "desktopApp" => {
                        applications.request_quit(&service_id)?;
                    }
                    "staticSite" => static_sites.stop(&service_id)?,
                    _ if service.source == "external" => external.stop(&service_id)?,
                    _ => runtime.stop_service(&service_id)?,
                }
                Ok(result(None))
            }
            ServiceOperation::Delete => {
                if service.kind == "staticSite" {
                    static_sites.remove_service(&service_id)?;
                }
                // 桌面应用本体不会被 remove_service 清理，该方法只关闭平台拥有的 PTY。
                runtime.remove_service(&service_id)?;
                store.delete_service(&service_id)?;
                applications.forget(&service_id);
                external.forget(&service_id);
                Ok(result(None))
            }
            ServiceOperation::Terminal(agent_id) => {
                let agent = agent_id.map(|id| store.agent(&id)).transpose()?;
                runtime
                    .create_terminal(service.clone(), agent)
                    .map(|s| result(Some(s)))
            }
            ServiceOperation::Convert => {
                if !matches!(service.kind.as_str(), "command" | "frontend") {
                    return Err("此服务类型不使用命令托管转换。".into());
                }
                let session = external.convert(&service_id)?;
                // 转换成功后 source 已变更；不能再按闭包捕获的旧 external 来源返回旧运行态。
                let current = runtime
                    .runtimes()
                    .into_iter()
                    .find(|r| r.service_id == service_id)
                    .unwrap_or_else(|| ServiceRuntime::stopped(&service_id));
                Ok(ServiceLaunchResult {
                    runtime: current,
                    session: Some(session),
                    candidates: vec![],
                })
            }
            ServiceOperation::Focus
            | ServiceOperation::RequestQuit
            | ServiceOperation::ForceQuit
            | ServiceOperation::UnlockApp => {
                if service.kind != "desktopApp" {
                    return Err("此操作只支持桌面应用。".into());
                }
                match operation {
                    ServiceOperation::Focus => applications.focus(&service_id)?,
                    ServiceOperation::RequestQuit => {
                        applications.request_quit(&service_id)?;
                    }
                    ServiceOperation::ForceQuit => {
                        applications.force_quit(&service_id)?;
                    }
                    ServiceOperation::UnlockApp => {
                        applications.unlock(&service_id)?;
                    }
                    _ => unreachable!(),
                }
                Ok(result(None))
            }
        }
    })
    .await
    .map_err(|e| format!("服务操作任务失败：{e}"))?
}
#[tauri::command]
async fn discovery_snapshot(
    state: State<'_, AppState>,
    force: bool,
) -> Result<DiscoverySnapshot, String> {
    let external = state.external.clone();
    let applications = state.applications.clone();
    let static_sites = state.static_sites.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut snapshot = external.snapshot(force);
        let mut owners = applications.managed_processes();
        owners.extend(static_sites.managed_processes());
        for process in &mut snapshot.processes {
            if let Some(id) = owners.get(&process.instance.identity.pid) {
                process.managed_service_id = Some(id.clone());
            }
        }
        snapshot
    })
    .await
    .map_err(|e| format!("发现服务失败：{e}"))
}
#[tauri::command]
async fn attach_external(
    state: State<'_, AppState>,
    request: AttachExternalRequest,
) -> Result<ServiceConfig, String> {
    let gate = state.gate(
        request
            .existing_service_id
            .as_deref()
            .unwrap_or("__external_attach"),
    )?;
    let external = state.external.clone();
    let applications = state.applications.clone();
    let static_sites = state.static_sites.clone();
    let lifecycle = state.lifecycle.clone();
    let quitting = state.quitting.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _lifecycle = lifecycle.read().map_err(|_| "应用生命周期锁不可用")?;
        let _guard = gate.lock().map_err(|_| "服务操作锁不可用")?;
        if quitting.load(Ordering::SeqCst) {
            return Err("应用正在退出，请稍后操作。".into());
        }
        if applications
            .managed_processes()
            .contains_key(&request.identity.pid)
            || static_sites
                .managed_processes()
                .contains_key(&request.identity.pid)
        {
            return Err("该实例已作为应用或静态站点纳入管理，请打开已有记录。".into());
        }
        external.attach(request)
    })
    .await
    .map_err(|e| format!("接入外部实例失败：{e}"))?
}

#[tauri::command]
fn platform_info() -> &'static str {
    if cfg!(target_os = "macos") {
        "macos"
    } else if cfg!(windows) {
        "windows"
    } else {
        "unknown"
    }
}
#[tauri::command]
async fn inspect_frontend_project(path: String) -> Result<FrontendProject, String> {
    tauri::async_runtime::spawn_blocking(move || projects::inspect(&path))
        .await
        .map_err(|e| format!("读取项目失败：{e}"))?
}
#[tauri::command]
async fn inspect_application(path: String) -> Result<ApplicationInspection, String> {
    tauri::async_runtime::spawn_blocking(move || applications::inspect_path(&path))
        .await
        .map_err(|e| format!("读取应用失败：{e}"))?
}
#[tauri::command]
async fn application_snapshot(
    state: State<'_, AppState>,
    force: bool,
) -> Result<ApplicationSnapshot, String> {
    let applications = state.applications.clone();
    let runtime = state.runtime.clone();
    let static_sites = state.static_sites.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let mut snapshot = applications.snapshot(force);
        let mut owners = runtime.managed_processes();
        owners.extend(static_sites.managed_processes());
        for application in &mut snapshot.applications {
            if let Some(id) = owners.get(&application.instance.identity.pid) {
                application.managed_service_id = Some(id.clone());
            }
        }
        snapshot
    })
    .await
    .map_err(|e| format!("发现运行中应用失败：{e}"))
}
#[tauri::command]
async fn attach_application(
    state: State<'_, AppState>,
    request: AttachApplicationRequest,
) -> Result<ServiceConfig, String> {
    let gate = state.gate(
        request
            .existing_service_id
            .as_deref()
            .unwrap_or("__application_attach"),
    )?;
    let applications = state.applications.clone();
    let runtime = state.runtime.clone();
    let static_sites = state.static_sites.clone();
    let lifecycle = state.lifecycle.clone();
    let quitting = state.quitting.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _lifecycle = lifecycle.read().map_err(|_| "应用生命周期锁不可用")?;
        let _guard = gate.lock().map_err(|_| "服务操作锁不可用")?;
        if quitting.load(Ordering::SeqCst) {
            return Err("应用正在退出，请稍后操作。".into());
        }
        if runtime
            .managed_processes()
            .contains_key(&request.identity.pid)
            || static_sites
                .managed_processes()
                .contains_key(&request.identity.pid)
        {
            return Err("该进程属于平台运行的服务，请打开已有记录。".into());
        }
        applications.attach(request)
    })
    .await
    .map_err(|e| format!("关联应用失败：{e}"))?
}
#[tauri::command]
async fn focus_application(state: State<'_, AppState>, service_id: String) -> Result<(), String> {
    service_operation(state, service_id, ServiceOperation::Focus)
        .await
        .map(|_| ())
}
#[tauri::command]
async fn request_quit_application(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<ServiceRuntime, String> {
    service_operation(state, service_id, ServiceOperation::RequestQuit)
        .await
        .map(|result| result.runtime)
}
#[tauri::command]
async fn force_quit_application(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<ServiceRuntime, String> {
    service_operation(state, service_id, ServiceOperation::ForceQuit)
        .await
        .map(|result| result.runtime)
}
#[tauri::command]
async fn unlock_application(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<ServiceRuntime, String> {
    service_operation(state, service_id, ServiceOperation::UnlockApp)
        .await
        .map(|result| result.runtime)
}
#[tauri::command]
async fn unlock_external(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<ServiceRuntime, String> {
    let gate = state.gate(&service_id)?;
    let external = state.external.clone();
    let lifecycle = state.lifecycle.clone();
    let quitting = state.quitting.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let _lifecycle = lifecycle.read().map_err(|_| "应用生命周期锁不可用")?;
        let _guard = gate.lock().map_err(|_| "服务操作锁不可用")?;
        if quitting.load(Ordering::SeqCst) {
            return Err("应用正在退出，请稍后操作。".into());
        }
        external.unlock(&service_id)
    })
    .await
    .map_err(|e| format!("解除保护失败：{e}"))?
}
#[tauri::command]
async fn convert_external(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<TerminalSession, String> {
    service_operation(state, service_id, ServiceOperation::Convert)
        .await?
        .session
        .ok_or("转换未返回终端会话。".into())
}
#[tauri::command]
async fn start_service(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<ServiceLaunchResult, String> {
    service_operation(state, service_id, ServiceOperation::Start).await
}
#[tauri::command]
async fn stop_service(state: State<'_, AppState>, service_id: String) -> Result<(), String> {
    service_operation(state, service_id, ServiceOperation::Stop)
        .await
        .map(|_| ())
}
#[tauri::command]
async fn restart_service(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<ServiceLaunchResult, String> {
    service_operation(state, service_id, ServiceOperation::Restart).await
}
#[tauri::command]
async fn delete_service(state: State<'_, AppState>, service_id: String) -> Result<(), String> {
    service_operation(state, service_id, ServiceOperation::Delete)
        .await
        .map(|_| ())
}
#[tauri::command]
async fn create_terminal(
    state: State<'_, AppState>,
    service_id: String,
    agent_id: Option<String>,
) -> Result<TerminalSession, String> {
    service_operation(state, service_id, ServiceOperation::Terminal(agent_id))
        .await?
        .session
        .ok_or("未能创建终端会话".into())
}
#[tauri::command]
async fn close_terminal(state: State<'_, AppState>, session_id: String) -> Result<(), String> {
    let runtime = state.runtime.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.close_terminal(&session_id))
        .await
        .map_err(|e| format!("关闭终端任务失败：{e}"))?
}
#[tauri::command]
async fn write_terminal(
    state: State<'_, AppState>,
    session_id: String,
    data: String,
) -> Result<(), String> {
    if data.len() > 1024 * 1024 {
        return Err("单次终端输入过大，请分段粘贴".into());
    }
    let runtime = state.runtime.clone();
    tauri::async_runtime::spawn_blocking(move || runtime.write(&session_id, &data))
        .await
        .map_err(|e| format!("终端输入任务失败：{e}"))?
}
#[tauri::command]
async fn resize_terminal(
    state: State<'_, AppState>,
    session_id: String,
    cols: u16,
    rows: u16,
) -> Result<(), String> {
    if !(2..=1000).contains(&cols) || !(1..=1000).contains(&rows) {
        return Err("终端尺寸无效".into());
    }
    state.runtime.resize(&session_id, cols, rows)
}
#[tauri::command]
async fn subscribe_terminal(
    state: State<'_, AppState>,
    session_id: String,
    channel: Channel<TerminalEvent>,
    after_seq: u64,
) -> Result<(), String> {
    state.runtime.subscribe(&session_id, channel, after_seq)
}
#[tauri::command]
async fn ack_terminal(
    state: State<'_, AppState>,
    session_id: String,
    seq: u64,
) -> Result<(), String> {
    state.runtime.ack(&session_id, seq)
}

#[tauri::command]
async fn save_agent(
    state: State<'_, AppState>,
    agent: AgentProfile,
) -> Result<AgentProfile, String> {
    state.store.save_agent(agent)
}
#[tauri::command]
async fn delete_agent(state: State<'_, AppState>, agent_id: String) -> Result<(), String> {
    state.store.delete_agent(&agent_id)
}
#[tauri::command]
async fn check_agent(
    state: State<'_, AppState>,
    agent_id: String,
    service_id: Option<String>,
) -> Result<AgentAvailability, String> {
    let agent = state.store.agent(&agent_id)?;
    let service = service_id.map(|id| state.store.service(&id)).transpose()?;
    tauri::async_runtime::spawn_blocking(move || runtime::check_agent(&agent, service.as_ref()))
        .await
        .map_err(|e| format!("工具检测失败：{e}"))
}
#[tauri::command]
async fn save_preferences(
    state: State<'_, AppState>,
    preferences: Preferences,
) -> Result<Preferences, String> {
    state.store.save_preferences(preferences)
}
#[tauri::command]
async fn list_documents(
    state: State<'_, AppState>,
    service_id: String,
) -> Result<Vec<ServiceDocument>, String> {
    state.store.documents(&service_id)
}
#[tauri::command]
async fn save_document(
    state: State<'_, AppState>,
    document: ServiceDocument,
) -> Result<ServiceDocument, String> {
    state.store.save_document(document)
}
#[tauri::command]
async fn delete_document(state: State<'_, AppState>, document_id: String) -> Result<(), String> {
    state.store.delete_document(&document_id)
}
#[tauri::command]
async fn export_document(
    state: State<'_, AppState>,
    document_id: String,
    path: String,
) -> Result<(), String> {
    let document = state.store.document(&document_id)?;
    let path = PathBuf::from(path);
    if !path.is_absolute()
        || path
            .extension()
            .and_then(|s| s.to_str())
            .map(|s| !s.eq_ignore_ascii_case("md"))
            .unwrap_or(true)
    {
        return Err("请选择以 .md 结尾的完整文件路径".into());
    }
    tauri::async_runtime::spawn_blocking(move || {
        use std::io::Write;
        let parent = path.parent().ok_or("导出目录无效")?;
        // 在目标所在目录完成写入后再原子替换，磁盘满、权限不足或写入失败均不会截断已有文件。
        // tempfile 在 Windows 使用支持替换的系统调用，保持与 macOS 一致的覆盖语义。
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .map_err(|e| format!("创建导出临时文件失败：{e}"))?;
        temporary
            .write_all(document.content.as_bytes())
            .map_err(|e| format!("写入导出文档失败：{e}"))?;
        temporary
            .as_file()
            .sync_all()
            .map_err(|e| format!("保存导出文档失败：{e}"))?;
        temporary
            .persist(&path)
            .map_err(|e| format!("替换导出文档失败：{}", e.error))?;
        Ok(())
    })
    .await
    .map_err(|e| format!("导出文档任务失败：{e}"))?
}
#[tauri::command]
async fn open_service_directory(
    state: State<'_, AppState>,
    service_id: String,
    kind: String,
) -> Result<(), String> {
    let service = state.store.service(&service_id)?;
    let path = match kind.as_str() {
        "work" => service.working_dir,
        "environment" => service.environment_dir,
        _ => return Err("未知目录类型".into()),
    };
    if path.is_empty() || !PathBuf::from(&path).is_dir() {
        return Err("目录不存在或无法访问，请编辑服务重新选择".into());
    }
    open::that_detached(path).map_err(|e| format!("打开目录失败：{e}"))
}
#[tauri::command]
fn open_external_url(url: String) -> Result<(), String> {
    // Markdown 链接只允许用户明确点击后打开；先解析完整 URL，再限制协议，
    // 防止自定义 scheme、相对路径或格式错误的字符串交给系统处理程序。
    let parsed = Url::parse(&url).map_err(|_| "链接格式无效。".to_owned())?;
    if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
        return Err("只支持有效的 HTTP 或 HTTPS 链接。".into());
    }
    open::that_detached(parsed.as_str()).map_err(|e| format!("无法打开系统浏览器：{e}"))
}
#[tauri::command]
async fn quit_app(app: tauri::AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    if state.quitting.swap(true, Ordering::SeqCst) {
        return Ok(());
    }
    state.content.shutdown();
    let _ = state.speech.cancel();
    let runtime = state.runtime.clone();
    let static_sites = state.static_sites.clone();
    let lifecycle = state.lifecycle.clone();
    let result = tauri::async_runtime::spawn_blocking(move || {
        let _lifecycle = lifecycle.write().map_err(|_| "应用生命周期锁不可用")?;
        // 桌面应用没有参与此清理清单，完全退出后它们仍继续运行。
        let terminals = runtime.stop_all();
        let previews = static_sites.stop_all();
        terminals.and(previews)
    })
    .await
    .map_err(|e| format!("退出清理失败：{e}"))
    .and_then(|r| r);
    if let Err(error) = result {
        state.quitting.store(false, Ordering::SeqCst);
        state.content.resume();
        return Err(error);
    }
    app.exit(0);
    Ok(())
}

#[tauri::command]
fn take_quit_request(state: State<'_, AppState>) -> bool {
    state.quit_requested.swap(false, Ordering::SeqCst)
}

fn request_quit(app: &tauri::AppHandle) {
    // 托盘在 WebView 监听就绪前就可操作；先保存请求，再发通知，防止冷启动时点击退出丢失。
    // 前端通过原子消费领取请求，所以监听与启动补查同时完成也只会提示一次。
    app.state::<AppState>()
        .quit_requested
        .store(true, Ordering::SeqCst);
    show_window(app);
    let _ = app.emit("quit-requested", ());
}

fn show_window(app: &tauri::AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
    }
}
pub fn run() {
    // Windows PTY 引导进程必须在 Tauri 与单实例插件初始化前运行，不能被识别为第二个应用实例。
    if content_mcp::run_stdio_if_requested()
        || static_sites::run_worker_if_requested()
        || runtime::run_worker_if_requested()
    {
        return;
    }
    let app = tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            show_window(app)
        }))
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            // 测试可指定隔离数据目录，正式启动则使用系统应用数据目录，不写入被管理项目。
            let data_dir = std::env::var_os("KACENT_DATA_DIR")
                .map(PathBuf::from)
                .unwrap_or(app.path().app_data_dir()?);
            std::fs::create_dir_all(&data_dir)?;
            let store = Arc::new(
                Store::open(&data_dir.join("services.sqlite3")).map_err(std::io::Error::other)?,
            );
            let runtime = RuntimeManager::new(Some(app.handle().clone()));
            let external = external::ExternalManager::new(
                store.clone(),
                runtime.clone(),
                Some(app.handle().clone()),
            );
            external.start_monitor();
            let applications = applications::ApplicationManager::new(
                store.clone(),
                runtime.discovery(),
                Some(app.handle().clone()),
            );
            let static_sites = static_sites::StaticSiteManager::new(
                runtime.discovery(),
                Some(app.handle().clone()),
            );
            applications.start_monitor();
            let content_manager = content::ContentManager::new(store.clone(), applications.clone());
            let content = content_control::ContentController::new(content_manager);
            let content_bridge_error =
                content_mcp::start(content.clone(), &data_dir, Some(app.handle().clone())).err();
            let speech = speech::SpeechManager::new(data_dir);
            app.manage(AppState {
                content,
                speech,
                content_bridge_error,
                store,
                runtime,
                external,
                applications,
                static_sites,
                operations: Mutex::new(HashMap::new()),
                quitting: Arc::new(AtomicBool::new(false)),
                quit_requested: AtomicBool::new(false),
                lifecycle: Arc::new(RwLock::new(())),
            });
            let show = MenuItem::with_id(app, "show", "显示窗口", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "停止托管服务并退出", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&show, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().cloned().unwrap_or_else(|| {
                    tauri::image::Image::new_owned(vec![65, 130, 240, 255], 1, 1)
                }))
                .tooltip("服务中心")
                .menu(&menu)
                .show_menu_on_left_click(false)
                .on_menu_event(|app, event| match event.id.as_ref() {
                    "show" => show_window(app),
                    "quit" => request_quit(app),
                    _ => {}
                })
                .on_tray_icon_event(|tray, event| {
                    if matches!(
                        event,
                        TrayIconEvent::Click {
                            button: MouseButton::Left,
                            button_state: MouseButtonState::Up,
                            ..
                        }
                    ) {
                        show_window(tray.app_handle());
                    }
                })
                .build(app)?;
            Ok(())
        })
        .on_window_event(|window, event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                let _ = window.hide();
            }
        })
        .invoke_handler(tauri::generate_handler![
            content_snapshot,
            content_save,
            content_associate,
            content_tasks,
            content_submit,
            content_plan,
            content_confirm,
            content_choose,
            content_cancel,
            content_save_form,
            content_agent_capabilities,
            content_preferences,
            save_content_preferences,
            content_mcp_config,
            content_mcp_check,
            speech_status,
            speech_start,
            speech_cancel,
            speech_finish,
            speech_download,
            speech_import,
            bootstrap,
            discovery_snapshot,
            platform_info,
            inspect_frontend_project,
            inspect_application,
            application_snapshot,
            attach_application,
            focus_application,
            request_quit_application,
            force_quit_application,
            unlock_application,
            attach_external,
            unlock_external,
            convert_external,
            save_service,
            delete_service,
            start_service,
            stop_service,
            restart_service,
            create_terminal,
            close_terminal,
            write_terminal,
            resize_terminal,
            subscribe_terminal,
            ack_terminal,
            save_agent,
            delete_agent,
            check_agent,
            save_preferences,
            list_documents,
            save_document,
            delete_document,
            export_document,
            open_service_directory,
            open_external_url,
            quit_app,
            take_quit_request
        ])
        .build(tauri::generate_context!())
        .expect("服务中心初始化失败");
    app.run(|app, event| {
        if let tauri::RunEvent::ExitRequested { api, .. } = event {
            if !app.state::<AppState>().quitting.load(Ordering::SeqCst) {
                api.prevent_exit();
                request_quit(app);
            }
        }
    });
}
