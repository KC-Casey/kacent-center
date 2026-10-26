//! 内容库的持久化模型与运行态模型。
//!
//! 内容记录刻意不复用 `ServiceConfig`：内容库只描述用户想要打开的目标，运行状态则由
//! `ContentManager` 每次根据文件系统、Steam URI 或原生应用身份即时计算。这样删除一条
//! 收藏不会删除它所链接的服务配置，也不会制造一个没有真实 PTY 的“假服务”。

use crate::models::ProcessIdentity;
use serde::{Deserialize, Serialize};
use std::path::Path;

/// 内容卡片的展示分类。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ContentKind {
    #[default]
    Folder,
    Application,
    Game,
}

impl ContentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Folder => "folder",
            Self::Application => "application",
            Self::Game => "game",
        }
    }

    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim() {
            "folder" => Ok(Self::Folder),
            "application" | "app" => Ok(Self::Application),
            "game" => Ok(Self::Game),
            other => Err(format!("内容分类无效：{other}")),
        }
    }
}

/// 内容目标的类型化联合体。
///
/// Steam 目标只保存规范化数字 App ID；固定的 `steam://run/<id>` URI 由
/// [`ContentTarget::canonical_uri`] 生成，调用方不能通过任意 URI 把收藏重定向到其他协议。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "type",
    deny_unknown_fields,
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum ContentTarget {
    Folder {
        path: String,
    },
    LocalApp {
        path: String,
        #[serde(default)]
        args: Vec<String>,
        #[serde(default)]
        start_dir: String,
    },
    Steam {
        app_id: String,
    },
    ServiceLink {
        service_id: String,
    },
}

impl ContentTarget {
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::Folder { .. } => "folder",
            Self::LocalApp { .. } => "localApp",
            Self::Steam { .. } => "steam",
            Self::ServiceLink { .. } => "serviceLink",
        }
    }

    /// 返回 Steam 的固定 URI；这里只允许 ASCII 数字 App ID，避免 URI 注入或歧义。
    pub fn canonical_uri(&self) -> Option<String> {
        match self {
            Self::Steam { app_id } => Self::steam_app_id(app_id)
                .ok()
                .map(|app_id| format!("steam://run/{app_id}")),
            _ => None,
        }
    }

    /// 规范化可由前端输入的 Steam 形式。仅接受纯数字 App ID、`steam://run/<id>` 和
    /// `steam://rungameid/<id>`，带 query、fragment、额外路径或其他协议的值一律拒绝。
    pub fn normalized(&self) -> Result<Self, String> {
        match self {
            Self::Steam { app_id } => Ok(Self::Steam {
                app_id: Self::steam_app_id(app_id)?,
            }),
            other => Ok(other.clone()),
        }
    }

    fn steam_app_id(value: &str) -> Result<String, String> {
        let value = value.trim();
        let id = if value.bytes().all(|byte| byte.is_ascii_digit()) && !value.is_empty() {
            value
        } else if let Some(id) = value.strip_prefix("steam://run/") {
            id
        } else if let Some(id) = value.strip_prefix("steam://rungameid/") {
            id
        } else {
            return Err(
                "Steam App ID 只能是数字、steam://run/<id> 或 steam://rungameid/<id>。".into(),
            );
        };
        if id.is_empty() || !id.bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Steam App ID 必须是数字，且不能包含 query、fragment 或额外路径。".into());
        }
        Ok(id.to_owned())
    }

    /// 返回用于确认快照的稳定目标串。目标字段变化会使旧确认自动失效。
    pub fn stable_key(&self) -> String {
        match self {
            Self::Folder { path } => format!("folder:{}", path),
            Self::LocalApp {
                path,
                args,
                start_dir,
            } => format!("localApp:{}:{:?}:{}", path, args, start_dir),
            Self::Steam { app_id } => format!("steam:{}", app_id),
            Self::ServiceLink { service_id } => format!("serviceLink:{}", service_id),
        }
    }
}

/// 内容库的一条独立记录。`target` 改变时由后端检查运行态并拒绝危险的热切换。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentItem {
    pub id: String,
    #[serde(alias = "title")]
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub group: String,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub notes: String,
    #[serde(default)]
    pub pinned: bool,
    #[serde(default)]
    pub last_used_at: Option<String>,
    pub kind: ContentKind,
    pub target: ContentTarget,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
    /// 单调递增的配置版本；确认快照必须包含该版本。
    #[serde(default)]
    pub revision: u64,
}

impl ContentItem {
    pub fn new(title: impl Into<String>, kind: ContentKind, target: ContentTarget) -> Self {
        Self {
            id: String::new(),
            name: title.into(),
            aliases: Vec::new(),
            group: String::new(),
            tags: Vec::new(),
            description: String::new(),
            notes: String::new(),
            pinned: false,
            last_used_at: None,
            kind,
            target,
            created_at: String::new(),
            updated_at: String::new(),
            revision: 0,
        }
    }
}

