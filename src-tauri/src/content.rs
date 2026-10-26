//! 独立内容库管理器。
//!
//! 内容收藏只保存展示配置与明确的目标引用。文件夹和 Steam 目标不产生服务记录；本地
//! 应用通过 `ApplicationManager` 的共享原生目标接口启动和控制，不创建 PTY；已有桌面
//! 服务通过 `serviceLink` 引用，因而它的启动配置与 Markdown 文档始终留在原服务下。

use crate::applications::{
    ApplicationManager, ApplicationOwner, ApplicationServiceSnapshot, ApplicationTarget,
    ApplicationTargetLaunch,
};
use crate::content_models::{
    validate_content_item, ContentApplicationBinding, ContentApplicationCandidate, ContentItem,
    ContentStatus, ContentTarget,
};
use crate::models::{LaunchConfig, ProcessIdentity, ServiceConfig};
use crate::store::Store;
use chrono::{SecondsFormat, Utc};
use serde_json::Value;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// 内容库的后端管理器。`operation` 串行化配置编辑、身份关联和应用控制，保证确认快照
/// 在校验后到执行前不会被同一个管理器中的另一条操作改写。
pub struct ContentManager {
    store: Arc<Store>,
    applications: Arc<ApplicationManager>,
    operation: Mutex<()>,
}

impl ContentManager {
    pub fn new(store: Arc<Store>, applications: Arc<ApplicationManager>) -> Arc<Self> {
        Arc::new(Self {
            store,
            applications,
            operation: Mutex::new(()),
        })
    }

    /// 读取内容配置；运行态由 `statuses` 单独计算。
    pub fn items(&self) -> Result<Vec<ContentItem>, String> {
        self.store.content_items()
    }

    /// 保存内容配置。无确认戳时适用于普通编辑；高风险确认流程应调用 `save_checked`。
    pub fn save(&self, item: ContentItem) -> Result<ContentItem, String> {
        self.save_checked(item, None)
    }

    /// 在同一内容操作锁内核验旧快照后保存，防止确认之后目标或原生关联发生变化。
    pub fn save_checked(
        &self,
        mut item: ContentItem,
        expected_stamp: Option<&str>,
    ) -> Result<ContentItem, String> {
        let _operation = self.lock_operation()?;
        self.check_expected_stamp(item.id.trim(), expected_stamp)?;
        item.target = item.target.normalized()?;
        validate_content_item(&item)?;
        self.validate_target_reference(&item)?;
        if !item.id.trim().is_empty() {
            let existing = self.store.content_item(item.id.trim())?;
            if existing.target != item.target && self.target_is_running(&existing)? {
                return Err("内容目标正在运行，不能直接修改目标；请先让实例退出后再编辑。".into());
            }
        }
        self.store.save_content_item(item)
    }

    /// 删除只解除内容自身的应用关联，不停止仍在运行的原生应用，也不删除 serviceLink
    /// 指向的服务或服务文档。
    pub fn remove(&self, id: &str) -> Result<(), String> {
        self.remove_checked(id, None)
    }

    pub fn remove_checked(&self, id: &str, expected_stamp: Option<&str>) -> Result<(), String> {
        let _operation = self.lock_operation()?;
        if id.trim().is_empty() {
            return Err("内容 ID 不能为空。".into());
        }
        self.check_expected_stamp(id.trim(), expected_stamp)?;
        self.store.delete_content_item(id.trim())?;
        // 删除收藏只解除其自身所有权；原生进程仍可运行，但该收藏的待关联状态不能
        // 残留到下一条同 ID 记录上。
        self.applications.clear_content_pending(id.trim());
        Ok(())
    }

