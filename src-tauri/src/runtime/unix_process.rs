//! Unix PTY 的进程组清理。
//!
//! portable-pty 为每个 PTY 子进程建立独立 session。本模块持续记录该 session 中
//! 进程的 PID 与启动标识；停止时只向已验证 session 的进程组发信号，避免 PID 复用误杀。

use std::collections::{HashMap, HashSet};
use std::io;
#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

#[derive(Clone, Debug)]
struct ProcessRecord {
    pid: i32,
    parent: i32,
    group: i32,
    session: i32,
    start_identity: u64,
    state: char,
}

pub(super) struct SessionTracker {
    root_pid: i32,
    root_start: u64,
    known: Mutex<HashMap<i32, u64>>,
}

impl SessionTracker {
    /// 必须在 PTY 进程仍作为 session leader 时记录其启动标识。
    pub(super) fn new(root_pid: Option<u32>) -> Result<std::sync::Arc<Self>, String> {
        let root_pid = root_pid
            .and_then(|pid| i32::try_from(pid).ok())
            .ok_or_else(|| "PTY 未提供有效 PID，无法安全记录 session。".to_owned())?;
        let mut found = None;
        for _ in 0..4 {
            let processes =
                process_list().map_err(|error| format!("无法检查 PTY session：{error}"))?;
            found = processes
                .iter()
                .find(|process| {
                    process.pid == root_pid
                        && process.session == root_pid
                        && process.group == root_pid
                })
                .map(|process| process.start_identity);
            if found.is_some() {
                break;
            }
            thread::sleep(Duration::from_millis(2));
        }
        let root_start = found.ok_or_else(|| {
            "PTY 进程没有建立预期的独立 Unix session，已拒绝不安全的进程管理。".to_owned()
        })?;
        let tracker = std::sync::Arc::new(Self {
            root_pid,
            root_start,
            known: Mutex::new(HashMap::new()),
        });
        tracker.refresh()?;
        Ok(tracker)
    }

    /// 子进程监视循环每 100ms 更新一次身份清单；root shell 退出前，能记录同 session
    /// 中额外的 job-control 进程组，供稍后安全清理。
    pub(super) fn refresh(&self) -> Result<(), String> {
        let processes = process_list().map_err(|error| format!("无法刷新受管进程清单：{error}"))?;
        let mut known = self
            .known
            .lock()
            .map_err(|_| "Unix 进程清单不可用".to_owned())?;
        let _ = capture_session_members(&processes, self.root_pid, self.root_start, &mut known);
        Ok(())
    }

    /// 端口探测复用经 session 与 PID 启动身份验证的进程清单；这样 double-fork 后被
    /// launchd 重新托管、但仍留在原 PTY session 的进程也能被准确归属。
    pub(super) fn member_pids(&self) -> Result<HashSet<u32>, String> {
        let processes = process_list().map_err(|error| format!("无法检查受管 session：{error}"))?;
        let mut known = self
            .known
            .lock()
            .map_err(|_| "Unix 进程清单不可用".to_owned())?;
        Ok(
            capture_session_members(&processes, self.root_pid, self.root_start, &mut known)
                .into_iter()
                .filter_map(|process| u32::try_from(process.pid).ok())
                .collect(),
        )
    }

