//! 外部监听进程发现与单进程控制。
//!
//! 本模块将本机 TCP Listen、UDP 绑定端口和端口所有者合并成只读快照；所有扫描共用
//! 同一个 2 秒缓存与刷新锁，运行时端口轮询也能复用原始 socket 结果。外部停止操作
//! 会重新读取原生进程创建身份，避免只凭 PID 对已经复用的进程号执行信号或终止。

mod grouping;
mod process;
#[cfg(windows)]
mod windows;

use crate::models::{
    DiscoveredProcess, DiscoverySnapshot, ExternalInstance, PortInfo, ProcessIdentity,
};
use chrono::{SecondsFormat, Utc};
use grouping::{family_identity_by_pid, InterpreterFamilySource, ProcessFamilyCandidate};
use netstat2::{get_sockets_info, AddressFamilyFlags, ProtocolFlags, ProtocolSocketInfo, TcpState};
use process::NativeProcessInfo;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::sync::OnceLock;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System, UpdateKind};

const CACHE_TTL: Duration = Duration::from_secs(2);
const MAX_ANCESTOR_DEPTH: usize = 128;

/// Windows Service Control Manager 对某 PID 的只读登记结果。
#[derive(Clone, Debug, Default)]
struct ServiceHostInfo {
    service_names: Vec<String>,
    shared: bool,
}

#[derive(Clone, Debug, Default)]
struct ProcessMetadata {
    name: String,
    executable: Option<String>,
    working_dir: Option<String>,
    command: Vec<String>,
}

struct CacheState {
    snapshot: DiscoverySnapshot,
    snapshot_attempted_at: Option<Instant>,
    raw_ports: Vec<PortInfo>,
    /// 原始 socket owner 在扫描时的创建身份，用来防止缓存命中时 PID 已经被复用。
    raw_port_identities: HashMap<u32, String>,
    ports_attempted_at: Option<Instant>,
    ports_error: Option<String>,
}

/// 共享本机进程和端口缓存的发现器。
///
/// `refresh_lock` 覆盖 socket 扫描与进程拼装全过程；并发调用不会为同一轮刷新重复
/// 执行 netstat 或并行读取大量进程元数据。缓存失败时保留上次完整快照，并明确标记
/// `stale`，让调用端继续显示最后一次可信结果。
pub struct DiscoveryManager {
    state: Mutex<CacheState>,
    refresh_lock: Mutex<()>,
}

impl DiscoveryManager {
    /// 创建可被运行时、Tauri 命令和监控器共同持有的发现器。
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CacheState {
                snapshot: DiscoverySnapshot {
                    revision: 0,
                    updated_at: None,
                    stale: false,
                    error: None,
                    processes: Vec::new(),
                    unattributed_ports: Vec::new(),
                },
                snapshot_attempted_at: None,
                raw_ports: Vec::new(),
                raw_port_identities: HashMap::new(),
                ports_attempted_at: None,
                ports_error: None,
            }),
            refresh_lock: Mutex::new(()),
        })
    }

    /// 取得进程与端口快照。成功扫描每 2 秒最多执行一次；失败会保留旧数据并设置 stale。
    pub fn snapshot(&self, force: bool) -> DiscoverySnapshot {
        let _refresh = lock_recover(&self.refresh_lock);
        {
            let state = lock_recover(&self.state);
            if !force
                && state
                    .snapshot_attempted_at
                    .map(|at| at.elapsed() < CACHE_TTL)
                    .unwrap_or(false)
            {
                return state.snapshot.clone();
            }
        }

        let ports = match self.cached_ports_locked(force) {
            Ok(ports) => ports,
            Err(error) => return self.mark_snapshot_stale(error),
        };
        let owner_identities = lock_recover(&self.state).raw_port_identities.clone();

        match build_snapshot(ports, &owner_identities) {
            Ok((processes, unattributed_ports)) => {
                let now = Instant::now();
                let updated_at = Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
                let mut state = lock_recover(&self.state);
                let revision = state.snapshot.revision.saturating_add(1);
                state.snapshot = DiscoverySnapshot {
                    revision,
                    updated_at: Some(updated_at),
                    stale: false,
                    error: None,
                    processes,
                    unattributed_ports,
                };
                state.snapshot_attempted_at = Some(now);
                state.snapshot.clone()
            }
            Err(error) => self.mark_snapshot_stale(error),
        }
    }

    /// 取得最近一次 TCP Listen/UDP 绑定结果，并按每个 owner PID 分别展开 socket。
    ///
    /// 无法归属到 PID 的 socket 保留 `pid: None`；其他调用方可以直接复用这份缓存，
    /// 不必各自重新枚举端口。强制刷新失败会返回错误，但不会清除上一份缓存。
    pub fn ports(&self, force: bool) -> Result<Vec<PortInfo>, String> {
        let _refresh = lock_recover(&self.refresh_lock);
        self.cached_ports_locked(force)
    }

    /// 按 PID 与高精度创建时间重新检查一个进程；退出或 PID 已复用时返回 `None`。
    pub fn inspect(&self, identity: &ProcessIdentity) -> Result<Option<ExternalInstance>, String> {
        if identity.pid == 0 || identity.created_at.trim().is_empty() {
            return Err("进程身份信息不足，已拒绝接入。".into());
        }

        let native = match process::query_process(identity.pid)? {
            Some(native) => native,
            None => return Ok(None),
        };
        if native.created_at != identity.created_at {
            return Ok(None);
        }

        let system = refreshed_system(&[identity.pid])?;
        let protected_pids = protected_ancestor_pids(&system);
        let service_result = service_hosts();
        let service_error = service_result.as_ref().err().cloned();
        let service_hosts = service_result.unwrap_or_default();
        let metadata = process_metadata(&system, identity.pid);
        let instance = build_instance(
            identity.pid,
            Some(native),
            metadata.as_ref(),
            service_hosts.get(&identity.pid),
            service_error.as_deref(),
            &protected_pids,
        );
        // 元数据和签名判定完成后再检查一次，避免把中途复用的 PID 当成最初请求的进程。
        let Some(current) = process::query_process(identity.pid)? else {
            return Ok(None);
        };
        if current.created_at != identity.created_at {
            return Ok(None);
        }
        Ok(Some(instance))
    }

    /// 只终止一个已核验的外部 PID；不会向子进程或进程组广播信号，也不会自动提权。
    ///
    /// 分类保护（例如 system/unknown 需由调用方根据当前实例解锁状态判断）留给上层；
    /// 本层仍会重新核对创建身份、共享宿主和本程序自身及其祖先进程。
    pub fn terminate(&self, identity: &ProcessIdentity) -> Result<(), String> {
        if identity.pid == 0 || identity.created_at.trim().is_empty() {
            return Err("进程身份信息不足，已拒绝停止。".into());
        }

        let instance = self
            .inspect(identity)?
            .ok_or_else(|| "进程已退出或 PID 已对应到其他进程，已拒绝停止。".to_owned())?;
        if instance.shared_host {
            return Err("该进程是共享或潜在共享宿主，当前只允许查看。".into());
        }

        if !instance.controllable {
            return Err(instance
                .control_reason
                .unwrap_or_else(|| "该进程当前不可停止。".into()));
        }

        // 平台实现会在实际发信号/调用 TerminateProcess 前再次核对同一创建身份。
        process::terminate(identity)
    }

    fn cached_ports_locked(&self, force: bool) -> Result<Vec<PortInfo>, String> {
        let cached = {
            let state = lock_recover(&self.state);
            if !force
                && state
                    .ports_attempted_at
                    .map(|at| at.elapsed() < CACHE_TTL)
                    .unwrap_or(false)
            {
                Some((
                    state.raw_ports.clone(),
                    state.raw_port_identities.clone(),
                    state.ports_error.clone(),
                ))
            } else {
                None
            }
        };
        if let Some((ports, identities, error)) = cached {
            return match error {
                Some(error) => Err(error),
                None => Ok(validate_cached_ports(&ports, &identities)),
            };
        }

        let result = scan_bound_ports();
        let mut state = lock_recover(&self.state);
        state.ports_attempted_at = Some(Instant::now());
        match result {
            Ok(scan) => {
                state.raw_ports = scan.ports;
                state.raw_port_identities = scan.owner_identities;
                state.ports_error = None;
                Ok(validate_cached_ports(
                    &state.raw_ports,
                    &state.raw_port_identities,
                ))
            }
            Err(error) => {
                state.ports_error = Some(error.clone());
                Err(error)
            }
        }
    }

    fn mark_snapshot_stale(&self, error: String) -> DiscoverySnapshot {
        let mut state = lock_recover(&self.state);
        state.snapshot_attempted_at = Some(Instant::now());
        state.snapshot.stale = true;
        state.snapshot.error = Some(error);
        state.snapshot.clone()
    }
}

