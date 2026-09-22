//! 未实现平台的显式适配层，避免将桌面应用进程误送入命令服务清理路径。

use super::NativeEnumeration;
use crate::models::{ApplicationInspection, ProcessIdentity};
use std::collections::BTreeMap;

pub(super) fn enumerate() -> Result<NativeEnumeration, String> {
    Ok(NativeEnumeration {
        applications: Vec::new(),
        warning: Some("当前平台尚未实现原生桌面应用枚举。".into()),
    })
}

pub(super) fn can_request_quit(_window_count: u32) -> bool {
    false
}

pub(super) fn inspect_identity(
    _identity: &ProcessIdentity,
) -> Result<Option<super::NativeApplication>, String> {
    Err("当前平台尚未实现桌面应用身份核验。".into())
}

pub(super) fn validate_application_path(_path: &str) -> Result<String, String> {
    Err("当前平台尚未支持桌面应用配置。".into())
}

pub(super) fn inspect_path(_path: &str) -> Result<ApplicationInspection, String> {
    Err("当前平台尚未支持桌面应用配置。".into())
}

pub(super) fn application_executable(_path: &str) -> Option<String> {
    None
}

pub(super) fn launch(
    _path: &str,
    _args: &[String],
    _start_dir: &str,
    _environment: &BTreeMap<String, String>,
) -> Result<ProcessIdentity, String> {
    Err("当前平台尚未实现桌面应用启动。".into())
}

pub(super) fn request_quit(_identity: &ProcessIdentity, _path: &str) -> Result<(), String> {
    Err("当前平台尚未实现桌面应用正常退出。".into())
}

pub(super) fn force_quit(_identity: &ProcessIdentity, _path: &str) -> Result<(), String> {
    Err("当前平台尚未实现桌面应用强制结束。".into())
}

pub(super) fn focus(_identity: &ProcessIdentity, _path: &str) -> Result<(), String> {
    Err("当前平台尚未实现桌面应用激活。".into())
}