    /// 计算所有内容的即时运行态；状态从不写回配置表。
    pub fn statuses(&self) -> Result<Vec<ContentStatus>, String> {
        let _operation = self.lock_operation()?;
        let items = self.store.content_items()?;
        // 单条应用权限错误不能让整页状态消失；将该条标为 unknown 并保留具体理由，
        // 其他收藏仍正常返回，前端也不会据空列表误判内容全部不可用。
        items
            .iter()
            .map(|item| match self.status_for(item) {
                Ok(status) => Ok(status),
                Err(error) => Ok(self.unknown_status(item, error)),
            })
            .collect()
    }

    /// 将用户在多实例选择器中明确选择的原生应用身份写入内容关联。
    pub fn associate(&self, id: &str, identity: ProcessIdentity) -> Result<(), String> {
        let _operation = self.lock_operation()?;
        let item = self.store.content_item(id.trim())?;
        let ContentTarget::LocalApp { path, .. } = &item.target else {
            return Err("只有 localApp 内容可以关联原生应用实例。".into());
        };
        let Some(application) = self
            .applications
            .inspect_content_identity(&identity, path)?
        else {
            return Err("所选应用已退出或 PID 创建身份已变化。".into());
        };
        if application.managed_service_id.is_some() {
            return Err("该实例已由桌面服务关联，请收藏对应服务。".into());
        }
        // 显式选择新实例也不能悄悄丢掉仍在运行的旧关联。只有原关联的完整创建身份
        // 已明确不存在，才允许用户把收藏转给新实例；核验失败时宁可保守拒绝。
        if let Some(previous) = self.store.content_application_binding(id.trim())? {
            if previous.identity != identity {
                match self
                    .applications
                    .inspect_content_identity(&previous.identity, &previous.target_path)
                {
                    Ok(Some(_)) => {
                        return Err(
                            "该收藏仍关联另一个运行中的应用实例，请先让旧实例退出后再重新关联。"
                                .into(),
                        )
                    }
                    Ok(None) => {}
                    Err(error) => {
                        return Err(format!("无法确认原关联实例已退出，已拒绝替换关联：{error}"))
                    }
                }
            }
        }
        let binding = ContentApplicationBinding {
            identity,
            target_path: application.path,
            bound_at: timestamp_now(),
        };
        self.store.bind_content_application(id.trim(), &binding)?;
        // 显式选择已经完成接管；清理按身份记录的所有待关联项，避免其它收藏把同一
        // 原生实例继续显示成 unknown。Store 随后也会以完整身份拒绝重复绑定。
        self.applications
            .clear_content_pending_identity(&binding.identity);
        Ok(())
    }

    /// 返回与当前配置和完整原生创建身份绑定的确认快照。PID 单独变化或路径变化都会
    /// 生成不同戳，调用方不能拿旧确认控制新实例。
    pub fn confirmation_stamp(&self, id: &str) -> Result<String, String> {
        let _operation = self.lock_operation()?;
        self.confirmation_stamp_locked(id.trim())
    }

    /// 需要前端确认的动作层入口。`expected_stamp` 为空时仍会执行后端自身的身份校验；
    /// 传入快照后则会在同一操作锁内重读并比较，避免确认期间换记录或换实例。
    pub fn execute_checked(
        &self,
        id: &str,
        action: &str,
        expected_stamp: Option<&str>,
    ) -> Result<Value, String> {
        let _operation = self.lock_operation()?;
        let item = self.store.content_item(id.trim())?;
        // 必须先捕获 serviceLink 的完整配置/身份快照，再核验内容确认戳。若旧服务
        // 界面在两步之间重启并换了 binding，后面的戳核验会失败；即使无二次确认戳，
        // 动作层也会在应用操作锁内再次比较这份快照，不能拿新实例补上旧确认。
        let expected_service_snapshot = self.capture_service_link_snapshot(&item)?;
        self.check_expected_stamp_locked(id.trim(), expected_stamp)?;
        let action = action.trim();
        match action {
            "status" => serde_json::to_value(
                self.status_for(&item)
                    .unwrap_or_else(|error| self.unknown_status(&item, error)),
            )
            .map_err(|error| format!("无法序列化内容状态：{error}")),
            "open" | "start" | "focus" | "requestQuit" | "forceQuit" => {
                self.execute_action(&item, action, expected_service_snapshot.as_ref())
            }
            other => Err(format!("不支持的内容操作：{other}")),
        }
    }