/// 获取缓存锁时恢复 poisoned 状态；快照本身始终由单个锁保护，不会部分写入。
fn lock_recover<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|error| error.into_inner())
}

/// 扫描操作系统的 socket 表，只保留对服务发现有意义的 TCP Listen 和 UDP 绑定记录。
struct RawPortScan {
    ports: Vec<PortInfo>,
    owner_identities: HashMap<u32, String>,
}

/// `build_snapshot` 的中间结果；家族身份仅用于展示，在单 PID 控制逻辑之外计算。
struct PendingDiscoveredProcess {
    instance: ExternalInstance,
    ports: Vec<PortInfo>,
    /// 同一 System 刷新样本或 native process query 确认的直接父 PID。
    parent_pid: Option<u32>,
    /// native 查询提供的父 PID；存在时在最终复核中要求它保持不变。
    native_parent_pid: Option<u32>,
    executable_key: Option<String>,
    is_interpreter: bool,
    interpreter_source: Option<InterpreterFamilySource>,
}

/// 在 socket 扫描前后读取 native identity，确保端口从未被归给刚复用的 PID。
fn scan_bound_ports() -> Result<RawPortScan, String> {
    // 先取一份候选 owner 列表，只为这些 PID 读取廉价的原生创建身份；这一步不碰命令行、
    // cwd、environment 或签名。随后再取真正要缓存的 socket 样本，避免把样本之后才查询到
    // 的新 PID 创建时间错当成 socket 建立时的 owner 身份。
    let candidates = read_bound_ports()?;
    let candidate_pids = candidates
        .iter()
        .filter_map(|port| port.pid)
        .collect::<HashSet<_>>();
    let identities_before = candidate_pids
        .into_iter()
        .filter_map(|pid| {
            process::query_process(pid)
                .ok()
                .flatten()
                .filter(|native| !native.created_at.is_empty())
                .map(|native| (pid, native.created_at))
        })
        .collect::<HashMap<_, _>>();

    // 使用第二次 socket 枚举作为本轮实际结果。候选样本里没有的新 owner 没有扫描前身份，
    // 本轮只作为未归属端口显示，下一轮重新发现，绝不会猜测它的创建时间。
    let mut ports = read_bound_ports()?;
    let scanned_pids = ports
        .iter()
        .filter_map(|port| port.pid)
        .collect::<HashSet<_>>();
    let mut owner_identities = HashMap::new();
    let mut verified = HashMap::<u32, bool>::new();
    for pid in scanned_pids {
        let same_identity = identities_before.get(&pid).is_some_and(|before| {
            process::query_process(pid)
                .ok()
                .flatten()
                .map(|after| !after.created_at.is_empty() && after.created_at == *before)
                .unwrap_or(false)
        });
        verified.insert(pid, same_identity);
        if same_identity {
            if let Some(identity) = identities_before.get(&pid) {
                owner_identities.insert(pid, identity.clone());
            }
        }
    }
    for port in &mut ports {
        if port
            .pid
            .is_some_and(|pid| !verified.get(&pid).copied().unwrap_or(false))
        {
            // 创建身份缺失、新 PID 或扫描期间发生复用：保留 socket 本身，只移除不可信归属。
            port.pid = None;
        }
    }
    sort_ports(&mut ports);
    Ok(RawPortScan {
        ports,
        owner_identities,
    })
}