/// 校验可持久化的内容配置。路径存在性留给运行态计算，以便收藏可以保留并提示失效目标。
pub fn validate_content_item(item: &ContentItem) -> Result<(), String> {
    if item.name.trim().is_empty() {
        return Err("内容名称不能为空。".into());
    }
    if item.name.len() > 512 {
        return Err("内容名称过长。".into());
    }
    for (value, label) in [
        (&item.name, "内容名称"),
        (&item.group, "内容分组"),
        (&item.description, "内容描述"),
        (&item.notes, "内容备注"),
    ] {
        if value.contains('\0') {
            return Err(format!("{label}不能包含 NUL 字符。"));
        }
    }
    for alias in &item.aliases {
        if alias.contains('\0') {
            return Err("内容别名不能包含 NUL 字符。".into());
        }
    }
    for tag in &item.tags {
        if tag.contains('\0') || tag.trim().is_empty() {
            return Err("内容标签不能为空且不能包含 NUL 字符。".into());
        }
    }

    match &item.target {
        ContentTarget::Folder { path } => validate_absolute_path(path, "文件夹路径")?,
        ContentTarget::LocalApp {
            path,
            args,
            start_dir,
        } => {
            validate_absolute_path(path, "本地应用路径")?;
            if !start_dir.trim().is_empty() {
                validate_absolute_path(start_dir, "应用启动目录")?;
            }
            for arg in args {
                if arg.contains('\0') {
                    return Err("应用参数不能包含 NUL 字符。".into());
                }
            }
        }
        ContentTarget::Steam { app_id } => {
            ContentTarget::steam_app_id(app_id)?;
        }
        ContentTarget::ServiceLink { service_id } => {
            if service_id.trim().is_empty() || service_id.contains('\0') {
                return Err("服务关联缺少有效的服务 ID。".into());
            }
        }
    }

    let kind_matches = match (&item.kind, &item.target) {
        (ContentKind::Folder, ContentTarget::Folder { .. }) => true,
        (ContentKind::Application, ContentTarget::LocalApp { .. })
        | (ContentKind::Application, ContentTarget::ServiceLink { .. }) => true,
        (ContentKind::Game, ContentTarget::LocalApp { .. }) => true,
        (ContentKind::Game, ContentTarget::Steam { .. })
        | (ContentKind::Game, ContentTarget::ServiceLink { .. }) => true,
        _ => false,
    };
    if !kind_matches {
        return Err(format!(
            "内容分类 {} 与目标类型 {} 不匹配。",
            item.kind.as_str(),
            item.target.type_name()
        ));
    }
    Ok(())
}

fn validate_absolute_path(value: &str, label: &str) -> Result<(), String> {
    let path = Path::new(value.trim());
    if value.trim().is_empty() || !path.is_absolute() {
        return Err(format!("{label}必须是绝对路径。"));
    }
    if value.contains('\0') {
        return Err(format!("{label}不能包含 NUL 字符。"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_only_fixed_steam_uri_forms() {
        for value in ["123", "steam://run/123", "steam://rungameid/123"] {
            let target = ContentTarget::Steam {
                app_id: value.into(),
            }
            .normalized()
            .expect("固定 Steam 形式应可规范化");
            assert_eq!(
                target,
                ContentTarget::Steam {
                    app_id: "123".into()
                }
            );
        }
        for value in [
            "steam://run/123?args=x",
            "steam://run/123#fragment",
            "steam://run/123/extra",
            "https://example.test/123",
            "steam://open/123",
        ] {
            assert!(ContentTarget::Steam {
                app_id: value.into(),
            }
            .normalized()
            .is_err());
        }
    }

    #[test]
    fn game_may_point_to_local_app() {
        let item = ContentItem::new(
            "游戏启动器",
            ContentKind::Game,
            ContentTarget::LocalApp {
                path: "/Applications/Game.app".into(),
                args: vec![],
                start_dir: String::new(),
            },
        );
        assert!(validate_content_item(&item).is_ok());
    }
}

/// 内容的原生应用关联。它只存身份快照和目标路径，不包含任何可持久化进程句柄。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentApplicationBinding {
    pub identity: ProcessIdentity,
    pub target_path: String,
    pub bound_at: String,
}

/// 多实例情况下返回给前端的可选原生应用实例。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentApplicationCandidate {
    pub identity: ProcessIdentity,
    pub path: String,
    pub name: String,
    pub window_count: u32,
}

/// 内容运行态；不写入 `content_items`，每次由管理器根据当前系统状态生成。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentStatus {
    pub content_id: String,
    /// `available`、`unavailable`、`running`、`stopped`、`unknown` 或
    /// `selectionRequired`。前端应把未知状态视为不可控制。
    pub state: String,
    pub available: bool,
    pub running: bool,
    pub identity: Option<ProcessIdentity>,
    pub pid: Option<u32>,
    pub target_uri: Option<String>,
    pub message: Option<String>,
    pub candidates: Vec<ContentApplicationCandidate>,
    pub can_open: bool,
    pub can_start: bool,
    pub can_focus: bool,
    pub can_request_quit: bool,
    pub can_force_quit: bool,
    /// 用于高风险确认的后端快照；它绑定配置版本、目标和完整创建身份。
    pub confirmation_stamp: String,
}