    /// Ctrl-C 已先写入 PTY；随后向当前 session 的进程组发 SIGTERM，最多等待 5 秒，
    /// 再向仍存活且身份仍匹配的组发送 SIGKILL。setsid 脱离的 daemon 不属于此清理范围。
    pub(super) fn terminate(&self) -> Result<(), String> {
        let mut known = self
            .known
            .lock()
            .map_err(|_| "Unix 进程清单不可用".to_owned())?;
        let initial =
            process_list().map_err(|error| format!("无法检查受管进程 session：{error}"))?;
        let mut remaining =
            capture_session_members(&initial, self.root_pid, self.root_start, &mut known);
        if remaining.is_empty() {
            return Ok(());
        }

        signal_groups(
            &remaining,
            libc::SIGTERM,
            self.root_pid,
            self.root_start,
            &mut known,
        )
        .map_err(|error| format!("无法向受管进程组发送温和停止信号：{error}"))?;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(100));
            let current =
                process_list().map_err(|error| format!("无法再次检查受管进程 session：{error}"))?;
            remaining =
                capture_session_members(&current, self.root_pid, self.root_start, &mut known);
            if remaining.is_empty() {
                return Ok(());
            }
            signal_groups(
                &remaining,
                libc::SIGTERM,
                self.root_pid,
                self.root_start,
                &mut known,
            )
            .map_err(|error| format!("无法继续停止受管进程组：{error}"))?;
        }

        let current =
            process_list().map_err(|error| format!("无法检查需要强制停止的进程：{error}"))?;
        remaining = capture_session_members(&current, self.root_pid, self.root_start, &mut known);
        if !remaining.is_empty() {
            signal_groups(
                &remaining,
                libc::SIGKILL,
                self.root_pid,
                self.root_start,
                &mut known,
            )
            .map_err(|error| format!("无法强制停止受管进程组：{error}"))?;
        }

        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            let current =
                process_list().map_err(|error| format!("无法确认受管进程是否退出：{error}"))?;
            remaining =
                capture_session_members(&current, self.root_pid, self.root_start, &mut known);
            if remaining.is_empty() {
                return Ok(());
            }
        }
        Err("受管 session 内仍有进程存活，已保留错误状态供用户重试。".into())
    }
}

/// 启动前确认本机允许读取进程清单；遇到沙箱或权限限制时，在目标代码执行前拒绝启动。
pub(super) fn preflight() -> Result<(), String> {
    process_list()
        .map(|processes| {
            if processes.is_empty() {
                Err("系统没有返回可检查的 Unix 进程清单。".to_owned())
            } else {
                Ok(())
            }
        })
        .map_err(|error| format!("无法安全检查 Unix 进程 session：{error}"))?
}

/// Tracker 初始化异常且子进程仍活着时，PID 仍由 Child 句柄持有，不能被重用。
/// 仅当 getsid/getpgid 都证明它是 portable-pty 新建的 session leader 时才杀整组；
/// 无法验证时只对仍被 Child 句柄持有的根进程发 SIGKILL。
pub(super) fn emergency_kill(pid: Option<u32>) {
    let Some(pid) = pid.and_then(|pid| i32::try_from(pid).ok()) else {
        return;
    };
    let is_verified_session_leader =
        unsafe { libc::getsid(pid) == pid && libc::getpgid(pid) == pid };
    let target = if is_verified_session_leader {
        -pid
    } else {
        pid
    };
    unsafe {
        let _ = libc::kill(target, libc::SIGKILL);
    }
}

/// 只承认仍匹配启动标识的 session leader、已记录的 PID+starttime，或已验证父进程的
/// 当前子进程。root 消失后，不会仅凭复用过的 sid 数字认领外部进程。
fn capture_session_members(
    processes: &[ProcessRecord],
    root_pid: i32,
    root_start: u64,
    known: &mut HashMap<i32, u64>,
) -> Vec<ProcessRecord> {
    let root_alive = processes.iter().any(|process| {
        process.pid == root_pid
            && process.session == root_pid
            && process.group == root_pid
            && process.start_identity == root_start
    });
    let by_pid = processes
        .iter()
        .map(|process| (process.pid, process))
        .collect::<HashMap<_, _>>();
    let mut accepted = HashSet::new();

    loop {
        let mut changed = false;
        for process in processes
            .iter()
            .filter(|process| process.session == root_pid)
        {
            let same_known_process = known
                .get(&process.pid)
                .map(|start| *start == process.start_identity)
                .unwrap_or(false);
            let verified_parent = by_pid
                .get(&process.parent)
                .and_then(|parent| known.get(&parent.pid).map(|start| (*parent, *start)))
                .map(|(parent, start)| parent.start_identity == start)
                .unwrap_or(false);
            if root_alive || same_known_process || verified_parent {
                if accepted.insert(process.pid) {
                    changed = true;
                }
                // 父子链或仍存活的 root 可证明是这个 session 的新进程；PID 在长会话中
                // 复用时更新启动标识，避免旧 PID 记录把新进程永久排除在清理范围外。
                known.insert(process.pid, process.start_identity);
            }
        }
        if !changed {
            break;
        }
    }

    processes
        .iter()
        .filter(|process| {
            accepted.contains(&process.pid)
                && known.get(&process.pid) == Some(&process.start_identity)
                && process.state != 'Z'
                && process.state != 'X'
        })
        .cloned()
        .collect()
}