    /// 默认动作入口，保留给不需要二次确认的低风险调用方。
    pub fn execute(&self, id: &str, action: &str) -> Result<Value, String> {
        self.execute_checked(id, action, None)
    }

    fn execute_action(
        &self,
        item: &ContentItem,
        action: &str,
        expected_service_snapshot: Option<&ApplicationServiceSnapshot>,
    ) -> Result<Value, String> {
        match &item.target {
            ContentTarget::Folder { path } => {
                if action != "open" {
                    return Err("文件夹内容只支持 open 和 status。".into());
                }
                if let Err(error) = folder_availability(path) {
                    return Err(error);
                }
                self.applications.open_location(path)?;
                self.store.touch_content(&item.id)?;
                self.value_status(item)
            }
            ContentTarget::Steam { app_id } => {
                if !matches!(action, "open" | "start") {
                    return Err("Steam 内容只支持 open、start 和 status。".into());
                }
                let uri = format!("steam://run/{}", app_id.trim());
                self.applications.open_location(&uri)?;
                self.store.touch_content(&item.id)?;
                if action == "start" {
                    self.value_status_with_message(item, "Steam 启动请求已发送。")
                } else {
                    self.value_status(item)
                }
            }
            ContentTarget::LocalApp {
                path,
                args,
                start_dir,
            } => self.execute_local(item, action, path, args, start_dir),
            ContentTarget::ServiceLink { service_id } => {
                self.execute_service_link(item, action, service_id, expected_service_snapshot)
            }
        }
    }

    fn execute_local(
        &self,
        item: &ContentItem,
        action: &str,
        path: &str,
        args: &[String],
        start_dir: &str,
    ) -> Result<Value, String> {
        if action == "open" {
            if let Some(binding) = self.store.content_application_binding(&item.id)? {
                if self
                    .applications
                    .inspect_content_identity(&binding.identity, &binding.target_path)?
                    .is_some()
                {
                    self.applications
                        .focus_content(&binding.identity, &binding.target_path)?;
                    self.store.touch_content(&item.id)?;
                    return self.value_status(item);
                }
            }
            return self.execute_local(item, "start", path, args, start_dir);
        }
        if action == "start" {
            if let Some(binding) = self.store.content_application_binding(&item.id)? {
                match self
                    .applications
                    .inspect_content_identity(&binding.identity, &binding.target_path)?
                {
                    Some(_) => return self.value_status(item),
                    None => self.store.clear_content_application_binding(&item.id)?,
                }
            }
            let target = self.applications.validate_target(&ApplicationTarget {
                path: path.to_owned(),
                args: args.to_vec(),
                start_dir: start_dir.to_owned(),
            })?;
            let launch = self
                .applications
                .start_content_target(&ApplicationOwner::Content(item.id.clone()), &target)?;
            match launch {
                ApplicationTargetLaunch::SelectionRequired(_) => self.value_status(item),
                ApplicationTargetLaunch::Pending(_) => self.value_status(item),
                ApplicationTargetLaunch::Running(application) => {
                    if application.managed_service_id.is_some() {
                        return Err("目标实例已由桌面服务关联，请改用 serviceLink 收藏。".into());
                    }
                    self.store.bind_content_application(
                        &item.id,
                        &ContentApplicationBinding {
                            identity: application.instance.identity.clone(),
                            target_path: application.path,
                            bound_at: timestamp_now(),
                        },
                    )?;
                    // start_content_target 在返回 Running 前先登记短暂保留，用来覆盖
                    // 应用锁释放到内容关联落库之间的窗口；落库成功后再按完整身份释放。
                    self.applications
                        .clear_content_pending_identity(&application.instance.identity);
                    self.store.touch_content(&item.id)?;
                    self.value_status(item)
                }
            }
        } else {
            let binding = self
                .store
                .content_application_binding(&item.id)?
                .ok_or_else(|| "内容尚未关联运行中的本地应用实例。".to_owned())?;
            match action {
                "focus" => self
                    .applications
                    .focus_content(&binding.identity, &binding.target_path)?,
                "requestQuit" => {
                    self.applications
                        .request_quit_content(&binding.identity, &binding.target_path)?;
                    // 应用确认退出后只解除身份关联，内容配置继续保留。
                    self.store.clear_content_application_binding(&item.id)?;
                }
                "forceQuit" => {
                    self.applications
                        .force_quit_content(&binding.identity, &binding.target_path)?;
                    self.store.clear_content_application_binding(&item.id)?;
                }
                _ => return Err(format!("本地应用不支持操作：{action}")),
            }
            self.store.touch_content(&item.id)?;
            self.value_status(item)
        }
    }