/// 枚举 TCP Listen 和 UDP 绑定；多个 owner 会逐项展开，没有 owner 的 socket 保留空 PID。
fn read_bound_ports() -> Result<Vec<PortInfo>, String> {
    let sockets = get_sockets_info(
        AddressFamilyFlags::IPV4 | AddressFamilyFlags::IPV6,
        ProtocolFlags::TCP | ProtocolFlags::UDP,
    )
    .map_err(|error| format!("读取本机 TCP Listen/UDP 绑定信息失败：{error}"))?;

    let mut ports = HashSet::new();
    for socket in sockets {
        let (address, port, protocol, is_bound) = match socket.protocol_socket_info {
            ProtocolSocketInfo::Tcp(info) => (
                info.local_addr.to_string(),
                info.local_port,
                "TCP",
                info.state == TcpState::Listen,
            ),
            ProtocolSocketInfo::Udp(info) => (
                info.local_addr.to_string(),
                info.local_port,
                "UDP",
                info.local_port != 0,
            ),
        };
        if !is_bound || port == 0 {
            continue;
        }

        // 一个内核 socket 可能同时归属多个继承或共享句柄的进程，逐个 owner 展开，
        // 不能只拿 associated_pids 的第一项，否则运行时会漏掉真实持有者。
        let owners = socket.associated_pids.into_iter().collect::<HashSet<_>>();
        if owners.is_empty() {
            ports.insert(PortInfo {
                address,
                port,
                protocol: protocol.into(),
                pid: None,
            });
        } else {
            for pid in owners {
                ports.insert(PortInfo {
                    address: address.clone(),
                    port,
                    protocol: protocol.into(),
                    pid: Some(pid),
                });
            }
        }
    }

    let mut ports = ports.into_iter().collect::<Vec<_>>();
    sort_ports(&mut ports);
    Ok(ports)
}

/// 缓存命中仍核对 socket owner 的原生创建身份；失效 owner 降为未归属，绝不挂到新进程。
fn validate_cached_ports(
    ports: &[PortInfo],
    owner_identities: &HashMap<u32, String>,
) -> Vec<PortInfo> {
    let mut verified = HashMap::<u32, bool>::new();
    let mut result = HashSet::new();
    for port in ports {
        let Some(pid) = port.pid else {
            result.insert(port.clone());
            continue;
        };
        let same = *verified.entry(pid).or_insert_with(|| {
            let Some(expected) = owner_identities.get(&pid) else {
                return false;
            };
            process::query_process(pid)
                .ok()
                .flatten()
                .map(|current| current.created_at == *expected)
                .unwrap_or(false)
        });
        if same {
            result.insert(port.clone());
        } else {
            result.insert(PortInfo {
                pid: None,
                ..port.clone()
            });
        }
    }
    let mut result = result.into_iter().collect::<Vec<_>>();
    sort_ports(&mut result);
    result
}

