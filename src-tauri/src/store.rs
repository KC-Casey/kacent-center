//! SQLite 持久化层。
//!
//! 配置数据和运行时状态分开保存：本模块只负责服务配置、Agent 启动配置、主题偏好与
//! Markdown 文档。外部服务记录只保存用户明确接入的进程 PID 与创建身份，不持久化 PTY
//! 会话、受管进程句柄或其他运行时状态。所有数据库访问共用一条受互斥锁保护的连接，
//! 避免 Tauri 命令并发调用时同时操作同一连接。

use crate::content_models::{
    validate_content_item, ContentApplicationBinding, ContentItem, ContentKind, ContentTarget,
};
use crate::models::{
    AgentProfile, ApplicationBinding, ExternalInstance, LaunchConfig, Preferences, ServiceConfig,
    ServiceDocument, TerminalAppearance,
};
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, Transaction, TransactionBehavior};
use serde::{de::DeserializeOwned, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use uuid::Uuid;

const SCHEMA_VERSION: i64 = 5;

/// 应用进程中的本地持久化服务。
///
/// `Connection` 本身不支持并发共享，所以集中放进 `Mutex`，由各个 `&self` 方法串行访问。
/// 数据库路径由应用主入口根据 Tauri AppData 目录计算并传入。
pub struct Store {
    connection: Mutex<Connection>,
}

impl Store {
    /// 打开给定路径的 SQLite 数据库，并在事务中完成所需的版本化迁移。
    ///
    /// 首次迁移只建立空的服务列表；内置 Agent 是未执行的启动配置，不会触发安装、登录
    /// 或进程启动。后续打开数据库不会重新插入已被用户删除的 Agent。
    pub fn open(path: &Path) -> Result<Self, String> {
        if !path.is_absolute() {
            return Err(format!("数据库路径必须使用绝对路径：{}", path.display()));
        }
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent)
                .map_err(|error| format!("无法创建数据库目录 {}：{}", parent.display(), error))?;
        }

        let mut connection = Connection::open(path)
            .map_err(|error| format!("无法打开本地数据库 {}：{}", path.display(), error))?;

        // SQLite 默认关闭外键约束；每条新连接都必须显式开启，才能保证删除服务时文档
        // 由 ON DELETE CASCADE 一并清理。
        connection
            .pragma_update(None, "foreign_keys", true)
            .map_err(|error| format!("无法启用数据库外键约束：{}", error))?;

        migrate(&mut connection)?;

        Ok(Self {
            connection: Mutex::new(connection),
        })
    }

    /// 按创建时间列出服务配置，不读取或校验目录是否仍然存在。
    pub fn services(&self) -> Result<Vec<ServiceConfig>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, name, description, working_dir, environment_type, environment_dir, \
                 executable_dirs_json, env_json, command, shell, expected_ports_json, notes, \
                 created_at, updated_at, source, external_json, kind, launch_json \
                 FROM services ORDER BY created_at, id",
            )
            .map_err(|error| database_error("无法读取服务配置", error))?;

        let rows = statement
            .query_map([], stored_service_from_row)
            .map_err(|error| database_error("无法读取服务配置", error))?;
        let stored = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| database_error("无法读取服务配置", error))?;

        stored.into_iter().map(StoredService::into_model).collect()
    }

    /// 读取单个服务配置。即使服务目录后来被删除，读取仍会返回已保存的数据。
    pub fn service(&self, id: &str) -> Result<ServiceConfig, String> {
        let connection = self.connection()?;
        fetch_service(&connection, id)?.ok_or_else(|| format!("找不到服务配置：{}", id))
    }

    /// 校验并保存服务配置。
    ///
    /// 新记录的 ID 由后端生成 UUID；更新时按已有 ID 定位记录，并保留数据库中的
    /// `created_at`。`updated_at` 每次保存都由后端刷新。此处不涉及运行中的会话。
    pub fn save_service(&self, mut service: ServiceConfig) -> Result<ServiceConfig, String> {
        validate_service(&service)?;
        service.expected_ports = deduplicate_ports(&service.expected_ports)?;

        let executable_dirs_json = encode_json(&service.executable_dirs, "可执行文件目录")?;
        let env_json = encode_json(&service.env, "环境变量")?;
        let expected_ports_json = encode_json(&service.expected_ports, "预期端口")?;
        let launch_json = encode_json(&service.launch, "启动配置")?;
        let external_json = service
            .external
            .as_ref()
            .map(|external| encode_json(external, "外部实例"))
            .transpose()?;

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error("无法开始保存服务配置", error))?;

        let requested_id = service.id.trim();
        let is_new = requested_id.is_empty();
        let existing_created_at = if is_new {
            None
        } else {
            transaction
                .query_row(
                    "SELECT created_at FROM services WHERE id = ?1",
                    [requested_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| database_error("无法检查服务配置", error))?
        };
        if !is_new && existing_created_at.is_none() {
            return Err(format!("服务配置不存在，无法更新：{}", requested_id));
        }

        let id = if is_new {
            Uuid::new_v4().to_string()
        } else {
            requested_id.to_owned()
        };
        if service.source == "external" && service.kind != "desktopApp" {
            let external = service
                .external
                .as_ref()
                .ok_or_else(|| "外部实例缺少进程身份信息。".to_owned())?;
            ensure_unique_external_identity(&transaction, external, &id)?;
        }
        let created_at = existing_created_at.unwrap_or_else(timestamp_now);
        let updated_at = timestamp_now();

        if is_new {
            // 新建路径下使用 INSERT，确保极低概率的 UUID 冲突也会作为错误返回，
            // 不会覆盖其他记录。
            transaction
                .execute(
                    "INSERT INTO services (
                        id, name, description, working_dir, environment_type, environment_dir,
                        executable_dirs_json, env_json, command, shell, expected_ports_json,
                        notes, created_at, updated_at, source, external_json, kind, launch_json
                    ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)",
                    params![
                        id,
                        service.name,
                        service.description,
                        service.working_dir,
                        service.environment_type,
                        service.environment_dir,
                        executable_dirs_json,
                        env_json,
                        service.command,
                        service.shell,
                        expected_ports_json,
                        service.notes,
                        created_at,
                        updated_at,
                        service.source,
                        external_json,
                        service.kind,
                        launch_json,
                    ],
                )
                .map_err(|error| database_error("无法保存服务配置", error))?;
        } else {
            transaction
                .execute(
                    "UPDATE services SET
                        name = ?2, description = ?3, working_dir = ?4,
                        environment_type = ?5, environment_dir = ?6,
                        executable_dirs_json = ?7, env_json = ?8, command = ?9,
                        shell = ?10, expected_ports_json = ?11, notes = ?12,
                        updated_at = ?13, source = ?14, external_json = ?15, kind = ?16, launch_json = ?17
                     WHERE id = ?1",
                    params![
                        id,
                        service.name,
                        service.description,
                        service.working_dir,
                        service.environment_type,
                        service.environment_dir,
                        executable_dirs_json,
                        env_json,
                        service.command,
                        service.shell,
                        expected_ports_json,
                        service.notes,
                        updated_at,
                        service.source,
                        external_json,
                        service.kind,
                        launch_json,
                    ],
                )
                .map_err(|error| database_error("无法更新服务配置", error))?;
        }

        transaction
            .commit()
            .map_err(|error| database_error("无法完成服务配置保存", error))?;

        service.id = id;
        service.created_at = created_at;
        service.updated_at = updated_at;
        Ok(service)
    }

    /// 只删除数据库中的服务记录。
    ///
    /// managed 服务的调用方必须先停止由平台启动的运行会话。删除 external 记录只解除
    /// 平台对当前进程的绑定，不停止外部进程。数据库外键会级联删除该服务的 Markdown 文档。
    pub fn delete_service(&self, id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM services WHERE id = ?1", [id])
            .map_err(|error| database_error("无法删除服务配置", error))?;
        Ok(())
    }

    /// 桌面应用的实例关联独立于启动配方，重开时只能恢复仍匹配的原实例。
    pub fn application_binding(&self, id: &str) -> Result<Option<ApplicationBinding>, String> {
        let connection = self.connection()?;
        let json: Option<String> = connection
            .query_row(
                "SELECT binding_json FROM application_bindings WHERE service_id = ?1",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| database_error("无法读取应用关联", e))?;
        json.map(|value| decode_json(&value, "应用关联"))
            .transpose()
    }
    pub fn application_bindings(&self) -> Result<Vec<(String, ApplicationBinding)>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare("SELECT service_id, binding_json FROM application_bindings")
            .map_err(|e| database_error("无法读取应用关联", e))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|e| database_error("无法读取应用关联", e))?;
        rows.map(|row| {
            let (id, json) = row.map_err(|e| database_error("无法读取应用关联", e))?;
            Ok((id, decode_json(&json, "应用关联")?))
        })
        .collect()
    }
    /// 在同一写事务内检查应用与旧端口接入记录，避免两个入口重复认领同一实例。
    pub fn bind_application(&self, id: &str, binding: &ApplicationBinding) -> Result<(), String> {
        validate_external_instance(&binding.instance)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|e| database_error("无法开始应用关联", e))?;
        let service = fetch_service(&transaction, id)?.ok_or("应用记录不存在")?;
        if service.kind != "desktopApp" {
            return Err("只能为桌面应用保存应用关联".into());
        }
        ensure_unique_external_identity(&transaction, &binding.instance, id)?;
        transaction.execute("INSERT INTO application_bindings (service_id, binding_json) VALUES (?1, ?2) ON CONFLICT(service_id) DO UPDATE SET binding_json = excluded.binding_json", params![id, encode_json(binding, "应用关联")?])
            .map_err(|e| database_error("无法保存应用关联", e))?;
        transaction
            .commit()
            .map_err(|e| database_error("无法完成应用关联", e))
    }
    pub fn clear_application_binding(&self, id: &str) -> Result<(), String> {
        self.connection()?
            .execute(
                "DELETE FROM application_bindings WHERE service_id = ?1",
                [id],
            )
            .map_err(|e| database_error("无法解除应用关联", e))?;
        Ok(())
    }

    // ---------------------------------------------------------------------
    // 内容库
    // ---------------------------------------------------------------------

    /// 列出独立内容配置。这里不计算文件夹或应用运行态；状态由 `ContentManager` 即时生成。
    pub fn content_items(&self) -> Result<Vec<ContentItem>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT config_json FROM content_items
                 ORDER BY pinned DESC, updated_at DESC, id",
            )
            .map_err(|error| database_error("无法读取内容配置", error))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|error| database_error("无法读取内容配置", error))?;
        rows.map(|row| {
            let json = row.map_err(|error| database_error("无法读取内容配置", error))?;
            decode_json(&json, "内容配置")
        })
        .collect()
    }

    /// 兼容内容库调用方使用的简短别名。
    pub fn contents(&self) -> Result<Vec<ContentItem>, String> {
        self.content_items()
    }

    /// 读取单条内容配置。
    pub fn content_item(&self, id: &str) -> Result<ContentItem, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT config_json FROM content_items WHERE id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| database_error("无法读取内容配置", error))?
            .map(|json| decode_json(&json, "内容配置"))
            .transpose()?
            .ok_or_else(|| format!("找不到内容：{id}"))
    }

    /// 保存内容配置。该方法只写内容表，不会创建或更新服务、终端、PTY 或文档。
    pub fn save_content_item(&self, mut item: ContentItem) -> Result<ContentItem, String> {
        item.target = item.target.normalized()?;
        validate_content_item(&item)?;
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error("无法开始保存内容配置", error))?;

        let requested_id = item.id.trim();
        let is_new = requested_id.is_empty();
        let existing = if is_new {
            None
        } else {
            transaction
                .query_row(
                    "SELECT created_at, revision FROM content_items WHERE id = ?1",
                    [requested_id],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
                )
                .optional()
                .map_err(|error| database_error("无法检查内容配置", error))?
        };
        if !is_new && existing.is_none() {
            return Err(format!("内容不存在，无法更新：{requested_id}"));
        }
        let id = if is_new {
            Uuid::new_v4().to_string()
        } else {
            requested_id.to_owned()
        };
        let created_at = existing
            .as_ref()
            .map(|(created_at, _)| created_at.clone())
            .unwrap_or_else(timestamp_now);
        let old_revision = existing
            .as_ref()
            .map(|(_, revision)| *revision)
            .unwrap_or(0);
        let revision = old_revision.saturating_add(1).max(1);
        let updated_at = timestamp_now();
        item.id = id.clone();
        item.created_at = created_at.clone();
        item.updated_at = updated_at.clone();
        item.revision = revision as u64;
        let config_json = encode_json(&item, "内容配置")?;

        if is_new {
            transaction
                .execute(
                    "INSERT INTO content_items
                     (id, kind, pinned, config_json, created_at, updated_at, revision)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        id,
                        item.kind.as_str(),
                        if item.pinned { 1 } else { 0 },
                        config_json,
                        created_at,
                        updated_at,
                        revision,
                    ],
                )
                .map_err(|error| database_error("无法保存内容配置", error))?;
        } else {
            transaction
                .execute(
                    "UPDATE content_items SET kind = ?2, pinned = ?3, config_json = ?4,
                     updated_at = ?5, revision = ?6 WHERE id = ?1",
                    params![
                        id,
                        item.kind.as_str(),
                        if item.pinned { 1 } else { 0 },
                        config_json,
                        updated_at,
                        revision,
                    ],
                )
                .map_err(|error| database_error("无法更新内容配置", error))?;
        }
        transaction
            .commit()
            .map_err(|error| database_error("无法完成内容配置保存", error))?;
        Ok(item)
    }

    /// 只更新内容最近使用时间，避免动作层为了写状态而改动目标配置版本。
    pub fn touch_content(&self, id: &str) -> Result<(), String> {
        let now = timestamp_now();
        let mut item = self.content_item(id)?;
        item.last_used_at = Some(now);
        // `save_content_item` 会递增 revision，而最近使用时间不应让确认快照失效；因此
        // 这里直接更新完整 JSON 并保留原有 revision。
        let json = encode_json(&item, "内容配置")?;
        let connection = self.connection()?;
        let changed = connection
            .execute(
                "UPDATE content_items SET config_json = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, json, item.updated_at],
            )
            .map_err(|error| database_error("无法更新内容使用时间", error))?;
        if changed == 0 {
            return Err(format!("找不到内容：{id}"));
        }
        Ok(())
    }

    /// 删除内容记录及其身份关联；由于内容表不持有服务外键，原服务和文档保持不变。
    pub fn delete_content_item(&self, id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM content_items WHERE id = ?1", [id])
            .map_err(|error| database_error("无法删除内容配置", error))?;
        Ok(())
    }

    pub fn content_application_binding(
        &self,
        id: &str,
    ) -> Result<Option<ContentApplicationBinding>, String> {
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT binding_json FROM content_application_bindings WHERE content_id = ?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| database_error("无法读取内容应用关联", error))?;
        json.map(|json| decode_json(&json, "内容应用关联"))
            .transpose()
    }

    pub fn content_application_bindings(
        &self,
    ) -> Result<Vec<(String, ContentApplicationBinding)>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT content_id, binding_json FROM content_application_bindings
                 ORDER BY content_id",
            )
            .map_err(|error| database_error("无法读取内容应用关联", error))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| database_error("无法读取内容应用关联", error))?;
        rows.map(|row| {
            let (id, json) = row.map_err(|error| database_error("无法读取内容应用关联", error))?;
            Ok((id, decode_json(&json, "内容应用关联")?))
        })
        .collect()
    }

    /// 以完整 PID 创建身份为唯一键保存内容关联，并检查服务表及其他内容是否已接管该实例。
    pub fn bind_content_application(
        &self,
        id: &str,
        binding: &ContentApplicationBinding,
    ) -> Result<(), String> {
        if binding.identity.created_at.trim().is_empty() {
            return Err("应用关联缺少进程创建身份。".into());
        }
        if binding.target_path.trim().is_empty() {
            return Err("应用关联缺少目标路径。".into());
        }
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error("无法开始保存内容应用关联", error))?;
        transaction
            .query_row(
                "SELECT 1 FROM content_items WHERE id = ?1",
                [id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| database_error("无法检查内容配置", error))?
            .ok_or_else(|| format!("找不到内容：{id}"))?;
        ensure_unique_content_identity(&transaction, &binding.identity, id)?;
        let json = encode_json(binding, "内容应用关联")?;
        transaction
            .execute(
                "INSERT INTO content_application_bindings (content_id, binding_json)
                 VALUES (?1, ?2)
                 ON CONFLICT(content_id) DO UPDATE SET binding_json = excluded.binding_json",
                params![id, json],
            )
            .map_err(|error| database_error("无法保存内容应用关联", error))?;
        transaction
            .commit()
            .map_err(|error| database_error("无法完成内容应用关联保存", error))
    }

    pub fn clear_content_application_binding(&self, id: &str) -> Result<(), String> {
        self.connection()?
            .execute(
                "DELETE FROM content_application_bindings WHERE content_id = ?1",
                [id],
            )
            .map_err(|error| database_error("无法解除内容应用关联", error))?;
        Ok(())
    }

    /// 内容库偏好使用独立单例表，由上层负责校验 Value 的字段契约。
    pub fn content_preferences(&self) -> Result<Value, String> {
        let connection = self.connection()?;
        let json = connection
            .query_row(
                "SELECT preferences_json FROM content_preferences WHERE singleton_id = 1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(|error| database_error("无法读取内容库偏好", error))?;
        match json {
            Some(json) => decode_json(&json, "内容库偏好"),
            None => Ok(Value::Object(serde_json::Map::new())),
        }
    }

    pub fn save_content_preferences(&self, preferences: Value) -> Result<Value, String> {
        let json = encode_json(&preferences, "内容库偏好")?;
        let connection = self.connection()?;
        connection
            .execute(
                "INSERT INTO content_preferences (singleton_id, preferences_json)
                 VALUES (1, ?1)
                 ON CONFLICT(singleton_id) DO UPDATE SET preferences_json = excluded.preferences_json",
                [json],
            )
            .map_err(|error| database_error("无法保存内容库偏好", error))?;
        Ok(preferences)
    }

    /// 列出所有 Agent 启动配置；本方法只读本地设置，不检查或执行 CLI。
    pub fn agents(&self) -> Result<Vec<AgentProfile>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, name, executable, args_json, env_json FROM agents ORDER BY name, id",
            )
            .map_err(|error| database_error("无法读取 Agent 配置", error))?;
        let rows = statement
            .query_map([], stored_agent_from_row)
            .map_err(|error| database_error("无法读取 Agent 配置", error))?;
        let stored = rows
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| database_error("无法读取 Agent 配置", error))?;

        stored.into_iter().map(StoredAgent::into_model).collect()
    }

    /// 读取单个 Agent 启动配置，不运行该配置中的可执行文件。
    pub fn agent(&self, id: &str) -> Result<AgentProfile, String> {
        let connection = self.connection()?;
        fetch_agent(&connection, id)?.ok_or_else(|| format!("找不到 Agent 配置：{}", id))
    }

    /// 保存 Agent 启动配置。
    ///
    /// 新记录使用后端 UUID；已有记录更新时继续使用其 ID。这里只保存 executable、参数和
    /// 环境变量，不安装、不登录，也不启动相应工具。
    pub fn save_agent(&self, mut agent: AgentProfile) -> Result<AgentProfile, String> {
        validate_agent(&agent)?;
        let args_json = encode_json(&agent.args, "Agent 启动参数")?;
        let env_json = encode_json(&agent.env, "Agent 环境变量")?;

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error("无法开始保存 Agent 配置", error))?;

        let requested_id = agent.id.trim();
        let is_new = requested_id.is_empty();
        let exists = if is_new {
            false
        } else {
            transaction
                .query_row("SELECT 1 FROM agents WHERE id = ?1", [requested_id], |_| {
                    Ok(())
                })
                .optional()
                .map_err(|error| database_error("无法检查 Agent 配置", error))?
                .is_some()
        };
        if !is_new && !exists {
            return Err(format!("Agent 配置不存在，无法更新：{}", requested_id));
        }
        let id = if is_new {
            Uuid::new_v4().to_string()
        } else {
            requested_id.to_owned()
        };

        if exists {
            transaction
                .execute(
                    "UPDATE agents SET name = ?2, executable = ?3, args_json = ?4, env_json = ?5
                     WHERE id = ?1",
                    params![id, agent.name, agent.executable, args_json, env_json],
                )
                .map_err(|error| database_error("无法更新 Agent 配置", error))?;
        } else {
            transaction
                .execute(
                    "INSERT INTO agents (id, name, executable, args_json, env_json)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![id, agent.name, agent.executable, args_json, env_json],
                )
                .map_err(|error| database_error("无法保存 Agent 配置", error))?;
        }

        transaction
            .commit()
            .map_err(|error| database_error("无法完成 Agent 配置保存", error))?;

        agent.id = id;
        Ok(agent)
    }

    /// 删除 Agent 配置；删除只作用于本地设置，不会影响其他已启动的进程。
    pub fn delete_agent(&self, id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM agents WHERE id = ?1", [id])
            .map_err(|error| database_error("无法删除 Agent 配置", error))?;
        Ok(())
    }

    /// 读取主题与终端外观偏好；尚未保存偏好或旧数据库缺少终端 JSON 时返回默认值。
    pub fn preferences(&self) -> Result<Preferences, String> {
        let connection = self.connection()?;
        let stored = connection
            .query_row(
                "SELECT theme, terminal_json FROM preferences WHERE singleton_id = 1",
                [],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()
            .map_err(|error| database_error("无法读取主题偏好", error))?;

        let defaults = Preferences::default();
        let (theme, terminal) = match stored {
            Some((theme, terminal_json)) => {
                let terminal = terminal_json
                    .map(|json| decode_json(&json, "终端外观偏好"))
                    .transpose()?
                    .unwrap_or_else(|| defaults.terminal.clone());
                (theme, terminal)
            }
            None => (defaults.theme.clone(), defaults.terminal.clone()),
        };

        Ok(Preferences { theme, terminal })
    }

    /// 保存受前端契约约束的主题及终端外观偏好。
    pub fn save_preferences(&self, preferences: Preferences) -> Result<Preferences, String> {
        validate_preferences(&preferences)?;
        let terminal_json = encode_json(&preferences.terminal, "终端外观偏好")?;

        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error("无法开始保存偏好", error))?;
        transaction
            .execute(
                "INSERT INTO preferences (singleton_id, theme, terminal_json) VALUES (1, ?1, ?2)
                 ON CONFLICT(singleton_id) DO UPDATE SET
                    theme = excluded.theme, terminal_json = excluded.terminal_json",
                params![preferences.theme, terminal_json],
            )
            .map_err(|error| database_error("无法保存偏好", error))?;
        transaction
            .commit()
            .map_err(|error| database_error("无法完成偏好保存", error))?;

        Ok(preferences)
    }

    /// 列出某服务的 Markdown 文档，按最近更新时间倒序排列。
    pub fn documents(&self, service_id: &str) -> Result<Vec<ServiceDocument>, String> {
        let connection = self.connection()?;
        let mut statement = connection
            .prepare(
                "SELECT id, service_id, title, content, updated_at FROM documents
                 WHERE service_id = ?1 ORDER BY updated_at DESC, id",
            )
            .map_err(|error| database_error("无法读取服务文档", error))?;
        let rows = statement
            .query_map([service_id], document_from_row)
            .map_err(|error| database_error("无法读取服务文档", error))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .map_err(|error| database_error("无法读取服务文档", error))
    }

    /// 读取单个 Markdown 文档。
    pub fn document(&self, id: &str) -> Result<ServiceDocument, String> {
        let connection = self.connection()?;
        connection
            .query_row(
                "SELECT id, service_id, title, content, updated_at FROM documents WHERE id = ?1",
                [id],
                document_from_row,
            )
            .optional()
            .map_err(|error| database_error("无法读取服务文档", error))?
            .ok_or_else(|| format!("找不到服务文档：{}", id))
    }

    /// 保存 Markdown 文档。新文档 ID 由后端生成；更新时由后端刷新 `updated_at`。
    pub fn save_document(&self, mut document: ServiceDocument) -> Result<ServiceDocument, String> {
        let mut connection = self.connection()?;
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|error| database_error("无法开始保存服务文档", error))?;

        let parent_exists = transaction
            .query_row(
                "SELECT 1 FROM services WHERE id = ?1",
                [&document.service_id],
                |_| Ok(()),
            )
            .optional()
            .map_err(|error| database_error("无法检查文档所属服务", error))?
            .is_some();
        if !parent_exists {
            return Err(format!(
                "找不到文档所属的服务，无法保存文档：{}",
                document.service_id
            ));
        }

        let requested_id = document.id.trim();
        let is_new = requested_id.is_empty();
        let existing_service_id = if is_new {
            None
        } else {
            transaction
                .query_row(
                    "SELECT service_id FROM documents WHERE id = ?1",
                    [requested_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(|error| database_error("无法检查服务文档", error))?
        };
        if !is_new && existing_service_id.is_none() {
            return Err(format!("服务文档不存在，无法更新：{}", requested_id));
        }
        if existing_service_id
            .as_deref()
            .is_some_and(|existing_id| existing_id != document.service_id)
        {
            return Err("不能将服务文档移动到其他服务。".into());
        }
        let id = if is_new {
            Uuid::new_v4().to_string()
        } else {
            requested_id.to_owned()
        };
        let updated_at = timestamp_now();

        if !is_new {
            transaction
                .execute(
                    "UPDATE documents SET title = ?2, content = ?3, updated_at = ?4
                     WHERE id = ?1",
                    params![id, document.title, document.content, updated_at],
                )
                .map_err(|error| database_error("无法更新服务文档", error))?;
        } else {
            transaction
                .execute(
                    "INSERT INTO documents (id, service_id, title, content, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![
                        id,
                        document.service_id,
                        document.title,
                        document.content,
                        updated_at,
                    ],
                )
                .map_err(|error| database_error("无法保存服务文档", error))?;
        }

        transaction
            .commit()
            .map_err(|error| database_error("无法完成服务文档保存", error))?;

        document.id = id;
        document.updated_at = updated_at;
        Ok(document)
    }

    /// 删除单个 Markdown 文档。
    pub fn delete_document(&self, id: &str) -> Result<(), String> {
        let connection = self.connection()?;
        connection
            .execute("DELETE FROM documents WHERE id = ?1", [id])
            .map_err(|error| database_error("无法删除服务文档", error))?;
        Ok(())
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, String> {
        self.connection
            .lock()
            .map_err(|_| "本地数据库访问状态异常，无法继续操作。".into())
    }
}