    fn execute_service_link(
        &self,
        item: &ContentItem,
        action: &str,
        service_id: &str,
        expected_snapshot: Option<&ApplicationServiceSnapshot>,
    ) -> Result<Value, String> {
        self.service_link_config(service_id)?;
        let expected_snapshot = expected_snapshot
            .ok_or_else(|| "无法取得桌面服务确认快照，请刷新后重试。".to_owned())?;
        if action == "open" {
            // 内容收藏的 open 语义是打开/显示原生应用；服务详情导航由服务 UI 自己负责。
            if self.applications.service_is_running(service_id)? {
                self.applications
                    .service_link_action_snapshot(service_id, "focus", expected_snapshot)
                    .map_err(|error| format!("无法显示已关联桌面应用：{error}"))?;
                self.store.touch_content(&item.id)?;
                return self.value_status(item);
            }
            self.applications.service_link_action_snapshot(
                service_id,
                "start",
                expected_snapshot,
            )?;
            self.store.touch_content(&item.id)?;
            return self.value_status(item);
        }
        match action {
            "start" | "focus" | "requestQuit" | "forceQuit" => self
                .applications
                .service_link_action_snapshot(service_id, action, expected_snapshot)?,
            _ => return Err(format!("服务关联不支持操作：{action}")),
        }
        self.store.touch_content(&item.id)?;
        self.value_status(item)
    }

    fn value_status(&self, item: &ContentItem) -> Result<Value, String> {
        let status = match self.status_for(item) {
            Ok(status) => status,
            Err(error) => self.unknown_status(item, error),
        };
        serde_json::to_value(status).map_err(|error| format!("无法序列化内容状态：{error}"))
    }

    fn value_status_with_message(
        &self,
        item: &ContentItem,
        message: &str,
    ) -> Result<Value, String> {
        let mut status = match self.status_for(item) {
            Ok(status) => status,
            Err(error) => self.unknown_status(item, error),
        };
        status.message = Some(message.into());
        serde_json::to_value(status).map_err(|error| format!("无法序列化内容状态：{error}"))
    }

    fn unknown_status(&self, item: &ContentItem, error: String) -> ContentStatus {
        ContentStatus {
            content_id: item.id.clone(),
            state: "unknown".into(),
            message: Some(error),
            confirmation_stamp: self.confirmation_stamp_locked(&item.id).unwrap_or_default(),
            ..ContentStatus::default()
        }
    }