/// 把带 owner 的端口按进程聚合，同时为无法归属 PID 的 socket 单独保留只读记录。
fn build_snapshot(
    raw_ports: Vec<PortInfo>,
    owner_identities: &HashMap<u32, String>,
) -> Result<(Vec<DiscoveredProcess>, Vec<PortInfo>), String> {
    let mut ports_by_pid = HashMap::<u32, Vec<PortInfo>>::new();
    let mut unattributed_ports = Vec::new();
    let mut owner_validation = HashMap::<u32, bool>::new();
    for port in raw_ports {
        match port.pid {
            Some(pid) => {
                let still_same = *owner_validation.entry(pid).or_insert_with(|| {
                    owner_identities
                        .get(&pid)
                        .and_then(|expected| {
                            process::query_process(pid)
                                .ok()
                                .flatten()
                                .map(|current| current.created_at == *expected)
                        })
                        .unwrap_or(false)
                });
                if still_same {
                    ports_by_pid.entry(pid).or_default().push(port);
                } else {
                    // 原生身份缺失或 PID 已变化时，不把 socket 错挂到新进程名下。
                    unattributed_ports.push(PortInfo { pid: None, ..port });
                }
            }
            None => unattributed_ports.push(port),
        }
    }

    let owner_pids = ports_by_pid.keys().copied().collect::<Vec<_>>();
    let system = refreshed_system(&owner_pids)?;
    let protected_pids = protected_ancestor_pids(&system);
    let service_result = service_hosts();
    let service_error = service_result.as_ref().err().cloned();
    let service_hosts = service_result.unwrap_or_default();

    let mut pending_processes = Vec::with_capacity(ports_by_pid.len());
    for (pid, mut ports) in ports_by_pid {
        sort_ports(&mut ports);
        let native = process::query_process(pid).ok().flatten();
        let expected_identity = owner_identities.get(&pid);
        let Some(native) = native.filter(|process| {
            expected_identity
                .map(|created_at| process.created_at == *created_at)
                .unwrap_or(false)
        }) else {
            unattributed_ports.extend(ports.into_iter().map(|port| PortInfo { pid: None, ..port }));
            continue;
        };
        let metadata = process_metadata(&system, pid);
        // Linux/macOS 的原生身份查询直接返回父 PID；Windows 的原生身份 API 不提供父 PID，
        // 因此使用同一轮 sysinfo 进程表中的关系。若原生父 PID 缺失，不用 sysinfo 猜测补齐。
        #[cfg(windows)]
        let system_parent_pid = system
            .process(Pid::from_u32(pid))
            .and_then(|process| process.parent())
            .map(|parent| parent.as_u32());
        #[cfg(windows)]
        let parent_pid = system_parent_pid;
        #[cfg(not(windows))]
        let parent_pid = native.parent_pid;
        let native_parent_pid = native.parent_pid;
        let instance = build_instance(
            pid,
            Some(native.clone()),
            metadata.as_ref(),
            service_hosts.get(&pid),
            service_error.as_deref(),
            &protected_pids,
        );
        // 分类/签名验证可能稍慢，结束前再次确认创建身份；native 可提供父 PID 的平台还
        // 要求它没有变化。无法确认时端口退回未归属，不让不稳定节点进入展示家族。
        let still_same = process::query_process(pid)
            .ok()
            .flatten()
            .map(|current| {
                current.created_at == native.created_at
                    && native_parent_pid
                        .map(|parent_pid| current.parent_pid == Some(parent_pid))
                        .unwrap_or(true)
            })
            .unwrap_or(false);
        if !still_same {
            unattributed_ports.extend(ports.into_iter().map(|port| PortInfo { pid: None, ..port }));
            continue;
        }
        let executable_key = instance
            .executable
            .as_deref()
            .and_then(normalized_path_key_from_str);
        let is_interpreter = instance
            .executable
            .as_deref()
            .is_some_and(is_script_interpreter);
        let interpreter_source = if is_interpreter {
            instance
                .executable
                .as_deref()
                .and_then(|executable| interpreter_family_source(executable, metadata.as_ref()))
        } else {
            None
        };
        pending_processes.push(PendingDiscoveredProcess {
            instance,
            ports,
            parent_pid,
            native_parent_pid,
            executable_key,
            is_interpreter,
            interpreter_source,
        });
    }

    // 所有 owner 元数据处理完成后再做一次原生身份复核。已退出、权限不足或 PID 已复用的
    // 节点不会作为父或子参与分组；Windows 的父关系来自前述 sysinfo 样本，但父子两端仍
    // 都必须在这次最终核验中保持同一原生创建身份。
    let mut final_native = HashMap::with_capacity(pending_processes.len());
    for pending in &pending_processes {
        let pid = pending.instance.identity.pid;
        let expected_created_at = pending.instance.identity.created_at.as_str();
        let current = process::query_process(pid)
            .ok()
            .flatten()
            .filter(|current| {
                !expected_created_at.is_empty()
                    && current.created_at == expected_created_at
                    && pending
                        .native_parent_pid
                        .map(|parent_pid| current.parent_pid == Some(parent_pid))
                        .unwrap_or(true)
            });
        final_native.insert(pid, current);
    }

    let pending_by_pid = pending_processes
        .iter()
        .map(|pending| (pending.instance.identity.pid, pending))
        .collect::<HashMap<_, _>>();
    let mut family_candidates = Vec::with_capacity(pending_processes.len());
    for pending in &pending_processes {
        let pid = pending.instance.identity.pid;
        let Some(current) = final_native.get(&pid).and_then(Option::as_ref) else {
            continue;
        };
        if owner_identities.get(&pid) != Some(&current.created_at) {
            continue;
        }

        // 即使父 PID 来源是 sysinfo，仍需确认它本身也属于本轮 socket owner，并在最终
        // 原生复核中保持同一创建身份。父进程缺失、不可访问或已退出时不留父子边。
        let verified_parent_pid = pending.parent_pid.filter(|parent_pid| {
            let Some(parent) = pending_by_pid.get(parent_pid) else {
                return false;
            };
            let Some(parent_native) = final_native.get(parent_pid).and_then(Option::as_ref) else {
                return false;
            };
            owner_identities.get(parent_pid) == Some(&parent_native.created_at)
                && parent.instance.identity.created_at == parent_native.created_at
        });

        family_candidates.push(ProcessFamilyCandidate {
            identity: pending.instance.identity.clone(),
            parent_pid: verified_parent_pid,
            executable_key: pending.executable_key.clone(),
            category: pending.instance.category.clone(),
            shared_host: pending.instance.shared_host,
            is_interpreter: pending.is_interpreter,
            interpreter: pending.interpreter_source.clone(),
        });
    }
    let family_identities = family_identity_by_pid(&family_candidates);

    let mut processes = pending_processes
        .into_iter()
        .map(|pending| {
            let pid = pending.instance.identity.pid;
            DiscoveredProcess {
                family_identity: family_identities.get(&pid).cloned(),
                instance: pending.instance,
                ports: pending.ports,
                // 该发现器不读取应用配置仓库，也不会把系统服务名冒充为用户管理的服务 ID。
                managed_service_id: None,
            }
        })
        .collect::<Vec<_>>();

    processes.sort_by(|left, right| {
        left.instance
            .name
            .to_lowercase()
            .cmp(&right.instance.name.to_lowercase())
            .then_with(|| left.instance.identity.pid.cmp(&right.instance.identity.pid))
    });
    sort_ports(&mut unattributed_ports);
    Ok((processes, unattributed_ports))
}