/// 供启动服务的运行时代码复用：配置读取不验证路径，启动前再检查路径仍有效。
///
/// 这样用户可以正常查看目录已失效的旧配置，同时不会在新保存或启动进程时使用失效路径。
pub fn validate_service_paths(service: &ServiceConfig) -> Result<(), String> {
    validate_directory(&service.working_dir, "工作目录")?;

    match service.environment_type.as_str() {
        "system" => {}
        "venv" => {
            validate_directory(&service.environment_dir, "Python 虚拟环境目录")?;
            let root = Path::new(&service.environment_dir);
            let python = if cfg!(windows) {
                root.join("Scripts").join("python.exe")
            } else {
                root.join("bin").join("python")
            };
            if !root.join("pyvenv.cfg").is_file() || !python.is_file() {
                return Err(format!(
                    "Python 虚拟环境目录必须包含 pyvenv.cfg 和 {}。",
                    if cfg!(windows) {
                        "Scripts/python.exe"
                    } else {
                        "bin/python"
                    }
                ));
            }
        }
        "custom" => {}
        other => {
            return Err(format!(
                "环境类型只能是 system、venv 或 custom，当前值为：{}",
                other
            ));
        }
    }

    // 非 venv 配置也允许不填写环境目录；一旦填写，就必须是现有目录。
    if !service.environment_dir.is_empty() && service.environment_type != "venv" {
        validate_directory(&service.environment_dir, "环境目录")?;
    }
    for directory in &service.executable_dirs {
        validate_directory(directory, "可执行文件搜索目录")?;
    }

    Ok(())
}

