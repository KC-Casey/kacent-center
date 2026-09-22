//! 项目识别只读取配置，不安装依赖、不执行 package.json 中的任何脚本。
use crate::models::FrontendProject;
use serde_json::Value;
use std::{collections::BTreeMap, path::Path};

pub fn inspect(path: &str) -> Result<FrontendProject, String> {
    let root = Path::new(path);
    if !root.is_absolute() || !root.is_dir() {
        return Err("请选择现有项目目录的绝对路径。".into());
    }
    let manifest = root.join("package.json");
    let metadata =
        std::fs::metadata(&manifest).map_err(|e| format!("无法读取 package.json：{e}"))?;
    if metadata.len() > 2 * 1024 * 1024 {
        return Err("package.json 超过 2 MiB，请检查所选文件。".into());
    }
    let bytes = std::fs::read(&manifest).map_err(|e| format!("无法读取 package.json：{e}"))?;
    let json: Value =
        serde_json::from_slice(&bytes).map_err(|e| format!("package.json 不是有效 JSON：{e}"))?;
    let scripts = json
        .get("scripts")
        .and_then(Value::as_object)
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_owned()))
                })
                .collect::<BTreeMap<_, _>>()
        })
        .unwrap_or_default();
    let mut warnings = Vec::new();
    let declared = json
        .get("packageManager")
        .and_then(Value::as_str)
        .map(|value| value.split('@').next().unwrap_or_default());
    let locks = [
        ("npm", &["package-lock.json", "npm-shrinkwrap.json"][..]),
        ("pnpm", &["pnpm-lock.yaml"][..]),
        ("yarn", &["yarn.lock"][..]),
        ("bun", &["bun.lock", "bun.lockb"][..]),
    ];
    let detected = locks
        .iter()
        .filter(|(_, files)| files.iter().any(|file| root.join(file).is_file()))
        .map(|(name, _)| *name)
        .collect::<Vec<_>>();
    let package_manager = match declared {
        Some(name) if matches!(name, "npm" | "pnpm" | "yarn" | "bun") => {
            if detected.iter().any(|detected| *detected != name) {
                warnings.push(
                    "锁文件与 packageManager 声明不一致，已优先采用声明，请确认项目配置。".into(),
                );
            }
            Some(name.to_owned())
        }
        Some(_) => {
            warnings.push("packageManager 声明暂不支持，请手动选择已安装的包管理器。".into());
            None
        }
        None if detected.len() == 1 => Some(detected[0].to_owned()),
        _ => {
            warnings.push("未找到唯一的包管理器依据，请手动选择。".into());
            None
        }
    };
    if scripts.is_empty() {
        warnings.push("项目未声明可运行脚本。".into());
    }
    let suggested_script = ["dev", "start"]
        .into_iter()
        .find(|key| scripts.contains_key(*key))
        .map(str::to_owned);
    let name = json
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(FrontendProject {
        name,
        scripts,
        package_manager,
        suggested_script,
        warnings,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn project_detection_does_not_execute_scripts_and_resolves_conflicting_locks() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(
            root.path().join("package.json"),
            r#"{"packageManager":"pnpm@11","scripts":{"dev":"不能执行的命令"}}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("package-lock.json"), "{}").unwrap();
        let project = super::inspect(root.path().to_str().unwrap()).unwrap();
        assert_eq!(project.package_manager.as_deref(), Some("pnpm"));
        assert_eq!(project.suggested_script.as_deref(), Some("dev"));
        assert!(!project.warnings.is_empty());
        std::fs::write(
            root.path().join("package.json"),
            r#"{"scripts":{"start":"node server.js"}}"#,
        )
        .unwrap();
        std::fs::write(root.path().join("yarn.lock"), "").unwrap();
        assert!(super::inspect(root.path().to_str().unwrap())
            .unwrap()
            .package_manager
            .is_none());
    }
}