    fn status_for(&self, item: &ContentItem) -> Result<ContentStatus, String> {
        let mut status = ContentStatus {
            content_id: item.id.clone(),
            confirmation_stamp: self.confirmation_stamp_locked(&item.id)?,
            ..ContentStatus::default()
        };
        match &item.target {
            ContentTarget::Folder { path } => {
                match folder_availability(path) {
                    Ok(()) => status.available = true,
                    Err(error) if error.starts_with("目标文件夹不存在") => {
                        status.available = false;
                        status.message = Some(error);
                    }
                    Err(error) => {
                        status.state = "unknown".into();
                        status.message = Some(error);
                        return Ok(status);
                    }
                }
                status.state = if status.available {
                    "available"
                } else {
                    "unavailable"
                }
                .into();
                status.can_open = status.available;
            }
            ContentTarget::Steam { app_id } => {
                status.available = app_id.trim().bytes().all(|byte| byte.is_ascii_digit())
                    && !app_id.trim().is_empty();
                status.state = if status.available {
                    "available"
                } else {
                    "unavailable"
                }
                .into();
                status.target_uri = Some(format!("steam://run/{}", app_id.trim()));
                status.can_open = status.available;
                status.can_start = status.available;
            }
            ContentTarget::LocalApp {
                path,
                args,
                start_dir,
            } => {
                self.fill_local_status(item, path, args, start_dir, &mut status)?;
            }
            ContentTarget::ServiceLink { service_id } => {
                self.fill_service_status(service_id, &mut status)?;
            }
        }
        Ok(status)
    }

    fn fill_local_status(
        &self,
        item: &ContentItem,
        path: &str,
        args: &[String],
        start_dir: &str,
        status: &mut ContentStatus,
    ) -> Result<(), String> {
        let binding = self.store.content_application_binding(&item.id)?;
        match self.applications.validate_target(&ApplicationTarget {
            path: path.to_owned(),
            args: args.to_vec(),
            start_dir: start_dir.to_owned(),
        }) {
            Err(error) => {
                status.state = "unavailable".into();
                status.message = Some(error);
                status.can_open = false;
                status.can_start = false;
            }
            Ok(_) => {
                status.available = true;
                // 停止态的 open 会启动应用；运行态则必须真的能显示窗口并通过原生
                // 控制保护检查，否则前端不应提供一个必然被后端拒绝的按钮。
                status.can_open = true;
                if let Some(pending) = self.applications.content_pending(path)? {
                    // 启动请求已交给系统但身份还未能安全核验时，不能把 open/start
                    // 重新开放；只有明确退出或显式关联后才结束这条待关联状态。
                    status.state = "unknown".into();
                    status.can_open = false;
                    status.can_start = false;
                    status.message = Some(format!("{} 请明确选择可核验的同路径实例；若启动器已转交其他应用，可移除收藏后重新选择实际应用。", pending.reason));
                    status.candidates = self
                        .applications
                        .content_candidates(path)?
                        .iter()
                        .map(candidate_from_application)
                        .collect();
                    return Ok(());
                }
                if let Some(binding) = binding {
                    match self
                        .applications
                        .inspect_content_identity(&binding.identity, &binding.target_path)?
                    {
                        Some(application) => {
                            status.state = "running".into();
                            status.running = true;
                            status.identity = Some(binding.identity.clone());
                            status.pid = Some(binding.identity.pid);
                            let controllable = application_control_allowed(&application.instance);
                            status.can_open = controllable && application.window_count > 0;
                            status.can_focus = controllable && application.window_count > 0;
                            status.can_request_quit = controllable
                                && self
                                    .applications
                                    .content_can_request_quit(application.window_count);
                            status.can_force_quit = controllable;
                            status.can_start = false;
                            if !controllable {
                                status.message = Some(
                                    application
                                        .instance
                                        .control_reason
                                        .clone()
                                        .unwrap_or_else(|| "当前应用受保护或不可控制。".into()),
                                );
                            } else if application.window_count == 0 {
                                status.message = Some("该应用当前没有可显示的窗口。".into());
                            }
                        }
                        None => {
                            status.state = "stopped".into();
                            status.can_start = true;
                        }
                    }
                } else {
                    let candidates = self.applications.content_candidates(path)?;
                    if candidates
                        .iter()
                        .any(|candidate| candidate.managed_service_id.is_some())
                    {
                        status.state = "unavailable".into();
                        status.can_open = false;
                        status.can_start = false;
                        status.candidates =
                            candidates.iter().map(candidate_from_application).collect();
                        status.message =
                            Some("该实例已由桌面服务关联，请收藏对应服务而不是重复接管。".into());
                        return Ok(());
                    }
                    if candidates.len() > 1 {
                        status.state = "selectionRequired".into();
                        status.can_open = false;
                        status.can_start = false;
                        status.candidates =
                            candidates.iter().map(candidate_from_application).collect();
                        status.message =
                            Some("同路径存在多个运行实例，请先选择要关联的实例。".into());
                    } else {
                        status.state = "stopped".into();
                        status.can_start = true;
                        status.candidates =
                            candidates.iter().map(candidate_from_application).collect();
                    }
                }
            }
        }
        Ok(())
    }