/// 校验可由平台启动的完整服务配置，用于外部实例转换成可重启的受管服务前验收。
///
/// 此函数刻意不根据 `source` 拒绝外部配置：调用方可先对用户补齐后的启动配方做完整
/// 校验，再原子地切换来源并清除外部身份。实际启动和终端仍应调用严格路径校验。
pub fn validate_service_for_launch(service: &ServiceConfig) -> Result<(), String> {
    validate_service_name(service)?;
    validate_launch_kind(service)?;
    reject_nul(&service.command, "启动命令")?;
    reject_nul(&service.shell, "Shell 名称")?;
    match &service.launch {
        LaunchConfig::Command => {
            if service.command.trim().is_empty() {
                return Err("启动命令不能为空。".into());
            }
            validate_service_paths(service)?;
        }
        LaunchConfig::Frontend {
            package_manager,
            script,
            args,
            url,
        } => {
            validate_service_paths(service)?;
            if !matches!(package_manager.as_str(), "npm" | "pnpm" | "yarn" | "bun") {
                return Err("请选择 npm、pnpm、yarn 或 bun 包管理器。".into());
            }
            if script.is_empty() || script.starts_with('-') {
                return Err("请选择有效的项目脚本。".into());
            }
            reject_nul(script, "项目脚本")?;
            for arg in args {
                reject_nul(arg, "脚本参数")?;
            }
            if !url.is_empty() {
                let parsed = tauri::Url::parse(url).map_err(|_| "访问地址格式无效")?;
                if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
                    return Err("访问地址只支持 HTTP 或 HTTPS。".into());
                }
            }
            let project = crate::projects::inspect(&service.working_dir)?;
            if !project.scripts.contains_key(script) {
                return Err(format!("package.json 中没有脚本：{script}"));
            }
        }
        LaunchConfig::StaticSite { root, .. } => {
            validate_directory(root, "静态目录")?;
            if !Path::new(root).join("index.html").is_file() {
                return Err("静态目录缺少 index.html，请先完成构建或选择正确目录。".into());
            }
            if !service.working_dir.is_empty() {
                validate_directory(&service.working_dir, "终端工作目录")?;
            }
        }
        LaunchConfig::DesktopApp {
            path,
            args,
            start_dir,
        } => {
            reject_nul(path, "应用路径")?;
            if !Path::new(path).is_absolute() || !Path::new(path).exists() {
                return Err("请选择现有应用的绝对路径。".into());
            }
            for arg in args {
                reject_nul(arg, "应用参数")?;
            }
            if !start_dir.is_empty() {
                validate_directory(start_dir, "应用启动目录")?;
            }
            if !service.working_dir.is_empty() {
                validate_directory(&service.working_dir, "终端工作目录")?;
            }
        }
    }
    validate_environment(&service.env, "服务环境变量")?;
    let _ = deduplicate_ports(&service.expected_ports)?;
    Ok(())
}