fn signal_groups(
    processes: &[ProcessRecord],
    signal: i32,
    root_pid: i32,
    root_start: u64,
    known: &mut HashMap<i32, u64>,
) -> io::Result<()> {
    let mut groups = processes
        .iter()
        .map(|process| process.group)
        .filter(|group| *group > 0)
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    groups.sort_unstable();
    for group in groups {
        // 在 killpg 前重新读取成员，避免进程退出/变成 zombie 后对已经失去成员身份的
        // PGID 发信号；session 与启动标识仍由 capture_session_members 再次校验。
        let current = process_list()?;
        let live_members = capture_session_members(&current, root_pid, root_start, known)
            .into_iter()
            .filter(|process| process.group == group)
            .collect::<Vec<_>>();
        if live_members.is_empty() {
            continue;
        }

        // 只对刚重新验证仍有受管成员的 process group 广播，不使用 Child::kill 猜测树。
        let result = unsafe { libc::kill(-group, signal) };
        if result != 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                continue;
            }
            if error.raw_os_error() == Some(libc::EPERM) {
                // Darwin 上最后一个进程可能在枚举与 killpg 之间退出；只有再次确认仍有
                // 同一受管 session 的活进程时，才把 EPERM 当作真实权限错误返回。
                let after = process_list()?;
                let still_live = capture_session_members(&after, root_pid, root_start, known)
                    .into_iter()
                    .filter(|process| process.group == group)
                    .collect::<Vec<_>>();
                if still_live.is_empty() {
                    continue;
                }
                let identities = still_live
                    .iter()
                    .map(|process| {
                        format!(
                            "pid={} ppid={} pgid={} sid={} start={} state={}",
                            process.pid,
                            process.parent,
                            process.group,
                            process.session,
                            process.start_identity,
                            process.state
                        )
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                return Err(io::Error::new(
                    error.kind(),
                    format!(
                        "向 PGID {group} 发送 {} 失败；重新枚举仍有受管成员（session leader pid={root_pid}, start={root_start}, euid={}）：{identities}；系统错误：{error}",
                        signal_name(signal),
                        unsafe { libc::geteuid() },
                    ),
                ));
            }
            return Err(io::Error::new(
                error.kind(),
                format!(
                    "向 PGID {group} 发送 {} 失败，重新枚举时仍有 {} 个受管成员；系统错误：{error}",
                    signal_name(signal),
                    live_members.len()
                ),
            ));
        }
    }
    Ok(())
}

fn signal_name(signal: i32) -> &'static str {
    match signal {
        libc::SIGTERM => "SIGTERM",
        libc::SIGKILL => "SIGKILL",
        _ => "Unix signal",
    }
}

#[cfg(target_os = "linux")]
fn process_list() -> io::Result<Vec<ProcessRecord>> {
    let mut processes = Vec::new();
    for entry in std::fs::read_dir("/proc")? {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let Ok(stat) = std::fs::read_to_string(entry.path().join("stat")) else {
            continue;
        };
        if let Some(record) = parse_linux_stat(pid, &stat) {
            processes.push(record);
        }
    }
    Ok(processes)
}

#[cfg(target_os = "linux")]
fn parse_linux_stat(pid: i32, stat: &str) -> Option<ProcessRecord> {
    // comm 字段可包含空格和括号，必须从最后一个右括号切开，再按固定字段号解析。
    let end_comm = stat.rfind(')')?;
    let fields = stat
        .get(end_comm + 1..)?
        .split_whitespace()
        .collect::<Vec<_>>();
    Some(ProcessRecord {
        pid,
        state: fields.first()?.chars().next()?,
        parent: fields.get(1)?.parse().ok()?,
        group: fields.get(2)?.parse().ok()?,
        session: fields.get(3)?.parse().ok()?,
        // /proc/PID/stat 的 starttime 是字段 22；此切片从字段 3 开始，所以索引为 19。
        start_identity: fields.get(19)?.parse().ok()?,
    })
}