    fn fill_service_status(
        &self,
        service_id: &str,
        status: &mut ContentStatus,
    ) -> Result<(), String> {
        let service = match self.store.service(service_id) {
            Ok(service) => service,
            Err(error) => {
                status.state = "unavailable".into();
                status.message = Some(error);
                return Ok(());
            }
        };
        if service.kind != "desktopApp" {
            status.state = "unavailable".into();
            status.message = Some("serviceLink 只能指向 desktopApp 服务。".into());
            return Ok(());
        }
        let Some((path, args, start_dir)) = desktop_target(&service) else {
            status.state = "unavailable".into();
            status.message = Some("桌面服务缺少有效的应用目标。".into());
            return Ok(());
        };
        let target = match self.applications.validate_target(&ApplicationTarget {
            path: path.clone(),
            args,
            start_dir,
        }) {
            Ok(target) => target,
            Err(error) => {
                status.state = "unavailable".into();
                status.message = Some(error);
                return Ok(());
            }
        };
        // 现有确认控制器优先读取 status.targetUri 作为具体目标标签。serviceLink 自身只
        // 保存 serviceId，必须在运行态里填入当前规范化路径，避免弹窗只显示内部 ID。
        status.target_uri = Some(target.path.clone());
        status.available = Path::new(&target.path).exists();
        status.can_open = status.available;
        match self.store.application_binding(service_id)? {
            Some(binding) => match self
                .applications
                .inspect_content_identity(&binding.instance.identity, &binding.target_path)?
            {
                Some(application) => {
                    status.state = "running".into();
                    status.running = true;
                    status.identity = Some(binding.instance.identity.clone());
                    status.pid = Some(binding.instance.identity.pid);
                    let controllable = application_control_allowed(&application.instance);
                    status.can_open = controllable && application.window_count > 0;
                    status.can_focus = controllable && application.window_count > 0;
                    status.can_request_quit = controllable
                        && self
                            .applications
                            .content_can_request_quit(application.window_count);
                    status.can_force_quit = controllable;
                    status.can_start = false;
                    if !controllable {
                        status.message = Some(
                            application
                                .instance
                                .control_reason
                                .clone()
                                .unwrap_or_else(|| "当前应用受保护或不可控制。".into()),
                        );
                    } else if application.window_count == 0 {
                        status.message = Some("该应用当前没有可显示的窗口。".into());
                    }
                }
                None => {
                    status.state = if status.available {
                        "stopped"
                    } else {
                        "unavailable"
                    }
                    .into();
                    status.can_start = status.available;
                }
            },
            None => {
                status.state = if status.available {
                    "stopped"
                } else {
                    "unavailable"
                }
                .into();
                status.can_start = status.available;
            }
        }
        Ok(())
    }