fn validate_service(service: &ServiceConfig) -> Result<(), String> {
    validate_service_name(service)?;
    validate_launch_kind(service)?;
    // 应用的当前身份在 application_bindings 中独立保存，external 字段不再是其必填项。
    if service.kind == "desktopApp" {
        if !matches!(service.source.as_str(), "managed" | "external") {
            return Err("应用来源无效。".into());
        }
        if let Some(instance) = &service.external {
            validate_external_instance(instance)?;
        }
        return validate_service_for_launch(service);
    }
    match service.source.as_str() {
        "managed" => {
            if service.external.is_some() {
                return Err("平台启动的服务不能同时携带外部进程身份。".into());
            }
            validate_service_for_launch(service)
        }
        "external" => {
            let external = service
                .external
                .as_ref()
                .ok_or_else(|| "外部实例缺少进程身份信息。".to_owned())?;
            validate_external_instance(external)?;
            // 外部实例暂不启动，其观察到的路径可能已失效，cwd 与启动命令也允许为空。
            // 仍校验可序列化配置中会影响后续转换或传输的文本和通用服务字段。
            reject_nul(&service.working_dir, "工作目录")?;
            reject_nul(&service.environment_dir, "环境目录")?;
            for directory in &service.executable_dirs {
                reject_nul(directory, "可执行文件搜索目录")?;
            }
            reject_nul(&service.command, "启动命令")?;
            reject_nul(&service.shell, "Shell 名称")?;
            validate_environment(&service.env, "服务环境变量")?;
            let _ = deduplicate_ports(&service.expected_ports)?;
            Ok(())
        }
        other => Err(format!(
            "服务来源只能是 managed 或 external，当前值为：{}",
            other
        )),
    }
}

fn validate_launch_kind(service: &ServiceConfig) -> Result<(), String> {
    let expected = match &service.launch {
        LaunchConfig::Command => "command",
        LaunchConfig::Frontend { .. } => "frontend",
        LaunchConfig::StaticSite { .. } => "staticSite",
        LaunchConfig::DesktopApp { .. } => "desktopApp",
    };
    if service.kind != expected {
        return Err("服务类型与启动配置不一致，请重新选择服务类型。".into());
    }
    if service.kind == "staticSite" && service.source != "managed" {
        return Err("静态站点只支持由平台启动。".into());
    }
    Ok(())
}

fn validate_service_name(service: &ServiceConfig) -> Result<(), String> {
    if service.name.trim().is_empty() {
        return Err("服务名称不能为空。".into());
    }
    Ok(())
}

fn validate_external_instance(external: &ExternalInstance) -> Result<(), String> {
    if external.identity.pid == 0 {
        return Err("外部实例 PID 必须大于 0。".into());
    }
    if external.identity.created_at.trim().is_empty() {
        return Err("外部实例缺少进程创建身份。".into());
    }
    if external.name.trim().is_empty() {
        return Err("外部实例名称不能为空。".into());
    }
    if !matches!(
        external.category.as_str(),
        "system" | "nonSystem" | "unknown"
    ) {
        return Err(format!("外部实例分类无效：{}", external.category));
    }

    reject_nul(&external.identity.created_at, "外部进程创建身份")?;
    reject_nul(&external.name, "外部实例名称")?;
    if let Some(executable) = &external.executable {
        reject_nul(executable, "外部实例可执行文件")?;
    }
    if let Some(working_dir) = &external.working_dir {
        reject_nul(working_dir, "外部实例工作目录")?;
    }
    if let Some(reason) = &external.control_reason {
        reject_nul(reason, "外部实例控制说明")?;
    }
    for reason in &external.reasons {
        reject_nul(reason, "外部实例分类依据")?;
    }
    Ok(())
}

fn validate_agent(agent: &AgentProfile) -> Result<(), String> {
    if agent.name.trim().is_empty() {
        return Err("Agent 名称不能为空。".into());
    }
    if agent.executable.trim().is_empty() {
        return Err("Agent 可执行文件不能为空。".into());
    }
    reject_nul(&agent.executable, "Agent 可执行文件")?;
    for (index, argument) in agent.args.iter().enumerate() {
        reject_nul(argument, &format!("Agent 第 {} 个启动参数", index + 1))?;
    }
    validate_environment(&agent.env, "Agent 环境变量")
}

fn validate_preferences(preferences: &Preferences) -> Result<(), String> {
    if !matches!(preferences.theme.as_str(), "system" | "dark" | "light") {
        return Err("主题只能是 system、dark 或 light。".into());
    }
    validate_terminal_appearance(&preferences.terminal)
}

fn validate_terminal_appearance(appearance: &TerminalAppearance) -> Result<(), String> {
    if !matches!(
        appearance.theme.as_str(),
        "system" | "dark" | "light" | "dracula" | "solarizedDark" | "custom"
    ) {
        return Err(
            "终端主题只能是 system、dark、light、dracula、solarizedDark 或 custom。".into(),
        );
    }
    if appearance.font_family.trim().is_empty() {
        return Err("终端字体不能为空。".into());
    }
    if appearance.font_family.chars().count() > 300 {
        return Err("终端字体名称不能超过 300 个字符。".into());
    }
    if appearance.font_family.chars().any(char::is_control) {
        return Err("终端字体名称不能包含控制字符。".into());
    }
    if !appearance.font_size.is_finite() || !(9.0..=32.0).contains(&appearance.font_size) {
        return Err("终端字体大小必须是 9 到 32 之间的有限数字。".into());
    }
    if !appearance.line_height.is_finite() || !(1.0..=2.0).contains(&appearance.line_height) {
        return Err("终端行高必须是 1 到 2 之间的有限数字。".into());
    }
    if !matches!(
        appearance.cursor_style.as_str(),
        "block" | "underline" | "bar"
    ) {
        return Err("终端光标样式只能是 block、underline 或 bar。".into());
    }

    let colors = &appearance.colors;
    for (label, value) in [
        ("background", &colors.background),
        ("foreground", &colors.foreground),
        ("cursor", &colors.cursor),
        ("selectionBackground", &colors.selection_background),
        ("black", &colors.black),
        ("red", &colors.red),
        ("green", &colors.green),
        ("yellow", &colors.yellow),
        ("blue", &colors.blue),
        ("magenta", &colors.magenta),
        ("cyan", &colors.cyan),
        ("white", &colors.white),
        ("brightBlack", &colors.bright_black),
        ("brightRed", &colors.bright_red),
        ("brightGreen", &colors.bright_green),
        ("brightYellow", &colors.bright_yellow),
        ("brightBlue", &colors.bright_blue),
        ("brightMagenta", &colors.bright_magenta),
        ("brightCyan", &colors.bright_cyan),
        ("brightWhite", &colors.bright_white),
    ] {
        if !is_hex_color(value) {
            return Err(format!("终端颜色 {} 必须是 #RRGGBB。", label));
        }
    }
    Ok(())
}

fn is_hex_color(value: &str) -> bool {
    value.len() == 7
        && value.as_bytes().first() == Some(&b'#')
        && value.as_bytes()[1..]
            .iter()
            .all(|byte| byte.is_ascii_hexdigit())
}

fn reject_nul(value: &str, label: &str) -> Result<(), String> {
    if value.contains('\0') {
        return Err(format!("{}不能包含 NUL 字符。", label));
    }
    Ok(())
}

fn validate_environment(values: &BTreeMap<String, String>, label: &str) -> Result<(), String> {
    for (key, value) in values {
        if key.trim().is_empty() {
            return Err(format!("{}的键不能为空。", label));
        }
        if key.contains('=') {
            return Err(format!("{}的键不能包含等号（=）：{}", label, key));
        }
        if key.contains('\0') || value.contains('\0') {
            return Err(format!("{}的键和值都不能包含 NUL 字符。", label));
        }
    }
    Ok(())
}

fn validate_directory(path: &str, label: &str) -> Result<(), String> {
    if path.trim().is_empty() {
        return Err(format!("{}不能为空。", label));
    }
    if !Path::new(path).is_absolute() {
        return Err(format!("{}必须使用绝对路径：{}", label, path));
    }
    if !Path::new(path).is_dir() {
        return Err(format!("{}必须是现有文件夹：{}", label, path));
    }
    Ok(())
}

fn deduplicate_ports(ports: &[u16]) -> Result<Vec<u16>, String> {
    let mut seen = HashSet::with_capacity(ports.len());
    let mut unique = Vec::with_capacity(ports.len());
    for port in ports {
        if *port == 0 {
            return Err("预期端口必须在 1 到 65535 之间。".into());
        }
        if seen.insert(*port) {
            unique.push(*port);
        }
    }
    Ok(unique)
}

