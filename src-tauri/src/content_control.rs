//! 内容指令的唯一执行入口。模型和 MCP 只能提出动作，不能提供 shell、临时参数或确认凭据。
//! 确认记录绑定收藏版本及原生实例身份；确认后再次核验，防止 PID 复用或编辑目标造成误操作。
use crate::content::ContentManager;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};
use uuid::Uuid;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PlannedAction {
    pub action: String,
    pub content_id: Option<String>,
    pub query: Option<String>,
    pub changes: Option<Value>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ActionPlan {
    pub summary: String,
    pub actions: Vec<PlannedAction>,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionStep {
    pub action: PlannedAction,
    pub state: String,
    pub content_id: Option<String>,
    pub target_name: Option<String>,
    pub target_label: Option<String>,
    pub stamp: Option<String>,
    pub candidates: Vec<Value>,
    pub result: Option<Value>,
    pub error: Option<String>,
    pub confirmed: bool,
}
#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionTask {
    pub id: String,
    pub request_id: String,
    pub source: String,
    pub input: String,
    pub summary: String,
    pub state: String,
    pub steps: Vec<ActionStep>,
    pub error: Option<String>,
    pub created_at: String,
    #[serde(skip)]
    fingerprint: String,
    #[serde(skip)]
    pub cancelled: Arc<AtomicBool>,
}
pub struct ContentController {
    pub content: Arc<ContentManager>,
    tasks: Mutex<VecDeque<ActionTask>>,
    /// 执行和确认按任务串行；查询只短暂持有 tasks 锁，等待应用正常退出时仍能取消任务。
    executor: Mutex<()>,
    /// 将取消与动作派发建立明确顺序；已派发的原生调用可能无法撤回，但后续步骤不会再启动。
    dispatch_gate: Mutex<()>,
    shutting_down: AtomicBool,
}
impl ContentController {
    pub fn new(content: Arc<ContentManager>) -> Arc<Self> {
        Arc::new(Self {
            content,
            tasks: Mutex::new(VecDeque::new()),
            executor: Mutex::new(()),
            dispatch_gate: Mutex::new(()),
            shutting_down: AtomicBool::new(false),
        })
    }
    pub fn shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
        for task in self.tasks() {
            let _ = self.cancel(&task.id);
        }
    }
    pub fn resume(&self) {
        self.shutting_down.store(false, Ordering::SeqCst);
    }
    pub fn tasks(&self) -> Vec<ActionTask> {
        self.tasks
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .rev()
            .collect()
    }
    pub fn task(&self, id: &str) -> Result<ActionTask, String> {
        self.tasks()
            .into_iter()
            .find(|t| t.id == id)
            .ok_or_else(|| "指令任务已不存在".into())
    }
    fn update(&self, mut task: ActionTask) {
        if task.cancelled.load(Ordering::SeqCst)
            && !matches!(task.state.as_str(), "completed" | "failed")
        {
            task.state = "cancelled".into();
        }
        let mut tasks = self.tasks.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(old) = tasks.iter_mut().find(|t| t.id == task.id) {
            *old = task;
        }
    }
    pub fn begin(
        &self,
        request_id: String,
        source: &str,
        input: String,
    ) -> Result<(ActionTask, bool), String> {
        if self.shutting_down.load(Ordering::SeqCst) {
            return Err("中心正在退出，已停止接收内容操作".into());
        }
        if request_id.is_empty() || request_id.len() > 128 || input.len() > 16000 {
            return Err("请求标识或指令长度无效".into());
        }
        let fingerprint = format!("{source}:{input}");
        let mut tasks = self.tasks.lock().map_err(|_| "指令队列不可用")?;
        if let Some(old) = tasks.iter().find(|t| t.request_id == request_id) {
            if old.fingerprint != fingerprint {
                return Err("请求标识已被其他内容使用".into());
            }
            return Ok((old.clone(), false));
        }
        if source == "agent"
            && tasks
                .iter()
                .filter(|t| t.source == "agent" && t.state == "understanding")
                .count()
                >= 2
        {
            return Err("已有两个 Agent 正在理解指令，请等待或取消后再试".into());
        }
        // 只淘汰结束任务；待用户处理的确认不会被后台 MCP 请求挤走。
        while tasks.len() >= 128 {
            if let Some(i) = tasks
                .iter()
                .position(|t| matches!(t.state.as_str(), "completed" | "failed" | "cancelled"))
            {
                tasks.remove(i);
            } else {
                return Err("待处理指令过多，请先完成或取消已有任务".into());
            }
        }
        let task = ActionTask {
            id: Uuid::new_v4().to_string(),
            request_id,
            source: source.into(),
            input,
            summary: String::new(),
            state: "understanding".into(),
            steps: vec![],
            error: None,
            created_at: chrono::Utc::now().to_rfc3339(),
            fingerprint,
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        tasks.push_back(task.clone());
        Ok((task, true))
    }
    pub fn fail(&self, id: &str, error: String) {
        if let Ok(mut task) = self.task(id) {
            task.state = if task.cancelled.load(Ordering::SeqCst) {
                "cancelled"
            } else {
                "failed"
            }
            .into();
            task.error = Some(error);
            self.update(task);
        }
    }
    pub fn accept_plan(&self, id: &str, value: Value) -> Result<ActionTask, String> {
        let plan: ActionPlan = serde_json::from_value(value)
            .map_err(|e| format!("Agent 输出不是合法操作计划：{e}"))?;
        validate_plan(&plan)?;
        let mut task = self.task(id)?;
        if task.cancelled.load(Ordering::SeqCst) {
            task.state = "cancelled".into();
            self.update(task.clone());
            return Ok(task);
        }
        if task.state != "understanding" {
            return Err("此任务已有计划，不能覆盖".into());
        }
        task.summary = plan.summary;
        task.steps = plan
            .actions
            .into_iter()
            .map(|action| ActionStep {
                action,
                state: "pending".into(),
                content_id: None,
                target_name: None,
                target_label: None,
                stamp: None,
                candidates: vec![],
                result: None,
                error: None,
                confirmed: false,
            })
            .collect();
        task.state = "ready".into();
        self.update(task);
        self.advance(id)
    }
    pub fn submit(
        &self,
        request_id: String,
        source: &str,
        plan: ActionPlan,
    ) -> Result<ActionTask, String> {
        validate_plan(&plan)?;
        let input = serde_json::to_string(&plan).map_err(|e| e.to_string())?;
        let (task, fresh) = self.begin(request_id, source, input)?;
        if fresh {
            self.accept_plan(
                &task.id,
                serde_json::to_value(plan).map_err(|e| e.to_string())?,
            )
        } else {
            Ok(task)
        }
    }
    pub fn cancel(&self, id: &str) -> Result<ActionTask, String> {
        let _gate = self.dispatch_gate.lock().map_err(|_| "取消队列不可用")?;
        let mut task = self.task(id)?;
        task.cancelled.store(true, Ordering::SeqCst);
        if !matches!(task.state.as_str(), "completed" | "failed") {
            task.state = "cancelled".into();
        }
        self.update(task.clone());
        Ok(task)
    }
    /// 只有 Tauri 主窗口命令可以调用此方法。MCP 路由根本不暴露确认方法。
    pub fn confirm(&self, id: &str, index: usize) -> Result<ActionTask, String> {
        {
            let _guard = self.executor.lock().map_err(|_| "操作队列不可用")?;
            let mut task = self.task(id)?;
            if task.state != "awaitingConfirmation" || task.cancelled.load(Ordering::SeqCst) {
                return Err("此任务当前不接受确认".into());
            }
            let step = task.steps.get_mut(index).ok_or("操作序号无效")?;
            if step.state != "awaitingConfirmation" {
                return Err("此操作当前不接受确认".into());
            }
            let content_id = step.content_id.as_deref().ok_or("尚未选择目标")?;
            let current = self.content.confirmation_stamp(content_id)?;
            if step.stamp.as_deref() != Some(&current) {
                step.stamp = Some(current);
                step.confirmed = false;
                // 二次确认必须展示新的具体目标和身份；只更新校验戳会让用户仍对着旧 PID 确认。
                let item = self
                    .content
                    .items()?
                    .into_iter()
                    .find(|item| item.id == content_id)
                    .ok_or("收藏已被移除")?;
                step.target_name = Some(item.name.clone());
                step.result = Some(self.content.execute(content_id, "status")?);
                let value = serde_json::to_value(&item).map_err(|e| e.to_string())?;
                step.target_label = target_label(&value, step.result.as_ref());
                step.error = Some("目标配置或进程实例已变化，请查看当前状态后重新确认。".into());
                self.update(task.clone());
                return Ok(task);
            }
            step.confirmed = true;
            step.state = "pending".into();
            task.state = "ready".into();
            self.update(task);
        }
        self.advance(id)
    }
    pub fn choose(&self, id: &str, index: usize, content_id: &str) -> Result<ActionTask, String> {
        {
            let _guard = self.executor.lock().map_err(|_| "操作队列不可用")?;
            let mut task = self.task(id)?;
            if task.state != "awaitingSelection" || task.cancelled.load(Ordering::SeqCst) {
                return Err("此任务当前不接受目标选择".into());
            }
            let step = task.steps.get_mut(index).ok_or("操作序号无效")?;
            if !step
                .candidates
                .iter()
                .any(|v| v["id"].as_str() == Some(content_id))
            {
                return Err("该目标不在当前候选范围".into());
            }
            step.content_id = Some(content_id.into());
            step.state = "pending".into();
            step.candidates.clear();
            task.state = "ready".into();
            self.update(task);
        }
        self.advance(id)
    }
    /// 保存与任务完成在同一执行锁内完成。外部调用者不能用任意现有 ID 冒充已保存表单。
    pub fn save_form(
        &self,
        id: &str,
        index: usize,
        item: crate::content_models::ContentItem,
    ) -> Result<crate::content_models::ContentItem, String> {
        let saved;
        {
            let _guard = self.executor.lock().map_err(|_| "操作队列不可用")?;
            let _gate = self.dispatch_gate.lock().map_err(|_| "取消队列不可用")?;
            let mut task = self.task(id)?;
            if task.state != "awaitingForm" || task.cancelled.load(Ordering::SeqCst) {
                return Err("此任务没有待填写表单".into());
            }
            let step = task.steps.get_mut(index).ok_or("操作序号无效")?;
            if step.state != "awaitingForm" {
                return Err("此步骤没有待填写表单".into());
            }
            if step.action.action == "edit" && step.content_id.as_deref() != Some(&item.id) {
                return Err("表单与编辑目标不一致".into());
            }
            if step.action.action == "add" && !item.id.is_empty() {
                return Err("新增收藏不能覆盖现有记录".into());
            }
            saved = self.content.save_checked(item, step.stamp.as_deref())?;
            step.content_id = Some(saved.id.clone());
            step.state = "completed".into();
            step.result = Some(json!({"message":"收藏已保存","contentId":saved.id}));
            task.state = "ready".into();
            self.update(task);
        }
        let _ = self.advance(id);
        Ok(saved)
    }
    fn advance(&self, id: &str) -> Result<ActionTask, String> {
        match self.advance_inner(id) {
            Ok(task) => Ok(task),
            Err(error) => {
                self.fail(id, error.clone());
                Err(error)
            }
        }
    }
    fn advance_inner(&self, id: &str) -> Result<ActionTask, String> {
        let _guard = self.executor.lock().map_err(|_| "操作队列不可用")?;
        let mut task = self.task(id)?;
        if !matches!(task.state.as_str(), "ready" | "running") {
            return Ok(task);
        }
        let batch_edit = task
            .steps
            .iter()
            .filter(|s| s.action.action == "edit")
            .count()
            > 1;
        for i in 0..task.steps.len() {
            if task.cancelled.load(Ordering::SeqCst) {
                task.state = "cancelled".into();
                break;
            }
            if task.steps[i].state == "completed" || task.steps[i].state == "failed" {
                continue;
            }
            let step = &mut task.steps[i];
            let action = step.action.action.clone();
            let items = serde_json::to_value(self.content.items()?).map_err(|e| e.to_string())?;
            let items = items.as_array().ok_or("收藏数据格式错误")?;
            if action == "search" {
                step.result = Some(Value::Array(find_items(
                    items,
                    step.action.query.as_deref().unwrap_or(""),
                )));
                step.state = "completed".into();
                self.update(task.clone());
                continue;
            }
            if action == "add" {
                step.state = "awaitingForm".into();
                task.state = "awaitingForm".into();
                self.update(task.clone());
                return Ok(task);
            }
            if step.content_id.is_none() {
                let query = step.action.query.as_deref().unwrap_or("").trim();
                let candidates = if !query.is_empty() {
                    find_items(items, query)
                } else if let Some(id) = &step.action.content_id {
                    items
                        .iter()
                        .filter(|v| v["id"].as_str() == Some(id))
                        .cloned()
                        .collect()
                } else {
                    vec![]
                };
                if candidates.len() != 1 {
                    step.candidates = candidates;
                    step.state = "awaitingSelection".into();
                    step.error = if step.candidates.is_empty() {
                        Some("没有找到明确目标，请取消后使用更准确的名称。".into())
                    } else {
                        None
                    };
                    task.state = "awaitingSelection".into();
                    self.update(task.clone());
                    return Ok(task);
                }
                step.content_id = candidates[0]["id"].as_str().map(str::to_owned);
            }
            let target = step.content_id.clone().ok_or("尚未选择目标")?;
            let current = items
                .iter()
                .find(|v| v["id"].as_str() == Some(&target))
                .ok_or("收藏已被移除")?;
            step.target_name = current["name"].as_str().map(str::to_owned);
            step.target_label = current["target"]["path"]
                .as_str()
                .map(str::to_owned)
                .or_else(|| {
                    current["target"]["appId"]
                        .as_str()
                        .map(|id| format!("Steam 游戏 {id}"))
                })
                .or_else(|| {
                    current["target"]["serviceId"]
                        .as_str()
                        .map(|id| format!("关联桌面应用 {id}"))
                });
            let stamp = self.content.confirmation_stamp(&target)?;
            let needs_confirmation =
                matches!(action.as_str(), "requestQuit" | "forceQuit" | "remove")
                    || (action == "edit" && batch_edit);
            if needs_confirmation && (!step.confirmed || step.stamp.as_deref() != Some(&stamp)) {
                step.confirmed = false;
                step.stamp = Some(stamp);
                step.state = "awaitingConfirmation".into();
                // 展示本次实例及全部已知状态，确认 UI 不只显示一个容易混淆的名字。
                step.result = Some(self.content.execute(&target, "status")?);
                step.target_label = target_label(current, step.result.as_ref());
                task.state = "awaitingConfirmation".into();
                self.update(task.clone());
                return Ok(task);
            }
            step.stamp = Some(stamp.clone());
            // 修改目标和新增始终走预填表单；模型提供的路径只能作为待确认草稿。
            if action == "edit"
                && step
                    .action
                    .changes
                    .as_ref()
                    .and_then(|v| v.get("target"))
                    .is_some()
            {
                step.state = "awaitingForm".into();
                task.state = "awaitingForm".into();
                self.update(task.clone());
                return Ok(task);
            }
            {
                let _gate = self.dispatch_gate.lock().map_err(|_| "取消队列不可用")?;
                if task.cancelled.load(Ordering::SeqCst) {
                    task.state = "cancelled".into();
                    break;
                }
                step.state = "running".into();
                task.state = "running".into();
                self.update(task.clone());
            }
            // 派发点之后的单个系统调用视为已开始；正常退出可能等待五秒，不能为此阻塞取消后续动作。
            let outcome = match action.as_str() {
                "remove" => self
                    .content
                    .remove_checked(&target, Some(&stamp))
                    .map(|_| json!({"message":"收藏已移除，实际文件和运行中的应用保留"})),
                "edit" => {
                    let mut value = current.clone();
                    if let Some(Value::Object(changes)) = &task.steps[i].action.changes {
                        for (key, value_change) in changes {
                            value[key] = value_change.clone();
                        }
                    }
                    serde_json::from_value(value)
                        .map_err(|e| format!("收藏修改内容无效：{e}"))
                        .and_then(|item| self.content.save_checked(item, Some(&stamp)))
                        .and_then(|v| serde_json::to_value(v).map_err(|e| e.to_string()))
                }
                _ => self.content.execute_checked(&target, &action, Some(&stamp)),
            };
            let step = &mut task.steps[i];
            match outcome {
                Ok(result) => {
                    step.result = Some(result);
                    step.state = "completed".into();
                    step.error = None;
                }
                Err(error) => {
                    step.error = Some(error);
                    step.state = "failed".into();
                }
            }
            self.update(task.clone());
        }
        if task.cancelled.load(Ordering::SeqCst) {
            task.state = "cancelled".into();
        } else {
            task.state = if task.steps.iter().any(|s| s.state == "failed") {
                "failed"
            } else {
                "completed"
            }
            .into();
        }
        self.update(task.clone());
        Ok(task)
    }
}
/// 关联旧桌面应用时优先展示状态里的真实目标路径，避免只显示内部记录标识。
fn target_label(item: &Value, status: Option<&Value>) -> Option<String> {
    item["target"]["path"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| {
            status
                .and_then(|s| s["targetUri"].as_str())
                .map(str::to_owned)
        })
        .or_else(|| {
            item["target"]["appId"]
                .as_str()
                .map(|id| format!("Steam 游戏 {id}"))
        })
        .or_else(|| {
            item["target"]["serviceId"]
                .as_str()
                .map(|id| format!("关联桌面应用 {id}"))
        })
}
fn find_items(items: &[Value], query: &str) -> Vec<Value> {
    let query = query.to_lowercase();
    // 同名与别名不使用“第一个”规则；全部候选交给用户明确选择。
    let exact = items
        .iter()
        .filter(|v| {
            v["name"].as_str().unwrap_or("").to_lowercase() == query
                || v["aliases"].as_array().is_some_and(|a| {
                    a.iter()
                        .any(|s| s.as_str().unwrap_or("").to_lowercase() == query)
                })
        })
        .cloned()
        .collect::<Vec<_>>();
    if !exact.is_empty() {
        return exact;
    }
    items
        .iter()
        .filter(|v| {
            query.is_empty()
                || ["name", "notes", "group"].iter().any(|key| {
                    v[*key]
                        .as_str()
                        .unwrap_or("")
                        .to_lowercase()
                        .contains(&query)
                })
                || ["aliases", "tags"].iter().any(|key| {
                    v[*key].as_array().is_some_and(|a| {
                        a.iter()
                            .any(|s| s.as_str().unwrap_or("").to_lowercase().contains(&query))
                    })
                })
        })
        .cloned()
        .collect()
}
pub fn validate_plan(plan: &ActionPlan) -> Result<(), String> {
    if plan.summary.len() > 2000 || plan.actions.is_empty() || plan.actions.len() > 20 {
        return Err("操作计划必须包含 1 至 20 个动作".into());
    }
    for action in &plan.actions {
        if ![
            "search",
            "status",
            "open",
            "start",
            "focus",
            "requestQuit",
            "forceQuit",
            "remove",
            "edit",
            "add",
        ]
        .contains(&action.action.as_str())
        {
            return Err(format!("不支持的内容操作：{}", action.action));
        }
        if action.content_id.as_ref().is_some_and(|v| v.len() > 128)
            || action.query.as_ref().is_some_and(|v| v.len() > 1000)
        {
            return Err("目标信息过长".into());
        }
        if let Some(changes) = &action.changes {
            let object = changes.as_object().ok_or("收藏修改必须是对象")?;
            if !matches!(action.action.as_str(), "add" | "edit") {
                return Err("此动作不接受修改参数".into());
            }
            if object.keys().any(|k| {
                ![
                    "name", "aliases", "group", "tags", "notes", "pinned", "kind", "target",
                ]
                .contains(&k.as_str())
            }) {
                return Err("不能修改内部身份、来源或时间字段".into());
            }
            for (key, value) in object {
                match key.as_str() {
                    "name" | "group" | "notes" => {
                        if !value.is_string() {
                            return Err(format!("{key} 必须是文字"));
                        }
                    }
                    "aliases" | "tags" => {
                        if !value.as_array().is_some_and(|values| {
                            values.len() <= 100
                                && values
                                    .iter()
                                    .all(|v| v.as_str().is_some_and(|s| s.len() <= 512))
                        }) {
                            return Err(format!("{key} 必须是有限长度的文字列表"));
                        }
                    }
                    "pinned" => {
                        if !value.is_boolean() {
                            return Err("置顶字段必须是布尔值".into());
                        }
                    }
                    "kind" => {
                        if !value
                            .as_str()
                            .is_some_and(|v| ["folder", "application", "game"].contains(&v))
                        {
                            return Err("收藏类型无效".into());
                        }
                    }
                    "target" => {
                        serde_json::from_value::<crate::content_models::ContentTarget>(
                            value.clone(),
                        )
                        .map_err(|e| format!("启动目标草稿无效：{e}"))?;
                    }
                    _ => return Err("非法修改字段".into()),
                }
            }
            if serde_json::to_vec(changes)
                .map_err(|e| e.to_string())?
                .len()
                > 32000
            {
                return Err("收藏草稿过大".into());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_executable_actions_and_hidden_fields() {
        for action in ["shell", "stop_service", "confirmed"] {
            assert!(validate_plan(&ActionPlan {
                summary: String::new(),
                actions: vec![PlannedAction {
                    action: action.into(),
                    content_id: None,
                    query: None,
                    changes: None
                }]
            })
            .is_err());
        }
        assert!(serde_json::from_value::<ActionPlan>(
            json!({"summary":"","actions":[],"confirmed":true})
        )
        .is_err());
    }
    #[test]
    fn duplicate_names_remain_ambiguous() {
        let items = vec![
            json!({"id":"1","name":"设计","aliases":[]}),
            json!({"id":"2","name":"设计","aliases":[]}),
        ];
        assert_eq!(find_items(&items, "设计").len(), 2);
    }
}

#[cfg(test)]
mod flow_tests {
    use super::*;
    use crate::{
        applications::ApplicationManager,
        content_models::{ContentItem, ContentKind, ContentTarget},
        discovery::DiscoveryManager,
        store::Store,
    };
    fn fixture() -> (tempfile::TempDir, Arc<ContentController>, ContentItem) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(Store::open(&dir.path().join("test.sqlite3")).unwrap());
        let apps = ApplicationManager::new(store.clone(), DiscoveryManager::new(), None);
        let content = ContentManager::new(store, apps);
        let item = content
            .save(ContentItem::new(
                "设计资料",
                ContentKind::Folder,
                ContentTarget::Folder {
                    path: dir.path().to_string_lossy().into_owned(),
                },
            ))
            .unwrap();
        (dir, ContentController::new(content), item)
    }
    fn action(name: &str, id: Option<String>, query: Option<String>) -> ActionPlan {
        ActionPlan {
            summary: "测试操作".into(),
            actions: vec![PlannedAction {
                action: name.into(),
                content_id: id,
                query,
                changes: None,
            }],
        }
    }
    #[test]
    fn removal_requires_current_confirmation_and_preserves_files() {
        let (dir, controller, item) = fixture();
        let plan = action("remove", Some(item.id.clone()), None);
        let task = controller
            .submit("stable-request".into(), "mcp", plan.clone())
            .unwrap();
        assert_eq!(task.state, "awaitingConfirmation");
        assert_eq!(controller.content.items().unwrap().len(), 1);
        let mut changed = item.clone();
        changed.notes = "确认之后改变的记录".into();
        controller.content.save(changed).unwrap();
        assert_eq!(
            controller.confirm(&task.id, 0).unwrap().state,
            "awaitingConfirmation"
        );
        assert_eq!(controller.confirm(&task.id, 0).unwrap().state, "completed");
        assert!(controller.content.items().unwrap().is_empty());
        assert!(dir.path().is_dir());
        let repeated = controller
            .submit("stable-request".into(), "mcp", plan)
            .unwrap();
        assert_eq!(repeated.id, task.id);
        assert_eq!(repeated.state, "completed");
    }
    #[test]
    fn ambiguity_requires_selection_and_cancel_cannot_execute() {
        let (dir, controller, item) = fixture();
        let second = dir.path().join("第二个 中文目录");
        std::fs::create_dir(&second).unwrap();
        let other = controller
            .content
            .save(ContentItem::new(
                &item.name,
                ContentKind::Folder,
                ContentTarget::Folder {
                    path: second.to_string_lossy().into_owned(),
                },
            ))
            .unwrap();
        let task = controller
            .submit(
                "ambiguous".into(),
                "agent",
                action("status", Some(item.id), Some(item.name)),
            )
            .unwrap();
        assert_eq!(task.state, "awaitingSelection");
        assert_eq!(task.steps[0].candidates.len(), 2);
        assert_eq!(
            controller.choose(&task.id, 0, &other.id).unwrap().state,
            "completed"
        );
        let remove = controller
            .submit(
                "cancel".into(),
                "mcp",
                action("remove", Some(other.id), None),
            )
            .unwrap();
        controller.cancel(&remove.id).unwrap();
        assert!(controller.confirm(&remove.id, 0).is_err());
        assert_eq!(controller.content.items().unwrap().len(), 2);
    }
    #[test]
    fn form_completion_requires_a_real_save_of_the_bound_target() {
        let (dir, controller, item) = fixture();
        let plan = ActionPlan {
            summary: "新增收藏草稿".into(),
            actions: vec![PlannedAction {
                action: "add".into(),
                content_id: None,
                query: None,
                changes: Some(json!({"name":"新收藏"})),
            }],
        };
        let task = controller.submit("form-save".into(), "mcp", plan).unwrap();
        assert_eq!(task.state, "awaitingForm");
        assert!(controller.save_form(&task.id, 0, item).is_err());
        assert_eq!(controller.task(&task.id).unwrap().state, "awaitingForm");
        let path = dir.path().join("表单确认目录");
        std::fs::create_dir(&path).unwrap();
        let saved = controller
            .save_form(
                &task.id,
                0,
                ContentItem::new(
                    "新收藏",
                    ContentKind::Folder,
                    ContentTarget::Folder {
                        path: path.to_string_lossy().into_owned(),
                    },
                ),
            )
            .unwrap();
        assert!(!saved.id.is_empty());
        assert_eq!(controller.task(&task.id).unwrap().state, "completed");
        assert_eq!(controller.content.items().unwrap().len(), 2);
        assert!(controller.save_form(&task.id, 0, saved).is_err());
    }
    #[test]
    fn malformed_form_fields_and_confirmation_parameters_are_rejected() {
        assert!(validate_plan(&ActionPlan {
            summary: String::new(),
            actions: vec![PlannedAction {
                action: "edit".into(),
                content_id: Some("id".into()),
                query: None,
                changes: Some(json!({"tags":9}))
            }]
        })
        .is_err());
        assert!(serde_json::from_value::<ActionPlan>(
            json!({"summary":"","actions":[{"action":"remove","contentId":"a","confirmed":true}]})
        )
        .is_err());
    }
}