    fn validate_target_reference(&self, item: &ContentItem) -> Result<(), String> {
        if let ContentTarget::ServiceLink { service_id } = &item.target {
            let service = self.store.service(service_id)?;
            if service.kind != "desktopApp" {
                return Err("serviceLink 只能指向 desktopApp 服务。".into());
            }
            if !matches!(service.launch, LaunchConfig::DesktopApp { .. }) {
                return Err("目标服务缺少 desktopApp 启动配置。".into());
            }
        }
        Ok(())
    }

    fn target_is_running(&self, item: &ContentItem) -> Result<bool, String> {
        match &item.target {
            ContentTarget::LocalApp { path, .. } => {
                if let Some(binding) = self.store.content_application_binding(&item.id)? {
                    return Ok(self
                        .applications
                        .inspect_content_identity(&binding.identity, &binding.target_path)?
                        .is_some());
                }
                if self.applications.content_pending(path)?.is_some() {
                    return Ok(true);
                }
                Ok(!self.applications.content_candidates(path)?.is_empty())
            }
            ContentTarget::ServiceLink { service_id } => {
                self.applications.service_is_running(service_id)
            }
            ContentTarget::Folder { .. } | ContentTarget::Steam { .. } => Ok(false),
        }
    }

    fn service_link_config(&self, service_id: &str) -> Result<ServiceConfig, String> {
        let service = self.store.service(service_id)?;
        if service.kind != "desktopApp" {
            return Err("serviceLink 只能控制 desktopApp 服务。".into());
        }
        Ok(service)
    }

    /// 在 stamp 校验之前取得 serviceLink 的配置版本、目标和完整运行身份。
    ///
    /// 不能在 stamp 校验通过后才读取 binding：旧服务 UI 可能恰好在这段窗口内完成
    /// restart，新 binding 会被误当成刚刚确认过的实例。快照动作层会在共享应用锁内
    /// 重读并比较它；非 serviceLink 内容不需要这份跨管理器快照。
    fn capture_service_link_snapshot(
        &self,
        item: &ContentItem,
    ) -> Result<Option<ApplicationServiceSnapshot>, String> {
        let ContentTarget::ServiceLink { service_id } = &item.target else {
            return Ok(None);
        };
        self.applications
            .service_application_snapshot(service_id)
            .map(Some)
    }

    fn confirmation_stamp_locked(&self, id: &str) -> Result<String, String> {
        let item = self.store.content_item(id)?;
        let mut stamp = format!(
            "content-v1|{}|{}|{}|{}",
            item.id,
            item.revision,
            item.updated_at,
            item.target.stable_key()
        );
        if let Some(binding) = self.store.content_application_binding(id)? {
            stamp.push_str(&format!(
                "|identity:{}:{}|path:{}",
                binding.identity.pid, binding.identity.created_at, binding.target_path
            ));
        }
        if let ContentTarget::ServiceLink { service_id } = &item.target {
            if let Ok(snapshot) = self.applications.service_application_snapshot(service_id) {
                stamp.push_str(&format!(
                    "|service:{}:{}",
                    service_id, snapshot.config_version
                ));
                if let Some(binding) = self.store.application_binding(service_id)? {
                    stamp.push_str(&format!(
                        "|serviceIdentity:{}:{}|servicePath:{}",
                        binding.instance.identity.pid,
                        binding.instance.identity.created_at,
                        binding.target_path
                    ));
                }
            }
        }
        Ok(stamp)
    }

    fn check_expected_stamp(&self, id: &str, expected: Option<&str>) -> Result<(), String> {
        let _ = expected;
        self.check_expected_stamp_locked(id, expected)
    }

    fn check_expected_stamp_locked(&self, id: &str, expected: Option<&str>) -> Result<(), String> {
        if id.is_empty() {
            if expected.is_some() {
                return Err("新内容没有可核验的确认快照。".into());
            }
            return Ok(());
        }
        let current = self.confirmation_stamp_locked(id)?;
        if let Some(expected) = expected {
            if expected != current {
                return Err("内容配置或原生应用身份已变化，请刷新后重新确认。".into());
            }
        }
        Ok(())
    }