fn migrate(connection: &mut Connection) -> Result<(), String> {
    let current_version =
        schema_version(connection).map_err(|error| database_error("无法读取数据库版本", error))?;
    if current_version > SCHEMA_VERSION {
        return Err(format!(
            "数据库版本 {} 高于此应用支持的版本 {}。",
            current_version, SCHEMA_VERSION
        ));
    }
    if current_version == SCHEMA_VERSION {
        return Ok(());
    }

    // 在拿到写锁后重新检查版本，避免两个应用实例同时首次打开时重复执行迁移和种子数据。
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|error| database_error("无法开始数据库迁移", error))?;
    let locked_version = schema_version(&transaction)
        .map_err(|error| database_error("无法读取数据库版本", error))?;
    if locked_version > SCHEMA_VERSION {
        return Err(format!(
            "数据库版本 {} 高于此应用支持的版本 {}。",
            locked_version, SCHEMA_VERSION
        ));
    }

    if locked_version == 0 {
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS services (
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
                    external_json TEXT
                );
                CREATE TABLE IF NOT EXISTS agents (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    executable TEXT NOT NULL,
                    args_json TEXT NOT NULL,
                    env_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS preferences (
                    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
                    theme TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS documents (
                    id TEXT PRIMARY KEY NOT NULL,
                    service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                    title TEXT NOT NULL,
                    content TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS documents_service_id_idx
                    ON documents(service_id);
                CREATE INDEX IF NOT EXISTS documents_updated_at_idx
                    ON documents(service_id, updated_at DESC);",
            )
            .map_err(|error| database_error("无法建立本地数据表", error))?;

        // 默认内容只是“启动配置”：仅存储用户以后可以选择的 CLI 名称，不会安装、登录
        // 或启动任何命令。首次建库完整读取共享预设目录，避免把目录外的工具硬编码在后端。
        let presets = load_agent_catalog()?;
        insert_catalog_agents(&transaction, &presets, false)?;
        transaction
            .execute(
                "INSERT OR IGNORE INTO preferences (singleton_id, theme) VALUES (1, 'system')",
                [],
            )
            .map_err(|error| database_error("无法初始化主题偏好", error))?;
    }

    if locked_version == 1 {
        // 旧服务仍是由平台启动的配置；PID 身份只属于外部实例，所以迁移时将来源设为
        // managed、外部身份留空，并保持 services 外键下的文档表和内容不变。
        transaction
            .execute_batch(
                "ALTER TABLE services
                    ADD COLUMN source TEXT NOT NULL DEFAULT 'managed';
                 ALTER TABLE services
                    ADD COLUMN external_json TEXT;",
            )
            .map_err(|error| database_error("无法迁移服务来源与外部实例信息", error))?;
    }

    if locked_version < 3 {
        // 旧记录统一保留命令型语义；应用绑定独立保存，删除记录仅级联删除关联数据。
        transaction.execute_batch(
            "ALTER TABLE services ADD COLUMN kind TEXT NOT NULL DEFAULT 'command';
             ALTER TABLE services ADD COLUMN launch_json TEXT NOT NULL DEFAULT '{\"type\":\"command\"}';
             CREATE TABLE application_bindings (
                service_id TEXT PRIMARY KEY NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                binding_json TEXT NOT NULL
             );"
        ).map_err(|error| database_error("无法迁移多类型服务配置", error))?;
    }

    if locked_version < 4 {
        // 版本 1 的极简旧数据库可能尚未有 Agent 与偏好表；先补齐表，再添加终端 JSON，
        // 这样旧服务和文档迁移也能安全落到当前 schema。terminal_json 允许为空，以兼容
        // 仍未写入终端外观的旧行；读取时会返回模型默认值。
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS agents (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    executable TEXT NOT NULL,
                    args_json TEXT NOT NULL,
                    env_json TEXT NOT NULL
                );
                CREATE TABLE IF NOT EXISTS preferences (
                    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
                    theme TEXT NOT NULL
                );",
            )
            .map_err(|error| database_error("无法补齐旧版设置表", error))?;

        if !table_has_column(&transaction, "preferences", "terminal_json")
            .map_err(|error| database_error("无法检查终端外观字段", error))?
        {
            transaction
                .execute("ALTER TABLE preferences ADD COLUMN terminal_json TEXT", [])
                .map_err(|error| database_error("无法迁移终端外观字段", error))?;
        }

        let terminal_json = encode_json(&TerminalAppearance::default(), "终端外观默认值")?;
        transaction
            .execute(
                "UPDATE preferences SET terminal_json = ?1 WHERE terminal_json IS NULL",
                [&terminal_json],
            )
            .map_err(|error| database_error("无法初始化终端外观偏好", error))?;

        // 所有旧版本都补充 catalog 中新增的预设。旧版三项即使已被用户删除也不重新
        // 插入；迁移完成后版本号变为 4，后续重开不会再次执行这段补充逻辑。
        if locked_version > 0 {
            let presets = load_agent_catalog()?;
            insert_catalog_agents(&transaction, &presets, true)?;
        }
    }

    if locked_version < 5 {
        // 内容库与服务配置、服务文档完全分表。content_application_bindings 只保存
        // 原生应用创建身份；删除收藏会触发这里的级联解除，但不会触碰 services/documents。
        transaction
            .execute_batch(
                "CREATE TABLE IF NOT EXISTS content_items (
                    id TEXT PRIMARY KEY NOT NULL,
                    kind TEXT NOT NULL,
                    pinned INTEGER NOT NULL DEFAULT 0 CHECK (pinned IN (0, 1)),
                    config_json TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    revision INTEGER NOT NULL DEFAULT 1
                );
                CREATE INDEX IF NOT EXISTS content_items_order_idx
                    ON content_items(pinned DESC, updated_at DESC, id);
                CREATE TABLE IF NOT EXISTS content_application_bindings (
                    content_id TEXT PRIMARY KEY NOT NULL
                        REFERENCES content_items(id) ON DELETE CASCADE,
                    binding_json TEXT NOT NULL
                );
                CREATE INDEX IF NOT EXISTS content_application_bindings_identity_idx
                    ON content_application_bindings(content_id);
                CREATE TABLE IF NOT EXISTS content_preferences (
                    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
                    preferences_json TEXT NOT NULL
                );
                INSERT OR IGNORE INTO content_preferences (singleton_id, preferences_json)
                    VALUES (1, '{}');",
            )
            .map_err(|error| database_error("无法迁移内容库数据表", error))?;
        // v4 的 desktopApp 已经是真实的原生服务配置，迁移时只创建一条 serviceLink
        // 收藏作为内容库入口。服务配置、文档和 application_bindings 都留在原表；这段
        // 代码只在 user_version 尚未到 5 时执行，提交后删除收藏也不会在重开时复活。
        seed_legacy_desktop_content(&transaction)?;
    }

    transaction
        .execute_batch(&format!("PRAGMA user_version = {};", SCHEMA_VERSION))
        .map_err(|error| database_error("无法更新数据库版本", error))?;
    transaction
        .commit()
        .map_err(|error| database_error("无法完成数据库迁移", error))
}

/// 将迁移前已经存在的 desktopApp 服务一次性映射为内容收藏。
///
/// 内容项只保存 serviceId，不复制启动参数、文档或原生应用身份。迁移在同一
/// `TransactionBehavior::Immediate` 事务中执行，表创建、种子记录和 user_version 更新要么
/// 一起提交，要么一起回滚；已存在相同 serviceLink 时跳过，兼容意外中断后的重试。
fn seed_legacy_desktop_content(transaction: &Transaction<'_>) -> Result<(), String> {
    let mut statement = transaction
        .prepare(
            "SELECT id, name, description, notes, created_at, updated_at
             FROM services WHERE kind = 'desktopApp' ORDER BY created_at, id",
        )
        .map_err(|error| database_error("无法读取旧 desktopApp 服务", error))?;
    let services = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })
        .map_err(|error| database_error("无法读取旧 desktopApp 服务", error))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| database_error("无法读取旧 desktopApp 服务", error))?;
    drop(statement);

    let mut linked_service_ids = HashSet::new();
    let mut existing_items = transaction
        .prepare("SELECT config_json FROM content_items")
        .map_err(|error| database_error("无法读取已有内容收藏", error))?;
    let existing_rows = existing_items
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| database_error("无法读取已有内容收藏", error))?;
    for row in existing_rows {
        let json = row.map_err(|error| database_error("无法读取已有内容收藏", error))?;
        let item: ContentItem = decode_json(&json, "内容配置")?;
        if let ContentTarget::ServiceLink { service_id } = item.target {
            linked_service_ids.insert(service_id);
        }
    }
    drop(existing_items);

    for (service_id, name, description, notes, created_at, updated_at) in services {
        if linked_service_ids.contains(&service_id) {
            continue;
        }
        let display_name = if name.trim().is_empty() {
            format!("桌面应用 {}", service_id)
        } else {
            name
        };
        let mut item = ContentItem::new(
            display_name,
            ContentKind::Application,
            ContentTarget::ServiceLink {
                service_id: service_id.clone(),
            },
        );
        item.id = Uuid::new_v4().to_string();
        item.description = description;
        item.notes = notes;
        item.created_at = if created_at.trim().is_empty() {
            timestamp_now()
        } else {
            created_at
        };
        item.updated_at = if updated_at.trim().is_empty() {
            item.created_at.clone()
        } else {
            updated_at
        };
        item.revision = 1;
        validate_content_item(&item)?;
        let config_json = encode_json(&item, "迁移后的内容配置")?;
        transaction
            .execute(
                "INSERT INTO content_items
                    (id, kind, pinned, config_json, created_at, updated_at, revision)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    item.id,
                    item.kind.as_str(),
                    if item.pinned { 1 } else { 0 },
                    config_json,
                    item.created_at,
                    item.updated_at,
                    item.revision as i64,
                ],
            )
            .map_err(|error| database_error("无法写入旧 desktopApp 内容收藏", error))?;
        linked_service_ids.insert(service_id);
    }
    Ok(())
}

fn schema_version(connection: &Connection) -> rusqlite::Result<i64> {
    connection.pragma_query_value(None, "user_version", |row| row.get(0))
}