/// 更新 sysinfo 进程元数据；原生创建身份仍由平台模块读取，不使用 sysinfo 的秒级启动时间。
fn refreshed_system(metadata_pids: &[u32]) -> Result<System, String> {
    let mut system = System::new();
    // 进程列表只需 PID、父 PID、名称与可执行文件；不采集 CPU/内存/磁盘，也不枚举线程。
    system.refresh_processes_specifics(
        ProcessesToUpdate::All,
        true,
        ProcessRefreshKind::nothing()
            .without_tasks()
            .with_exe(UpdateKind::Always),
    );
    if system.processes().is_empty() {
        return Err("系统未返回可检查的进程清单，已保留上一次发现快照。".into());
    }
    if !metadata_pids.is_empty() {
        let target_pids = metadata_pids
            .iter()
            .copied()
            .map(Pid::from_u32)
            .collect::<Vec<_>>();
        // cmd/cwd 只对 socket owner 候选 PID 读取以判断脚本归属；绝不请求 environ。
        system.refresh_processes_specifics(
            ProcessesToUpdate::Some(&target_pids),
            true,
            ProcessRefreshKind::nothing()
                .without_tasks()
                .with_cmd(UpdateKind::Always)
                .with_cwd(UpdateKind::Always)
                .with_exe(UpdateKind::Always),
        );
    }
    Ok(system)
}

/// 读取进程的显示元数据；`cmd` 仅用于本地判断解释器的脚本来源，绝不读取 environ。
fn process_metadata(system: &System, pid: u32) -> Option<ProcessMetadata> {
    let process = system.process(Pid::from_u32(pid))?;
    Some(ProcessMetadata {
        name: process.name().to_string_lossy().into_owned(),
        executable: process
            .exe()
            .map(|path| path.to_string_lossy().into_owned()),
        working_dir: process
            .cwd()
            .map(|path| path.to_string_lossy().into_owned()),
        command: process
            .cmd()
            .iter()
            .map(|part| part.to_string_lossy().into_owned())
            .collect(),
    })
}

/// 从当前进程向上走父链，生成不可由外部控制的 PID 集合。
fn protected_ancestor_pids(system: &System) -> HashSet<u32> {
    let mut protected = HashSet::new();
    let mut current = Pid::from_u32(std::process::id());
    for _ in 0..MAX_ANCESTOR_DEPTH {
        if !protected.insert(current.as_u32()) {
            break;
        }
        let Some(parent) = system.process(current).and_then(|process| process.parent()) else {
            break;
        };
        current = parent;
    }
    protected
}