#[cfg(target_os = "macos")]
fn process_list() -> io::Result<Vec<ProcessRecord>> {
    // proc_listpids 是 macOS 的原生枚举接口；不用 `ps sid`（macOS 不支持该字段），
    // 也不把 `ps sess` 的 Mach 指针误认为 session id。
    const PROC_ALL_PIDS: u32 = 1;
    let needed_bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if needed_bytes <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut capacity = (needed_bytes as usize / std::mem::size_of::<i32>()).saturating_add(256);
    for _ in 0..4 {
        let mut pids = vec![0i32; capacity];
        let buffer_bytes = capacity
            .checked_mul(std::mem::size_of::<i32>())
            .and_then(|bytes| i32::try_from(bytes).ok())
            .ok_or_else(|| io::Error::other("进程清单缓冲区大小溢出"))?;
        let returned = unsafe {
            libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), buffer_bytes)
        };
        if returned < 0 {
            return Err(io::Error::last_os_error());
        }
        if returned as usize >= buffer_bytes as usize {
            capacity = capacity.saturating_mul(2);
            continue;
        }
        let count = (returned as usize / std::mem::size_of::<i32>()).min(pids.len());
        pids.truncate(count);
        return Ok(pids
            .into_iter()
            .filter(|pid| *pid > 0)
            .filter_map(macos_process_record)
            .collect());
    }
    Err(io::Error::other("进程清单在多次读取期间持续增长"))
}

#[cfg(target_os = "macos")]
fn macos_process_record(pid: i32) -> Option<ProcessRecord> {
    let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
    let returned = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDTBSDINFO,
            0,
            info.as_mut_ptr().cast(),
            std::mem::size_of::<libc::proc_bsdinfo>() as i32,
        )
    };
    if returned != std::mem::size_of::<libc::proc_bsdinfo>() as i32 {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let session = unsafe { libc::getsid(pid) };
    if session < 0 {
        return None;
    }
    let start_identity = info
        .pbi_start_tvsec
        .checked_mul(1_000_000)?
        .checked_add(info.pbi_start_tvusec)?;
    Some(ProcessRecord {
        pid,
        parent: info.pbi_ppid as i32,
        group: info.pbi_pgid as i32,
        session,
        start_identity,
        state: if info.pbi_status == libc::SZOMB {
            'Z'
        } else {
            'R'
        },
    })
}

#[cfg(all(unix, not(any(target_os = "linux", target_os = "macos"))))]
fn process_list() -> io::Result<Vec<ProcessRecord>> {
    #[cfg(not(target_os = "macos"))]
    use std::hash::{Hash, Hasher};

    let format = "pid=,ppid=,pgid=,lstart=,stat=";
    let output = Command::new("/bin/ps").args(["-axo", format]).output()?;
    if !output.status.success() {
        return Err(io::Error::other("ps 无法读取进程列表"));
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut processes = Vec::new();
    for line in text.lines() {
        let mut fields = line.split_whitespace();
        let (Some(pid), Some(parent), Some(group)) = (
            fields.next().and_then(|field| field.parse::<i32>().ok()),
            fields.next().and_then(|field| field.parse::<i32>().ok()),
            fields.next().and_then(|field| field.parse::<i32>().ok()),
        ) else {
            continue;
        };
        let started = fields.by_ref().take(5).collect::<Vec<_>>().join(" ");
        let state = fields
            .next()
            .and_then(|value| value.chars().next())
            .unwrap_or('?');
        let session = unsafe { libc::getsid(pid) };
        if session < 0 {
            // 对无权检查的系统进程不猜测 SID；受管 PTY 的子进程应当是当前用户进程。
            continue;
        }
        let start_identity = {
            let mut hasher = std::collections::hash_map::DefaultHasher::new();
            started.hash(&mut hasher);
            hasher.finish()
        };
        processes.push(ProcessRecord {
            pid,
            parent,
            group,
            session,
            start_identity,
            state,
        });
    }
    Ok(processes)
}