fn table_has_column(connection: &Connection, table: &str, column: &str) -> rusqlite::Result<bool> {
    // 表名来自后端固定迁移代码，不接受外部输入；PRAGMA 结果只读取列名，避免依赖
    // SQLite 在不同版本上的 ALTER TABLE 错误文本。
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, String>(1)? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

/// 读取编译时嵌入的 Agent 预设目录，确保开发运行目录和发布包中的配置完全一致。
fn load_agent_catalog() -> Result<Vec<AgentProfile>, String> {
    let presets: Vec<AgentProfile> =
        serde_json::from_str(include_str!("../../assets/agent-presets.json"))
            .map_err(|error| format!("Agent 预设目录格式无效：{}", error))?;
    for preset in &presets {
        if preset.id.trim().is_empty() {
            return Err(format!("Agent 预设 {} 缺少 ID。", preset.name));
        }
        validate_agent(preset)
            .map_err(|error| format!("Agent 预设 {} 无效：{}", preset.name, error))?;
    }
    Ok(presets)
}

fn insert_catalog_agents(
    transaction: &Transaction<'_>,
    presets: &[AgentProfile],
    skip_legacy_builtins: bool,
) -> Result<(), String> {
    let mut existing = read_agents(transaction)?;
    for preset in presets {
        if skip_legacy_builtins && is_legacy_builtin_agent(preset) {
            continue;
        }
        if existing
            .iter()
            .any(|agent| agent_identity_matches(agent, preset))
        {
            continue;
        }

        // 预设在读取时已完整校验，稳定 ID 可供后续模板匹配，不能在迁移时随机改写。
        insert_agent_record(transaction, preset)?;
        existing.push(preset.clone());
    }
    Ok(())
}

fn read_agents(connection: &Connection) -> Result<Vec<AgentProfile>, String> {
    let mut statement = connection
        .prepare("SELECT id, name, executable, args_json, env_json FROM agents")
        .map_err(|error| database_error("无法读取 Agent 预设去重数据", error))?;
    let rows = statement
        .query_map([], stored_agent_from_row)
        .map_err(|error| database_error("无法读取 Agent 预设去重数据", error))?;
    rows.map(|row| {
        row.map_err(|error| database_error("无法读取 Agent 预设去重数据", error))?
            .into_model()
    })
    .collect()
}

fn insert_agent_record(transaction: &Transaction<'_>, agent: &AgentProfile) -> Result<(), String> {
    let args_json = encode_json(&agent.args, "Agent 启动参数")?;
    let env_json = encode_json(&agent.env, "Agent 环境变量")?;
    transaction
        .execute(
            "INSERT INTO agents (id, name, executable, args_json, env_json)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![agent.id, agent.name, agent.executable, args_json, env_json],
        )
        .map_err(|error| database_error("无法初始化默认 Agent 配置", error))?;
    Ok(())
}

fn agent_identity_matches(left: &AgentProfile, right: &AgentProfile) -> bool {
    (!left.id.trim().is_empty() && fold_text(&left.id) == fold_text(&right.id))
        || fold_text(&left.name) == fold_text(&right.name)
        || (!agent_executable_key(&left.executable).is_empty()
            && agent_executable_key(&left.executable) == agent_executable_key(&right.executable))
}

fn is_legacy_builtin_agent(agent: &AgentProfile) -> bool {
    let name = fold_text(&agent.name);
    let executable = agent_executable_key(&agent.executable);
    matches!(name.as_str(), "codex" | "claude code" | "gemini cli")
        || matches!(executable.as_str(), "codex" | "claude" | "gemini")
}

fn fold_text(value: &str) -> String {
    value.trim().to_lowercase()
}

fn agent_executable_key(value: &str) -> String {
    let value = value
        .trim()
        .trim_matches(|character| character == '"' || character == '\'');
    let basename = value
        .rsplit(|character| character == '/' || character == '\\')
        .next()
        .unwrap_or(value)
        .to_lowercase();
    for extension in [".exe", ".cmd", ".bat"] {
        if let Some(stem) = basename.strip_suffix(extension) {
            return stem.to_owned();
        }
    }
    basename
}

fn timestamp_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

fn encode_json<T: Serialize + ?Sized>(value: &T, label: &str) -> Result<String, String> {
    serde_json::to_string(value).map_err(|error| format!("无法序列化{}：{}", label, error))
}

fn decode_json<T: DeserializeOwned>(value: &str, label: &str) -> Result<T, String> {
    serde_json::from_str(value).map_err(|error| format!("数据库中的{}无法解析：{}", label, error))
}

fn database_error(label: &str, error: impl std::fmt::Display) -> String {
    format!("{}：{}", label, error)
}

struct StoredService {
    id: String,
    name: String,
    description: String,
    working_dir: String,
    environment_type: String,
    environment_dir: String,
    executable_dirs_json: String,
    env_json: String,
    command: String,
    shell: String,
    expected_ports_json: String,
    notes: String,
    created_at: String,
    updated_at: String,
    source: String,
    external_json: Option<String>,
    kind: String,
    launch_json: String,
}

impl StoredService {
    fn into_model(self) -> Result<ServiceConfig, String> {
        Ok(ServiceConfig {
            id: self.id,
            kind: self.kind,
            launch: decode_json(&self.launch_json, "启动配置")?,
            name: self.name,
            description: self.description,
            working_dir: self.working_dir,
            environment_type: self.environment_type,
            environment_dir: self.environment_dir,
            executable_dirs: decode_json(&self.executable_dirs_json, "可执行文件目录")?,
            env: decode_json(&self.env_json, "服务环境变量")?,
            command: self.command,
            shell: self.shell,
            expected_ports: decode_json(&self.expected_ports_json, "预期端口")?,
            notes: self.notes,
            created_at: self.created_at,
            updated_at: self.updated_at,
            source: self.source,
            external: self
                .external_json
                .map(|json| decode_json(&json, "外部实例"))
                .transpose()?,
        })
    }
}

fn stored_service_from_row(row: &Row<'_>) -> rusqlite::Result<StoredService> {
    Ok(StoredService {
        id: row.get(0)?,
        name: row.get(1)?,
        description: row.get(2)?,
        working_dir: row.get(3)?,
        environment_type: row.get(4)?,
        environment_dir: row.get(5)?,
        executable_dirs_json: row.get(6)?,
        env_json: row.get(7)?,
        command: row.get(8)?,
        shell: row.get(9)?,
        expected_ports_json: row.get(10)?,
        notes: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        source: row.get(14)?,
        external_json: row.get(15)?,
        kind: row.get(16)?,
        launch_json: row.get(17)?,
    })
}

fn fetch_service(connection: &Connection, id: &str) -> Result<Option<ServiceConfig>, String> {
    let stored = connection
        .query_row(
            "SELECT id, name, description, working_dir, environment_type, environment_dir,
             executable_dirs_json, env_json, command, shell, expected_ports_json, notes,
             created_at, updated_at, source, external_json, kind, launch_json FROM services WHERE id = ?1",
            [id],
            stored_service_from_row,
        )
        .optional()
        .map_err(|error| database_error("无法读取服务配置", error))?;
    stored.map(StoredService::into_model).transpose()
}

/// 在互斥连接持有的写事务内检查外部进程身份是否已绑定到其他服务。
///
/// PID 会被系统复用，因此以 PID 与进程创建身份组成唯一实例键；同一服务更新时排除自身，
/// 允许只修改名称或备注而继续绑定当前实例。
fn ensure_unique_external_identity(
    transaction: &Transaction<'_>,
    external: &ExternalInstance,
    current_service_id: &str,
) -> Result<(), String> {
    let mut statement = transaction
        .prepare(
            "SELECT external_json FROM services
             WHERE source = 'external' AND kind <> 'desktopApp' AND id <> ?1",
        )
        .map_err(|error| database_error("无法检查重复的外部实例", error))?;
    let rows = statement
        .query_map([current_service_id], |row| row.get::<_, Option<String>>(0))
        .map_err(|error| database_error("无法检查重复的外部实例", error))?;
    let stored_instances = rows
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| database_error("无法检查重复的外部实例", error))?;
    drop(statement);

    for value in stored_instances {
        let json = value.ok_or_else(|| "已有外部服务记录缺少进程身份信息。".to_owned())?;
        let existing: ExternalInstance = decode_json(&json, "外部实例")?;
        if existing.identity.pid == external.identity.pid
            && existing.identity.created_at == external.identity.created_at
        {
            return Err("该外部进程实例已接入平台。".into());
        }
    }
    let mut statement = transaction
        .prepare("SELECT binding_json FROM application_bindings WHERE service_id <> ?1")
        .map_err(|e| database_error("无法检查应用关联", e))?;
    let rows = statement
        .query_map([current_service_id], |row| row.get::<_, String>(0))
        .map_err(|e| database_error("无法检查应用关联", e))?;
    for row in rows {
        let existing: ApplicationBinding = decode_json(
            &row.map_err(|e| database_error("无法检查应用关联", e))?,
            "应用关联",
        )?;
        if existing.instance.identity == external.identity {
            return Err("该进程实例已作为桌面应用接入平台。".into());
        }
    }
    let mut statement = transaction
        .prepare("SELECT binding_json FROM content_application_bindings")
        .map_err(|e| database_error("无法检查内容应用关联", e))?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| database_error("无法检查内容应用关联", e))?;
    for row in rows {
        let existing: ContentApplicationBinding = decode_json(
            &row.map_err(|e| database_error("无法检查内容应用关联", e))?,
            "内容应用关联",
        )?;
        if existing.identity == external.identity {
            return Err("该进程实例已由内容收藏关联。".into());
        }
    }
    Ok(())
}

/// 内容收藏与桌面服务共用同一实例所有权：PID 复用时必须同时匹配创建身份，不能因为
/// 内容目标路径相同就让两条记录重复接管同一个原生应用。
fn ensure_unique_content_identity(
    transaction: &Transaction<'_>,
    identity: &crate::models::ProcessIdentity,
    current_content_id: &str,
) -> Result<(), String> {
    let mut content_statement = transaction
        .prepare(
            "SELECT content_id, binding_json FROM content_application_bindings
             WHERE content_id <> ?1",
        )
        .map_err(|error| database_error("无法检查重复的内容应用实例", error))?;
    let content_rows = content_statement
        .query_map([current_content_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| database_error("无法检查重复的内容应用实例", error))?;
    for row in content_rows {
        let (content_id, json) =
            row.map_err(|error| database_error("无法检查重复的内容应用实例", error))?;
        let binding: ContentApplicationBinding = decode_json(&json, "内容应用关联")?;
        if binding.identity == *identity {
            return Err(format!("该原生应用实例已由内容 {} 关联。", content_id));
        }
    }
    drop(content_statement);

    let mut binding_statement = transaction
        .prepare("SELECT service_id, binding_json FROM application_bindings")
        .map_err(|error| database_error("无法检查桌面服务应用实例", error))?;
    let binding_rows = binding_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| database_error("无法检查桌面服务应用实例", error))?;
    for row in binding_rows {
        let (service_id, json) =
            row.map_err(|error| database_error("无法检查桌面服务应用实例", error))?;
        let binding: ApplicationBinding = decode_json(&json, "应用关联")?;
        if binding.instance.identity == *identity {
            return Err(format!(
                "该原生应用实例已由桌面服务 {} 关联；请收藏该服务。",
                service_id
            ));
        }
    }
    drop(binding_statement);

    // 非桌面外部服务没有 application_bindings 行，仍应阻止内容把相同 PID 接管为 GUI 应用。
    let mut service_statement = transaction
        .prepare(
            "SELECT id, external_json FROM services
             WHERE source = 'external' AND external_json IS NOT NULL",
        )
        .map_err(|error| database_error("无法检查外部服务应用实例", error))?;
    let service_rows = service_statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| database_error("无法检查外部服务应用实例", error))?;
    for row in service_rows {
        let (service_id, json) =
            row.map_err(|error| database_error("无法检查外部服务应用实例", error))?;
        let external: ExternalInstance = decode_json(&json, "外部实例")?;
        if external.identity == *identity {
            return Err(format!("该进程实例已由外部服务 {} 关联。", service_id));
        }
    }
    Ok(())
}

struct StoredAgent {
    id: String,
    name: String,
    executable: String,
    args_json: String,
    env_json: String,
}

impl StoredAgent {
    fn into_model(self) -> Result<AgentProfile, String> {
        Ok(AgentProfile {
            id: self.id,
            name: self.name,
            executable: self.executable,
            args: decode_json(&self.args_json, "Agent 启动参数")?,
            env: decode_json(&self.env_json, "Agent 环境变量")?,
        })
    }
}

fn stored_agent_from_row(row: &Row<'_>) -> rusqlite::Result<StoredAgent> {
    Ok(StoredAgent {
        id: row.get(0)?,
        name: row.get(1)?,
        executable: row.get(2)?,
        args_json: row.get(3)?,
        env_json: row.get(4)?,
    })
}

fn fetch_agent(connection: &Connection, id: &str) -> Result<Option<AgentProfile>, String> {
    let stored = connection
        .query_row(
            "SELECT id, name, executable, args_json, env_json FROM agents WHERE id = ?1",
            [id],
            stored_agent_from_row,
        )
        .optional()
        .map_err(|error| database_error("无法读取 Agent 配置", error))?;
    stored.map(StoredAgent::into_model).transpose()
}