/// 按身份、可信路径、脚本来源和平台服务登记构造可显示的外部进程记录。
fn build_instance(
    pid: u32,
    native: Option<NativeProcessInfo>,
    metadata: Option<&ProcessMetadata>,
    service: Option<&ServiceHostInfo>,
    service_error: Option<&str>,
    protected_pids: &HashSet<u32>,
) -> ExternalInstance {
    let created_at = native
        .as_ref()
        .map(|process| process.created_at.clone())
        .unwrap_or_default();
    let executable = native
        .as_ref()
        .and_then(|process| process.executable.clone())
        .or_else(|| metadata.and_then(|process| process.executable.clone()));
    let working_dir = metadata.and_then(|process| process.working_dir.clone());
    let name = metadata
        .map(|process| process.name.trim())
        .filter(|name| !name.is_empty())
        .map(str::to_owned)
        .or_else(|| {
            executable.as_deref().and_then(|path| {
                Path::new(path)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
        })
        .unwrap_or_else(|| format!("未知进程 {pid}"));
    let shared_host = service.map(|info| info.shared).unwrap_or(false)
        || is_windows_service_host(&name, executable.as_deref());

    let mut reasons = Vec::new();
    let category = if created_at.is_empty() {
        reasons.push("无法读取精确进程创建身份，只允许查看端口占用。".into());
        if let Some(error) = service_error.filter(|_| shared_host) {
            reasons.push(format!("Windows 服务登记读取失败：{error}"));
        }
        "unknown".to_owned()
    } else if shared_host {
        // svchost.exe 自身的 Microsoft 签名不能证明其承载的服务 DLL 都属于系统；在
        // 没有逐项验证宿主模块来源前，整个共享宿主归为 unknown 并维持只读。
        reasons.push(
            "共享宿主承载模块来源未逐项确认，可能混合系统组件和第三方组件，按未知进程保护。".into(),
        );
        if let Some(error) = service_error {
            reasons.push(format!("Windows 服务登记读取失败：{error}"));
        }
        "unknown".to_owned()
    } else {
        let classification = classify_process(executable.as_deref(), metadata, service);
        reasons.extend(classification.reasons);
        classification.category
    };

    if let Some(service) = service.filter(|info| !info.service_names.is_empty()) {
        reasons.push(format!(
            "平台登记的 Windows 服务：{}。",
            service.service_names.join("、")
        ));
    }
    if shared_host {
        reasons.push("该进程是共享或潜在共享宿主，发现器只允许查看，不允许停止。".into());
        if service
            .map(|info| info.service_names.is_empty())
            .unwrap_or(true)
        {
            reasons.push("无法唯一映射该宿主的服务项目，按只读进程处理。".into());
        }
    }

    let identity = ProcessIdentity { pid, created_at };
    let control_reason = if identity.created_at.is_empty() {
        Some("无法核验 PID 创建身份，已拒绝接入和停止。".into())
    } else if protected_pids.contains(&pid) {
        Some("当前程序及其祖先进程不可停止。".into())
    } else if shared_host {
        Some("共享服务宿主只读，不能停止单个宿主进程。".into())
    } else {
        match process::can_terminate(&identity) {
            Ok(true) => None,
            Ok(false) => Some("当前用户没有停止该进程所需的权限，且不会自动提权。".into()),
            Err(error) => Some(error),
        }
    };

    ExternalInstance {
        identity,
        name,
        executable,
        working_dir,
        category,
        reasons,
        shared_host,
        controllable: control_reason.is_none(),
        control_reason,
    }
}

struct Classification {
    category: String,
    reasons: Vec<String>,
}

/// 优先按解释器实际脚本来源分类；普通程序则由受保护系统路径和平台可信证据判断。
fn classify_process(
    executable: Option<&str>,
    metadata: Option<&ProcessMetadata>,
    service: Option<&ServiceHostInfo>,
) -> Classification {
    let Some(executable) = executable.filter(|path| !path.trim().is_empty()) else {
        return classification("unknown", "无法取得可执行文件路径，无法可靠判断系统归属。");
    };

    if is_script_interpreter(executable) {
        return match script_source(executable, metadata) {
            ScriptSource::External => classification(
                "nonSystem",
                "该进程由解释器执行来源明确的用户脚本，按非系统进程处理。",
            ),
            ScriptSource::TrustedSystem => classification(
                "system",
                "脚本位于平台受保护系统目录并通过签名验证，按系统进程处理。",
            ),
            ScriptSource::Unknown => classification(
                "unknown",
                "检测到脚本解释器，但无法确认脚本来源，按未知进程保护。",
            ),
        };
    }

    // 只有文件实际存在且能解析为稳定路径时，路径规则和签名规则才可作为证据；
    // canonicalize 失败的 deleted、无权限或伪造路径一律归 unknown。
    let canonical_executable = match Path::new(executable).canonicalize() {
        Ok(path) => path,
        Err(_) => {
            return classification(
                "unknown",
                "无法规范化可执行文件路径，缺少可靠的平台组件证据，按未知进程保护。",
            )
        }
    };

    #[cfg(windows)]
    {
        let in_system_tree = windows::is_protected_system_path(&canonical_executable);
        if in_system_tree {
            return match windows::authenticode_trusted(&canonical_executable) {
                Some(true) => classification(
                    "system",
                    if service
                        .map(|info| !info.service_names.is_empty())
                        .unwrap_or(false)
                    {
                        "可执行文件位于 Windows 系统目录、签名可信，并有系统服务登记。"
                    } else {
                        "可执行文件位于 Windows 系统目录，且签名发布者为 Microsoft Windows。"
                    },
                ),
                Some(false) => classification(
                    "unknown",
                    "可执行文件位于 Windows 系统目录，但签名验证未通过；默认保护。",
                ),
                None => classification("unknown", "无法读取可执行文件签名证据；默认保护。"),
            };
        }
        return classification("nonSystem", "可执行文件路径不在 Windows 受保护系统目录中。");
    }

    #[cfg(target_os = "macos")]
    {
        let _ = service;
        if is_macos_system_path(&canonical_executable) {
            return match macos_apple_system_signature(&canonical_executable) {
                Some(true) => classification(
                    "system",
                    "可执行文件位于 macOS 系统组件目录，且通过 Apple 系统签名验证。",
                ),
                Some(false) => classification(
                    "unknown",
                    "可执行文件位于 macOS 系统组件目录，但未通过 Apple 签名验证；默认保护。",
                ),
                None => classification("unknown", "无法读取 macOS Apple 系统签名证据；默认保护。"),
            };
        }
        return classification(
            "nonSystem",
            "可执行文件路径不在 macOS 受保护系统组件目录中。",
        );
    }

    #[cfg(target_os = "linux")]
    {
        let _ = service;
        if is_linux_system_path(&canonical_executable) {
            return classification(
                "unknown",
                "可执行文件位于 Linux 系统目录，但该路径也安装第三方软件，缺少 OS 内建签名证据。",
            );
        }
        return classification(
            "nonSystem",
            "可执行文件路径不在 Linux 标准系统目录中，按非系统进程处理。",
        );
    }

    #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
    {
        let _ = service;
        classification("unknown", "当前平台没有可用的系统组件可信判定规则。")
    }
}

/// 统一生成带单条中文解释的分类结果，保证 unknown 与系统进程一样默认可保护。
fn classification(category: &str, reason: &str) -> Classification {
    Classification {
        category: category.into(),
        reasons: vec![reason.into()],
    }
}

enum ScriptSource {
    External,
    TrustedSystem,
    Unknown,
}

/// 识别常见解释器，避免把 `/usr/bin/python`、Node 或 shell 本身误认成系统服务。
fn is_script_interpreter(executable: &str) -> bool {
    let Some(file_name) = Path::new(executable).file_name() else {
        return false;
    };
    let name = file_name.to_string_lossy().to_ascii_lowercase();
    let name = name.strip_suffix(".exe").unwrap_or(&name);
    name == "node"
        || name == "ruby"
        || name == "perl"
        || name == "php"
        || name == "bash"
        || name == "sh"
        || name == "zsh"
        || name == "fish"
        || name == "pwsh"
        || name == "powershell"
        || name == "lua"
        || name == "java"
        || name.starts_with("python")
}

/// 只在当前进程 argv 中寻找脚本入口；从不读取 environ，也不会启动解释器或用户命令。
fn script_source(executable: &str, metadata: Option<&ProcessMetadata>) -> ScriptSource {
    let Some(resolved) = resolved_script_path(executable, metadata) else {
        return ScriptSource::Unknown;
    };

    if is_trusted_system_path(&resolved) {
        #[cfg(windows)]
        return match windows::authenticode_trusted(&resolved) {
            Some(true) => ScriptSource::TrustedSystem,
            _ => ScriptSource::Unknown,
        };
        #[cfg(target_os = "linux")]
        return ScriptSource::Unknown;
        #[cfg(target_os = "macos")]
        return match macos_apple_system_signature(&resolved) {
            Some(true) => ScriptSource::TrustedSystem,
            _ => ScriptSource::Unknown,
        };
        #[cfg(not(any(windows, target_os = "macos", target_os = "linux")))]
        ScriptSource::TrustedSystem
    } else {
        ScriptSource::External
    }
}

/// 解释器进程的分组来源必须同时具备同一规范化 cwd 和同一个规范化脚本文件路径。
///
/// 脚本来源不完整（例如 `-c` 内嵌代码、模块入口、缺少 argv/cwd 或文件已消失）时，
/// 返回 None，让解释器保持独立，避免把同一个 Node/Python 可执行文件当成同一服务。
fn interpreter_family_source(
    executable: &str,
    metadata: Option<&ProcessMetadata>,
) -> Option<InterpreterFamilySource> {
    let metadata = metadata?;
    let working_dir = metadata.working_dir.as_deref()?;
    let working_dir_key = normalized_path_key_from_str(working_dir)?;
    let source_key = normalized_path_key(&resolved_script_path(executable, Some(metadata))?)?;
    Some(InterpreterFamilySource {
        working_dir_key,
        source_key,
    })
}

/// 仅从已采集的 argv 和 cwd 定位脚本；不访问 environ、不执行命令，也不猜测模块来源。
fn resolved_script_path(executable: &str, metadata: Option<&ProcessMetadata>) -> Option<PathBuf> {
    let Some(metadata) = metadata else {
        return None;
    };
    if metadata.command.is_empty() {
        return None;
    }

    let args = metadata.command.as_slice();
    let start = match args
        .first()
        .map(|argv0| argv0_matches_executable(argv0, executable))
    {
        Some(true) => 1,
        Some(false) if is_likely_script_path(&args[0]) => 0,
        _ => return None,
    };
    let args = &args[start..];
    if args.is_empty() {
        return None;
    }

    let mut script = None;
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        let lower = arg.to_ascii_lowercase();
        if matches!(
            lower.as_str(),
            "-c" | "-e" | "-p" | "--eval" | "--print" | "-command" | "-encodedcommand"
        ) {
            // 内嵌代码可能来自系统任务或用户任务，来源无法验证时保持 unknown。
            return None;
        }
        if matches!(lower.as_str(), "-file" | "--file" | "-jar") {
            script = args.get(index + 1).map(String::as_str);
            break;
        }
        if matches!(lower.as_str(), "-m" | "--module" | "-classpath" | "-cp") {
            // 模块名或 classpath 不足以还原实际代码文件来源，不能据此标成系统进程。
            return None;
        }
        if !arg.starts_with('-') {
            script = Some(arg);
            break;
        }
        index += 1;
    }

    let script = script.filter(|value| !value.is_empty() && *value != "-")?;
    let path = Path::new(script);
    let resolved = if path.is_absolute() {
        path.to_path_buf()
    } else if let Some(working_dir) = metadata.working_dir.as_deref() {
        PathBuf::from(working_dir).join(path)
    } else {
        return None;
    };
    std::fs::canonicalize(resolved).ok()
}

/// 可执行文件与解释器脚本都必须能解析成磁盘上的规范路径才允许参与合并。
fn normalized_path_key_from_str(path: &str) -> Option<String> {
    normalized_path_key(Path::new(path))
}

fn normalized_path_key(path: &Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    #[cfg(windows)]
    {
        // Windows 路径不区分大小写，统一分隔符与 ASCII 大小写以消除路径显示差异。
        Some(
            path.to_string_lossy()
                .replace('/', "\\")
                .to_ascii_lowercase(),
        )
    }
    #[cfg(not(windows))]
    {
        Some(path.to_string_lossy().into_owned())
    }
}

/// 只有 argv[0] 与进程 exe 同名或明确属于同系别名时才跳过第一项。
fn argv0_matches_executable(argv0: &str, executable: &str) -> bool {
    let name = |value: &str| {
        Path::new(value)
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default()
            .trim_end_matches(".exe")
            .to_owned()
    };
    let argv_name = name(argv0);
    let executable_name = name(executable);
    if argv_name == executable_name {
        return true;
    }
    (argv_name.starts_with("python") && executable_name.starts_with("python"))
        || (matches!(argv_name.as_str(), "sh" | "bash" | "zsh" | "fish")
            && matches!(executable_name.as_str(), "sh" | "bash" | "zsh" | "fish"))
        || (matches!(argv_name.as_str(), "node" | "nodejs")
            && matches!(executable_name.as_str(), "node" | "nodejs"))
        || (matches!(argv_name.as_str(), "powershell" | "pwsh")
            && matches!(executable_name.as_str(), "powershell" | "pwsh"))
}

fn is_likely_script_path(value: &str) -> bool {
    Path::new(value)
        .extension()
        .map(|extension| {
            matches!(
                extension.to_string_lossy().to_ascii_lowercase().as_str(),
                "py" | "pyw"
                    | "js"
                    | "mjs"
                    | "cjs"
                    | "rb"
                    | "pl"
                    | "php"
                    | "lua"
                    | "sh"
                    | "bash"
                    | "zsh"
                    | "ps1"
                    | "jar"
            )
        })
        .unwrap_or(false)
}

/// 对普通 Unix 平台使用组件级路径判断，防止 `/usr/bin2` 被误当作 `/usr/bin`。
fn is_trusted_system_path(path: &Path) -> bool {
    #[cfg(target_os = "macos")]
    {
        return is_macos_system_path(path);
    }
    #[cfg(target_os = "linux")]
    {
        return is_linux_system_path(path);
    }
    #[cfg(windows)]
    {
        return windows::is_protected_system_path(path);
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        let _ = path;
        false
    }
}

#[cfg(target_os = "macos")]
fn is_macos_system_path(path: &Path) -> bool {
    [
        "/System",
        "/usr/bin",
        "/usr/sbin",
        "/usr/libexec",
        "/bin",
        "/sbin",
    ]
    .iter()
    .any(|root| path.starts_with(root))
}

#[cfg(target_os = "macos")]
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct MacSignatureFingerprint {
    path: String,
    device: u64,
    inode: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
}

#[cfg(target_os = "macos")]
static MAC_SIGNATURE_CACHE: OnceLock<Mutex<HashMap<MacSignatureFingerprint, Option<bool>>>> =
    OnceLock::new();

/// 只有系统受保护目录中的文件再通过 Apple 锚定签名验证，才可证明其为系统组件。
///
/// codesign 是系统固定路径下的只读校验工具；以独立 argv 调用，不经过 shell，不执行目标
/// 文件或用户命令。验证结果按设备、inode、大小和纳秒级修改时间缓存，避免每轮扫描反复
/// 启动 codesign；超时或工具不可用均返回 None，由调用方归 unknown。
#[cfg(target_os = "macos")]
fn macos_apple_system_signature(path: &Path) -> Option<bool> {
    use std::os::unix::fs::MetadataExt;

    let path = std::fs::canonicalize(path).ok()?;
    let metadata = std::fs::metadata(&path).ok()?;
    let fingerprint = MacSignatureFingerprint {
        path: path.to_string_lossy().into_owned(),
        device: metadata.dev(),
        inode: metadata.ino(),
        size: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
    };
    let cache = MAC_SIGNATURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(result) = lock_recover(cache).get(&fingerprint).copied() {
        return result;
    }

    let result = verify_macos_apple_signature(&path);
    let mut cache = lock_recover(cache);
    if cache.len() >= 512 {
        cache.clear();
    }
    cache.insert(fingerprint, result);
    result
}

#[cfg(target_os = "macos")]
fn verify_macos_apple_signature(path: &Path) -> Option<bool> {
    use std::process::{Command, Stdio};

    let mut child = Command::new("/usr/bin/codesign")
        .arg("--verify")
        .arg("--strict")
        .arg("-R=anchor apple")
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status.success()),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                // 对系统校验工具设定硬时限，超时后只结束该 codesign PID，不触及目标进程。
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn is_linux_system_path(path: &Path) -> bool {
    [
        "/usr/bin",
        "/usr/sbin",
        "/usr/lib",
        "/usr/libexec",
        "/bin",
        "/sbin",
        "/lib",
        "/lib64",
    ]
    .iter()
    .any(|root| path.starts_with(root))
}

#[cfg(windows)]
fn is_windows_service_host(name: &str, executable: Option<&str>) -> bool {
    name.eq_ignore_ascii_case("svchost.exe")
        || executable
            .and_then(|path| Path::new(path).file_name())
            .map(|file| file.to_string_lossy().eq_ignore_ascii_case("svchost.exe"))
            .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_windows_service_host(_name: &str, _executable: Option<&str>) -> bool {
    false
}

/// 为 socket 快照读取一次只读平台服务映射；其他平台没有 SCM 服务登记表。
fn service_hosts() -> Result<HashMap<u32, ServiceHostInfo>, String> {
    #[cfg(windows)]
    {
        return windows::running_services();
    }
    #[cfg(not(windows))]
    {
        Ok(HashMap::new())
    }
}

/// TCP/UDP 端口列表的确定性排序，便于前端稳定合并快照。
fn sort_ports(ports: &mut [PortInfo]) {
    ports.sort_by(|left, right| {
        left.port
            .cmp(&right.port)
            .then_with(|| left.protocol.cmp(&right.protocol))
            .then_with(|| left.address.cmp(&right.address))
            .then_with(|| left.pid.cmp(&right.pid))
    });
}

#[cfg(test)]
mod tests {
    use super::{script_source, ProcessMetadata, ScriptSource};

    fn script_metadata(executable: &str, script: &str, working_dir: &str) -> ProcessMetadata {
        ProcessMetadata {
            name: executable.rsplit('/').next().unwrap_or(executable).into(),
            executable: Some(executable.into()),
            working_dir: Some(working_dir.into()),
            command: vec![executable.into(), script.into()],
        }
    }

    #[test]
    fn python3_argv0_is_skipped_before_classifying_script() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let script = directory.path().join("server.py");
        std::fs::write(&script, "# 测试脚本\n").expect("测试脚本应可写入");
        let metadata = script_metadata(
            "/usr/bin/python3",
            script.to_str().expect("测试路径应为 UTF-8"),
            directory.path().to_str().expect("测试路径应为 UTF-8"),
        );

        assert!(matches!(
            script_source("/usr/bin/python3", Some(&metadata)),
            ScriptSource::External
        ));
    }

    #[test]
    fn node_relative_script_uses_working_directory() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        std::fs::write(directory.path().join("server.js"), "// 测试脚本\n")
            .expect("测试脚本应可写入");
        let metadata = script_metadata(
            "/usr/bin/node",
            "server.js",
            directory.path().to_str().expect("测试路径应为 UTF-8"),
        );

        assert!(matches!(
            script_source("/usr/bin/node", Some(&metadata)),
            ScriptSource::External
        ));
    }

    #[test]
    fn bash_inline_code_has_unknown_source() {
        let metadata = ProcessMetadata {
            name: "bash".into(),
            executable: Some("/bin/bash".into()),
            working_dir: Some("/tmp".into()),
            command: vec!["/bin/bash".into(), "-c".into(), "run something".into()],
        };

        assert!(matches!(
            script_source("/bin/bash", Some(&metadata)),
            ScriptSource::Unknown
        ));
    }

    #[test]
    fn missing_script_path_is_unknown() {
        let directory = tempfile::tempdir().expect("临时目录应可创建");
        let metadata = script_metadata(
            "/usr/bin/python3",
            "deleted.py",
            directory.path().to_str().expect("测试路径应为 UTF-8"),
        );

        assert!(matches!(
            script_source("/usr/bin/python3", Some(&metadata)),
            ScriptSource::Unknown
        ));
    }
}