    fn lock_operation(&self) -> Result<MutexGuard<'_, ()>, String> {
        self.operation
            .lock()
            .map_err(|_| "内容库操作状态异常，无法继续。".into())
    }
}

fn candidate_from_application(
    application: &crate::models::DiscoveredApplication,
) -> ContentApplicationCandidate {
    ContentApplicationCandidate {
        identity: application.instance.identity.clone(),
        path: application.path.clone(),
        name: application.instance.name.clone(),
        window_count: application.window_count,
    }
}

fn application_control_allowed(instance: &crate::models::ExternalInstance) -> bool {
    instance.controllable
        && !instance.shared_host
        && !matches!(instance.category.as_str(), "system" | "unknown")
}

fn desktop_target(service: &ServiceConfig) -> Option<(String, Vec<String>, String)> {
    match &service.launch {
        LaunchConfig::DesktopApp {
            path,
            args,
            start_dir,
        } => Some((path.clone(), args.clone(), start_dir.clone())),
        _ => None,
    }
}

/// 用 `read_dir` 区分“目录不存在”和“目录存在但当前进程无权读取”。`Path::is_dir`
/// 会把两者都压成 false，导致前端给出错误的“已删除”提示。
fn folder_availability(path: &str) -> Result<(), String> {
    match std::fs::read_dir(path) {
        Ok(_) => Ok(()),
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotFound) => {
            Err("目标文件夹不存在或不是目录。".into())
        }
        Err(error) if matches!(error.kind(), std::io::ErrorKind::NotADirectory) => {
            Err("目标文件夹不存在或不是目录。".into())
        }
        Err(error) => Err(format!("无法读取目标文件夹，权限或系统状态未知：{error}")),
    }
}

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_models::ContentKind;
    use crate::discovery::DiscoveryManager;
    use std::collections::BTreeMap;

    #[test]
    fn confirmation_stamp_invalidates_old_edit_and_delete() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let database = directory.path().join("content.sqlite3");
        let store = Arc::new(Store::open(&database)?);
        let applications = ApplicationManager::new(store.clone(), DiscoveryManager::new(), None);
        let manager = ContentManager::new(store, applications);
        let mut item = ContentItem::new(
            "测试目录",
            ContentKind::Folder,
            ContentTarget::Folder {
                path: directory.path().to_string_lossy().into_owned(),
            },
        );
        item = manager.save(item)?;
        let first_stamp = manager.confirmation_stamp(&item.id)?;
        item.notes = "已编辑".into();
        let item = manager.save_checked(item, Some(&first_stamp))?;
        let second_stamp = manager.confirmation_stamp(&item.id)?;
        assert_ne!(first_stamp, second_stamp);
        assert!(manager
            .remove_checked(&item.id, Some(&first_stamp))
            .is_err());
        manager.remove_checked(&item.id, Some(&second_stamp))?;
        assert!(manager.items()?.is_empty());
        Ok(())
    }

    #[test]
    fn service_link_rejects_non_desktop_service() -> Result<(), String> {
        let directory = tempfile::tempdir().map_err(|error| error.to_string())?;
        let database = directory.path().join("content.sqlite3");
        let store = Arc::new(Store::open(&database)?);
        let service = store.save_service(ServiceConfig {
            id: String::new(),
            kind: "command".into(),
            launch: LaunchConfig::Command,
            source: "managed".into(),
            external: None,
            name: "命令服务".into(),
            description: String::new(),
            working_dir: directory.path().to_string_lossy().into_owned(),
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
        })?;
        let applications = ApplicationManager::new(store.clone(), DiscoveryManager::new(), None);
        let manager = ContentManager::new(store, applications);
        let item = ContentItem::new(
            "错误关联",
            ContentKind::Application,
            ContentTarget::ServiceLink {
                service_id: service.id,
            },
        );
        assert!(manager.save(item).is_err());
        Ok(())
    }
}