fn document_from_row(row: &Row<'_>) -> rusqlite::Result<ServiceDocument> {
    Ok(ServiceDocument {
        id: row.get(0)?,
        service_id: row.get(1)?,
        title: row.get(2)?,
        content: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_models::{ContentKind, ContentTarget};
    use crate::models::{ProcessIdentity, TerminalAppearance};
    use std::path::PathBuf;

    #[test]
    fn version_two_preserves_documents_and_application_identity_is_unique() -> Result<(), String> {
        let root = tempfile::tempdir().map_err(|e| e.to_string())?;
        let db = root.path().join("迁移.sqlite3");
        let store = Store::open(&db)?;
        let old =
            store.save_service(service_config(root.path().to_str().unwrap(), "旧命令服务"))?;
        store.save_document(ServiceDocument {
            id: String::new(),
            service_id: old.id.clone(),
            title: "保留文档".into(),
            content: "迁移不丢失内容".into(),
            updated_at: String::new(),
        })?;
        drop(store);
        let connection = Connection::open(&db).map_err(|e| e.to_string())?;
        connection.execute_batch("DROP TABLE application_bindings; ALTER TABLE services DROP COLUMN kind; ALTER TABLE services DROP COLUMN launch_json; PRAGMA user_version = 2;").map_err(|e| e.to_string())?;
        drop(connection);
        let store = Store::open(&db)?;
        assert_eq!(store.service(&old.id)?.kind, "command");
        assert_eq!(store.documents(&old.id)?[0].content, "迁移不丢失内容");
        let target = root.path().join("演示.app");
        std::fs::create_dir(&target).map_err(|e| e.to_string())?;
        let mut app = service_config(root.path().to_str().unwrap(), "桌面应用");
        app.kind = "desktopApp".into();
        app.launch = LaunchConfig::DesktopApp {
            path: target.to_string_lossy().into_owned(),
            args: vec![],
            start_dir: String::new(),
        };
        app.command.clear();
        app.working_dir.clear();
        let app = store.save_service(app)?;
        let external = external_config(48791, "验证身份", "外部应用")
            .external
            .unwrap();
        let binding = ApplicationBinding {
            instance: external,
            target_path: target.to_string_lossy().into_owned(),
            bundle_id: None,
            origin: "attached".into(),
            bound_at: timestamp_now(),
        };
        store.bind_application(&app.id, &binding)?;
        assert_eq!(
            store
                .application_binding(&app.id)?
                .unwrap()
                .instance
                .identity,
            binding.instance.identity
        );
        assert!(store
            .save_service(external_config(48791, "验证身份", "重复端口记录"))
            .is_err());
        let mut duplicate = app.clone();
        duplicate.id.clear();
        duplicate.name = "重复应用".into();
        let duplicate = store.save_service(duplicate)?;
        assert!(store.bind_application(&duplicate.id, &binding).is_err());
        store.delete_service(&app.id)?;
        assert!(store.application_binding(&app.id)?.is_none());
        assert_eq!(store.documents(&old.id)?.len(), 1);
        Ok(())
    }

    fn temporary_database() -> (PathBuf, PathBuf) {
        let directory = std::env::temp_dir().join(format!("kacent-store-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&directory).expect("测试数据库目录应可创建");
        (directory.clone(), directory.join("store.sqlite3"))
    }

    fn service_config(working_dir: &str, name: &str) -> ServiceConfig {
        ServiceConfig {
            id: String::new(),
            kind: "command".into(),
            launch: LaunchConfig::Command,
            source: "managed".into(),
            external: None,
            name: name.into(),
            description: "本地测试配置".into(),
            working_dir: working_dir.into(),
            environment_type: "system".into(),
            environment_dir: String::new(),
            executable_dirs: Vec::new(),
            env: BTreeMap::from([("APP_MODE".into(), "test".into())]),
            command: "echo ready".into(),
            shell: "sh".into(),
            expected_ports: vec![4317, 4317],
            notes: "roundtrip".into(),
            created_at: "由后端覆盖".into(),
            updated_at: "由后端覆盖".into(),
        }
    }

    fn external_instance(pid: u32, created_at: &str) -> ExternalInstance {
        ExternalInstance {
            identity: ProcessIdentity {
                pid,
                created_at: created_at.into(),
            },
            name: "外部运行实例".into(),
            executable: Some("/missing/example-service".into()),
            working_dir: Some("/missing/example-working-dir".into()),
            category: "nonSystem".into(),
            reasons: vec!["已识别为第三方程序".into()],
            shared_host: false,
            controllable: true,
            control_reason: None,
        }
    }

    fn external_config(pid: u32, created_at: &str, name: &str) -> ServiceConfig {
        let mut service = service_config("", name);
        service.source = "external".into();
        service.external = Some(external_instance(pid, created_at));
        service
    }

    fn create_version_one_database(path: &std::path::Path) -> Result<(), String> {
        let connection = Connection::open(path)
            .map_err(|error| database_error("无法建立版本 1 测试数据库", error))?;
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
                    updated_at TEXT NOT NULL
                );
                CREATE TABLE documents (
                    id TEXT PRIMARY KEY NOT NULL,
                    service_id TEXT NOT NULL REFERENCES services(id) ON DELETE CASCADE,
                    title TEXT NOT NULL,
                    content TEXT NOT NULL,
                    updated_at TEXT NOT NULL
                );
                CREATE INDEX documents_service_id_idx ON documents(service_id);
                CREATE INDEX documents_updated_at_idx ON documents(service_id, updated_at DESC);
                PRAGMA user_version = 1;",
            )
            .map_err(|error| database_error("无法建立版本 1 测试表", error))?;
        connection
            .execute(
                "INSERT INTO services (
                    id, name, description, working_dir, environment_type, environment_dir,
                    executable_dirs_json, env_json, command, shell, expected_ports_json,
                    notes, created_at, updated_at
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    "legacy-service",
                    "旧服务",
                    "迁移验证",
                    "",
                    "system",
                    "",
                    "[]",
                    "{}",
                    "echo legacy",
                    "sh",
                    "[4317]",
                    "保留文档",
                    "2025-01-01T00:00:00Z",
                    "2025-01-02T00:00:00Z",
                ],
            )
            .map_err(|error| database_error("无法写入版本 1 测试服务", error))?;
        connection
            .execute(
                "INSERT INTO documents (id, service_id, title, content, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    "legacy-document",
                    "legacy-service",
                    "旧版操作说明",
                    "迁移后仍须保留的内容。",
                    "2025-01-03T00:00:00Z",
                ],
            )
            .map_err(|error| database_error("无法写入版本 1 测试文档", error))?;
        Ok(())
    }

    /// 版本 1 服务迁移为 managed，且已有服务文档和外键关系不变。
    #[test]
    fn migrates_version_one_services_without_losing_documents() -> Result<(), String> {
        let (directory, database_path) = temporary_database();
        create_version_one_database(&database_path)?;

        let store = Store::open(&database_path)?;
        let service = store.service("legacy-service")?;
        assert_eq!(service.source, "managed");
        assert!(service.external.is_none());
        let documents = store.documents("legacy-service")?;
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0].id, "legacy-document");
        assert_eq!(documents[0].content, "迁移后仍须保留的内容。");
        let connection = store.connection()?;
        assert_eq!(
            schema_version(&connection)
                .map_err(|error| database_error("无法读取迁移后版本", error))?,
            SCHEMA_VERSION
        );
        drop(connection);
        drop(store);

        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理迁移测试数据库：{}", error))?;
        Ok(())
    }

    /// 外部实例可在当前工作目录和启动命令缺失时保存，但必须绑定有效 PID 与创建身份。
    #[test]
    fn external_instances_allow_incomplete_launch_fields_but_require_identity() -> Result<(), String>
    {
        let (directory, database_path) = temporary_database();
        let store = Store::open(&database_path)?;

        let mut external = external_config(1234, "creation-token-1", "外部服务");
        external.working_dir.clear();
        external.command.clear();
        let saved = store.save_service(external)?;
        assert_eq!(saved.source, "external");
        assert!(saved.working_dir.is_empty());
        assert!(saved.command.is_empty());
        let loaded = store.service(&saved.id)?;
        assert_eq!(
            loaded
                .external
                .as_ref()
                .map(|instance| instance.identity.pid),
            Some(1234)
        );

        let mut missing_identity = external_config(0, "creation-token-2", "无 PID");
        assert!(store.save_service(missing_identity.clone()).is_err());
        missing_identity.external.as_mut().unwrap().identity.pid = 1235;
        missing_identity
            .external
            .as_mut()
            .unwrap()
            .identity
            .created_at = "  ".into();
        assert!(store.save_service(missing_identity).is_err());

        let mut managed_with_external = service_config("", "启动配置");
        managed_with_external.external = Some(external_instance(1236, "creation-token-3"));
        assert!(store.save_service(managed_with_external).is_err());

        drop(store);
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理外部实例测试数据库：{}", error))?;
        Ok(())
    }

    /// 同一 PID 与创建身份只能绑定一个服务，更新自身记录则仍可成功。
    #[test]
    fn external_identity_cannot_be_attached_to_multiple_services() -> Result<(), String> {
        let (directory, database_path) = temporary_database();
        let store = Store::open(&database_path)?;

        let first = store.save_service(external_config(2345, "creation-token", "外部服务"))?;
        let mut updated = first.clone();
        updated.name = "更新后的外部服务".into();
        assert_eq!(store.save_service(updated)?.name, "更新后的外部服务");

        let duplicate = external_config(2345, "creation-token", "重复接入");
        assert!(store.save_service(duplicate).is_err());

        let different_generation = external_config(2345, "different-creation-token", "新实例");
        assert!(store.save_service(different_generation).is_ok());

        drop(store);
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理外部身份去重测试数据库：{}", error))?;
        Ok(())
    }

    /// 覆盖持久化往返、后端 ID/时间、已删 ID 不复活、默认 Agent 不重复补回及文档级联。
    #[test]
    fn round_trip_and_delete_cascade_match_store_contract() -> Result<(), String> {
        let (directory, database_path) = temporary_database();
        let working_dir = std::env::current_dir()
            .map_err(|error| format!("无法读取测试目录：{}", error))?
            .to_string_lossy()
            .into_owned();
        let store = Store::open(&database_path)?;

        assert!(store.services()?.is_empty());
        let defaults = store.agents()?;
        assert_eq!(defaults.len(), 9);
        let deleted_default = defaults[0].clone();
        store.delete_agent(&deleted_default.id)?;
        assert!(store.save_agent(deleted_default.clone()).is_err());

        let agent = store.save_agent(AgentProfile {
            id: String::new(),
            name: "自定义工具".into(),
            executable: "custom-cli".into(),
            args: vec!["--safe".into()],
            env: BTreeMap::from([("CUSTOM_MODE".into(), "1".into())]),
        })?;
        assert!(Uuid::parse_str(&agent.id).is_ok());
        let loaded_agent = store.agent(&agent.id)?;
        assert_eq!(loaded_agent.executable, "custom-cli");
        assert_eq!(loaded_agent.args, vec!["--safe"]);
        assert_eq!(
            loaded_agent.env.get("CUSTOM_MODE").map(String::as_str),
            Some("1")
        );

        let saved_service = store.save_service(service_config(&working_dir, "主服务"))?;
        assert!(Uuid::parse_str(&saved_service.id).is_ok());
        assert_ne!(saved_service.created_at, "由后端覆盖");
        assert_eq!(saved_service.expected_ports, vec![4317]);
        let mut updated_service = saved_service.clone();
        updated_service.name = "更新后的服务".into();
        updated_service.created_at = "不应覆盖".into();
        let updated_service = store.save_service(updated_service)?;
        assert_eq!(updated_service.id, saved_service.id);
        assert_eq!(updated_service.created_at, saved_service.created_at);
        let loaded_service = store.service(&saved_service.id)?;
        assert_eq!(loaded_service.name, "更新后的服务");
        assert_eq!(
            loaded_service.env.get("APP_MODE").map(String::as_str),
            Some("test")
        );

        let second_service = store.save_service(service_config(&working_dir, "另一个服务"))?;
        let document = store.save_document(ServiceDocument {
            id: String::new(),
            service_id: saved_service.id.clone(),
            title: "操作说明".into(),
            content: "# 使用方式\n\n本地文档内容。".into(),
            updated_at: "由后端覆盖".into(),
        })?;
        assert!(Uuid::parse_str(&document.id).is_ok());
        assert_eq!(store.document(&document.id)?.content, document.content);

        let mut moved_document = document.clone();
        moved_document.service_id = second_service.id.clone();
        assert!(store.save_document(moved_document).is_err());
        assert_eq!(store.documents(&saved_service.id)?.len(), 1);

        store.save_preferences(Preferences {
            theme: "dark".into(),
            ..Preferences::default()
        })?;
        store.delete_service(&saved_service.id)?;
        assert!(store.documents(&saved_service.id)?.is_empty());
        assert!(store.document(&document.id).is_err());
        assert!(store.save_service(updated_service).is_err());
        store.delete_service(&second_service.id)?;
        drop(store);

        let reopened = Store::open(&database_path)?;
        assert!(reopened.services()?.is_empty());
        assert!(reopened.agent(&deleted_default.id).is_err());
        assert_eq!(reopened.agents()?.len(), 9);
        assert_eq!(reopened.preferences()?.theme, "dark");
        drop(reopened);
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理测试数据库：{}", error))?;
        Ok(())
    }

    /// 终端外观与旧版偏好 JSON 均能稳定往返，边界值和颜色格式则在写入前拒绝。
    #[test]
    fn terminal_preferences_round_trip_and_validation() -> Result<(), String> {
        let (directory, database_path) = temporary_database();
        let store = Store::open(&database_path)?;

        let defaults = store.preferences()?;
        assert_eq!(defaults.theme, "system");
        assert_eq!(defaults.terminal.font_size, 13.0);
        assert_eq!(defaults.terminal.line_height, 1.45);
        assert_eq!(defaults.terminal.colors.bright_white, "#ffffff");

        let mut saved = Preferences::default();
        saved.theme = "dark".into();
        saved.terminal.theme = "dracula".into();
        saved.terminal.font_size = 16.0;
        saved.terminal.line_height = 1.6;
        saved.terminal.cursor_style = "bar".into();
        saved.terminal.cursor_blink = false;
        saved.terminal.colors.background = "#202124".into();
        store.save_preferences(saved.clone())?;
        let loaded = store.preferences()?;
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.terminal, saved.terminal);

        let mut invalid = saved.clone();
        invalid.terminal.font_size = f64::NAN;
        assert!(store.save_preferences(invalid).is_err());
        let mut invalid = saved.clone();
        invalid.terminal.line_height = 2.01;
        assert!(store.save_preferences(invalid).is_err());
        let mut invalid = saved.clone();
        invalid.terminal.colors.cursor = "#12345g".into();
        assert!(store.save_preferences(invalid).is_err());

        let legacy: Preferences = serde_json::from_str(r#"{"theme":"light"}"#)
            .map_err(|error| format!("旧版偏好 JSON 应可解析：{}", error))?;
        assert_eq!(legacy.theme, "light");
        assert_eq!(legacy.terminal, TerminalAppearance::default());

        drop(store);
        let reopened = Store::open(&database_path)?;
        assert_eq!(reopened.preferences()?.terminal, saved.terminal);
        drop(reopened);
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理终端偏好测试数据库：{}", error))?;
        Ok(())
    }

    /// v3 升级只补 catalog 的新增工具：旧三项不复活，路径和 Windows 扩展名去重生效，
    /// 且升级完成后删除预设不会在下一次打开数据库时再次插入。
    #[test]
    fn migrates_new_agent_catalog_once_without_resurrecting_deleted_entries() -> Result<(), String>
    {
        let (directory, database_path) = temporary_database();
        let connection = Connection::open(&database_path).map_err(|error| error.to_string())?;
        connection
            .execute_batch(
                "CREATE TABLE agents (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    executable TEXT NOT NULL,
                    args_json TEXT NOT NULL,
                    env_json TEXT NOT NULL
                );
                CREATE TABLE preferences (
                    singleton_id INTEGER PRIMARY KEY NOT NULL CHECK (singleton_id = 1),
                    theme TEXT NOT NULL
                );
                CREATE TABLE services (
                    id TEXT PRIMARY KEY NOT NULL,
                    name TEXT NOT NULL,
                    description TEXT NOT NULL,
                    notes TEXT NOT NULL,
                    created_at TEXT NOT NULL,
                    updated_at TEXT NOT NULL,
                    kind TEXT NOT NULL
                );
                PRAGMA user_version = 3;",
            )
            .map_err(|error| error.to_string())?;
        connection
            .execute(
                "INSERT INTO agents (id, name, executable, args_json, env_json)
                 VALUES (?1, ?2, ?3, '[]', '{}')",
                params![
                    "existing-opencode",
                    "私有 OpenCode",
                    r"C:\Tools\OPENCODE.CMD"
                ],
            )
            .map_err(|error| error.to_string())?;
        drop(connection);

        let store = Store::open(&database_path)?;
        let agents = store.agents()?;
        // 现有 opencode 加上 catalog 中另外五项新增工具；旧三项不因迁移重新出现。
        assert_eq!(agents.len(), 6);
        assert!(agents.iter().any(|agent| agent.id == "existing-opencode"));
        assert!(!agents.iter().any(|agent| {
            matches!(agent.name.as_str(), "Codex" | "Claude Code" | "Gemini CLI")
        }));
        assert!(agents.iter().any(|agent| agent.id == "hermes"));

        store.delete_agent("hermes")?;
        assert!(store.agent("hermes").is_err());
        drop(store);

        let reopened = Store::open(&database_path)?;
        assert_eq!(reopened.agents()?.len(), 5);
        assert!(reopened.agent("hermes").is_err());
        drop(reopened);
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理 Agent 迁移测试数据库：{}", error))?;
        Ok(())
    }

    /// 内容表独立于服务和文档：删除收藏只解除自身关联，Steam URI 及内容偏好也能往返。
    #[test]
    fn content_schema_round_trip_delete_and_identity_deduplication() -> Result<(), String> {
        let (directory, database_path) = temporary_database();
        let store = Store::open(&database_path)?;
        let folder = directory.to_string_lossy().into_owned();
        let service = store.save_service(ServiceConfig {
            id: String::new(),
            kind: "desktopApp".into(),
            launch: LaunchConfig::DesktopApp {
                path: folder.clone(),
                args: vec![],
                start_dir: String::new(),
            },
            source: "managed".into(),
            external: None,
            name: "桌面应用".into(),
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
        })?;
        let document = store.save_document(ServiceDocument {
            id: String::new(),
            service_id: service.id.clone(),
            title: "保留文档".into(),
            content: "原服务文档".into(),
            updated_at: String::new(),
        })?;
        let item = ContentItem::new(
            "关联收藏",
            ContentKind::Application,
            ContentTarget::ServiceLink {
                service_id: service.id.clone(),
            },
        );
        let item = store.save_content_item(item)?;
        assert_eq!(store.content_items()?.len(), 1);
        let steam = store.save_content_item(ContentItem::new(
            "游戏",
            ContentKind::Game,
            ContentTarget::Steam {
                app_id: "steam://rungameid/730".into(),
            },
        ))?;
        assert_eq!(
            steam.target.canonical_uri().as_deref(),
            Some("steam://run/730")
        );
        assert_eq!(store.content_preferences()?, serde_json::json!({}));
        store.save_content_preferences(serde_json::json!({
            "agentId": "codex",
            "voiceEnabled": true
        }))?;
        assert_eq!(store.content_preferences()?["agentId"], "codex");

        let identity = ProcessIdentity {
            pid: 7788,
            created_at: "generation-a".into(),
        };
        let local = store.save_content_item(ContentItem::new(
            "本地应用",
            ContentKind::Game,
            ContentTarget::LocalApp {
                path: folder.clone(),
                args: vec![],
                start_dir: String::new(),
            },
        ))?;
        store.bind_content_application(
            &local.id,
            &ContentApplicationBinding {
                identity: identity.clone(),
                target_path: folder.clone(),
                bound_at: timestamp_now(),
            },
        )?;
        let duplicate = store.save_content_item(ContentItem::new(
            "重复应用",
            ContentKind::Application,
            ContentTarget::LocalApp {
                path: folder.clone(),
                args: vec![],
                start_dir: String::new(),
            },
        ))?;
        assert!(store
            .bind_content_application(
                &duplicate.id,
                &ContentApplicationBinding {
                    identity,
                    target_path: folder.clone(),
                    bound_at: timestamp_now(),
                },
            )
            .is_err());
        store.delete_content_item(&item.id)?;
        assert_eq!(store.documents(&service.id)?[0].id, document.id);
        assert!(store.service(&service.id).is_ok());
        // 重复实例接管被拒绝，但用户创建的独立收藏记录仍保留，不能顺带删除。
        assert_eq!(store.content_items()?.len(), 3);
        assert!(store.content_application_binding(&duplicate.id)?.is_none());
        let connection = store.connection()?;
        assert_eq!(
            schema_version(&connection).map_err(|e| e.to_string())?,
            SCHEMA_VERSION
        );
        drop(connection);
        drop(store);
        std::fs::remove_dir_all(directory)
            .map_err(|error| format!("无法清理内容库测试数据库：{error}"))?;
        Ok(())
    }
}
