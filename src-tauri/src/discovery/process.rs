//! 原生进程身份与单 PID 停止实现。
//!
//! `sysinfo::Process::start_time` 只有秒级精度，因此不能用于外部控制身份。macOS 从
//! libproc 的 `proc_bsdinfo` 读取启动时间；Linux 读取 `/proc/<pid>/stat` 的 starttime
//! tick；Windows 则由同一个进程 handle 读取 FILETIME 并执行 TerminateProcess。

#[derive(Clone, Debug)]
pub(super) struct NativeProcessInfo {
    /// 平台原生、可稳定比较的进程创建时间数值字符串。
    pub created_at: String,
    /// 平台直接提供的可执行文件路径；读取失败时由 sysinfo 元数据补充。
    pub executable: Option<String>,
    /// 原生进程表中的父 PID，用于额外确认自身祖先进程。
    pub parent_pid: Option<u32>,
}

#[cfg(windows)]
pub(super) use super::windows::{can_terminate, query_process, terminate};

#[cfg(target_os = "macos")]
mod macos {
    use super::NativeProcessInfo;
    use std::ffi::{c_char, c_void};
    use std::io;
    use std::mem::size_of;

    const PROC_PIDTBSDINFO: i32 = 3;
    const PROC_PIDPATHINFO_MAXSIZE: usize = 4096;
    const STATUS_ZOMBIE: u32 = 5;

    /// 结构字段与当前 macOS SDK `sys/proc_info.h` 的 `proc_bsdinfo` 保持一致。
    /// 这里仅复制固定布局的查询结果，不把结构中的名字或参数用作安全身份。
    #[repr(C)]
    #[derive(Default)]
    struct ProcBsdInfo {
        pbi_flags: u32,
        pbi_status: u32,
        pbi_xstatus: u32,
        pbi_pid: u32,
        pbi_ppid: u32,
        pbi_uid: u32,
        pbi_gid: u32,
        pbi_ruid: u32,
        pbi_rgid: u32,
        pbi_svuid: u32,
        pbi_svgid: u32,
        rfu_1: u32,
        pbi_comm: [c_char; 16],
        pbi_name: [c_char; 32],
        pbi_nfiles: u32,
        pbi_pgid: u32,
        pbi_pjobc: u32,
        e_tdev: u32,
        e_tpgid: u32,
        pbi_nice: i32,
        pbi_start_tvsec: u64,
        pbi_start_tvusec: u64,
    }

    #[link(name = "proc")]
    unsafe extern "C" {
        fn proc_pidinfo(
            pid: i32,
            flavor: i32,
            arg: u64,
            buffer: *mut c_void,
            buffersize: i32,
        ) -> i32;
        fn proc_pidpath(pid: i32, buffer: *mut c_void, buffersize: u32) -> i32;
    }

    /// 通过 libproc 读取 PID、父 PID 和秒+微秒启动时间，PID 退出或被复用时绝不猜测。
    pub(crate) fn query_process(pid: u32) -> Result<Option<NativeProcessInfo>, String> {
        let pid_native = i32::try_from(pid).map_err(|_| "进程 PID 超出 macOS 支持范围。")?;
        let Some(first) = read_bsd_info(pid_native)? else {
            return Ok(None);
        };
        if first.pbi_pid != pid {
            return Ok(None);
        }
        if first.pbi_status == STATUS_ZOMBIE {
            return Ok(None);
        }

        let executable = read_executable_path(pid_native);
        // 读取路径期间进程可能退出并被系统复用；第二次原生查询用创建时间封住这一窗口。
        let Some(second) = read_bsd_info(pid_native)? else {
            return Ok(None);
        };
        if second.pbi_pid != pid || start_time(&second) != start_time(&first) {
            return Ok(None);
        }
        if second.pbi_status == STATUS_ZOMBIE {
            return Ok(None);
        }

        Ok(Some(NativeProcessInfo {
            created_at: start_time(&first),
            executable,
            parent_pid: nonzero(first.pbi_ppid),
        }))
    }

    fn read_bsd_info(pid: i32) -> Result<Option<ProcBsdInfo>, String> {
        let mut info = ProcBsdInfo::default();
        // libproc 通过 errno 解释 -1，但库不保证失败前清空线程 errno；先清零再检查结果。
        unsafe {
            *libc::__error() = 0;
        }
        let read = unsafe {
            proc_pidinfo(
                pid,
                PROC_PIDTBSDINFO,
                0,
                (&mut info as *mut ProcBsdInfo).cast::<c_void>(),
                size_of::<ProcBsdInfo>() as i32,
            )
        };
        if read == size_of::<ProcBsdInfo>() as i32 {
            return Ok(Some(info));
        }

        let error = io::Error::last_os_error();
        match (read, error.raw_os_error()) {
            (_, Some(libc::ESRCH)) | (_, Some(libc::ENOENT)) => Ok(None),
            (_, Some(libc::EPERM)) | (_, Some(libc::EACCES)) => {
                Err("macOS 拒绝读取该进程的原生创建身份。".into())
            }
            (0, _) => {
                let pid_result = unsafe { libc::kill(pid, 0) };
                if pid_result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
                {
                    Ok(None)
                } else {
                    Err("macOS 未返回完整的原生进程身份，且进程仍可见。".into())
                }
            }
            (-1, Some(code)) => Err(format!("macOS 原生进程身份探测失败（系统错误码 {code}）。")),
            _ => Err("macOS 返回了不完整的原生进程身份数据。".into()),
        }
    }

    fn start_time(info: &ProcBsdInfo) -> String {
        // libproc 提供秒和微秒两部分；合并为精确微秒数，不经过浮点或 JavaScript Number。
        info.pbi_start_tvsec
            .saturating_mul(1_000_000)
            .saturating_add(info.pbi_start_tvusec)
            .to_string()
    }

    fn read_executable_path(pid: i32) -> Option<String> {
        let mut buffer = [0u8; PROC_PIDPATHINFO_MAXSIZE];
        let written = unsafe {
            proc_pidpath(
                pid,
                buffer.as_mut_ptr().cast::<c_void>(),
                buffer.len() as u32,
            )
        };
        if written <= 0 {
            return None;
        }
        let end = buffer
            .iter()
            .position(|byte| *byte == 0)
            .unwrap_or(buffer.len());
        Some(String::from_utf8_lossy(&buffer[..end]).into_owned())
    }

    fn nonzero(pid: u32) -> Option<u32> {
        (pid != 0).then_some(pid)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use super::NativeProcessInfo;
    use crate::models::ProcessIdentity;
    use std::io;
    use std::path::PathBuf;
    use std::thread;
    use std::time::{Duration, Instant};

    /// 读取 Linux `/proc` 原始 starttime tick，并用前后两次读数规避 PID 复用窗口。
    pub(crate) fn query_process(pid: u32) -> Result<Option<NativeProcessInfo>, String> {
        let Some(first) = read_stat(pid)? else {
            return Ok(None);
        };
        let executable = std::fs::read_link(PathBuf::from(format!("/proc/{pid}/exe")))
            .ok()
            .map(|path| path.to_string_lossy().into_owned());
        let Some(second) = read_stat(pid)? else {
            return Ok(None);
        };
        if first.created_at != second.created_at {
            return Ok(None);
        }
        Ok(Some(NativeProcessInfo {
            created_at: first.created_at,
            executable,
            parent_pid: second.parent_pid,
        }))
    }

    struct StatIdentity {
        parent_pid: Option<u32>,
        created_at: String,
    }

    fn read_stat(pid: u32) -> Result<Option<StatIdentity>, String> {
        let path = format!("/proc/{pid}/stat");
        let bytes = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                return Err("Linux 拒绝读取该进程的原生创建身份。".into())
            }
            Err(error) => return Err(format!("无法读取 Linux 进程身份：{error}")),
        };

        let content = String::from_utf8_lossy(&bytes);
        let Some(command_end) = content.rfind(')') else {
            return Err("Linux 进程身份格式无效。".into());
        };
        let fields = content[command_end + 1..]
            .split_whitespace()
            .collect::<Vec<_>>();
        // ')' 后第 0 项是 state（字段 3）；ppid 是字段 4，starttime 是字段 22。
        let parent_pid = fields.get(1).and_then(|value| value.parse::<u32>().ok());
        let Some(created_at) = fields.get(19).filter(|value| !value.is_empty()) else {
            return Err("Linux 进程身份缺少原生启动计数。".into());
        };
        if created_at.bytes().any(|byte| !byte.is_ascii_digit()) {
            return Err("Linux 返回了无效的原生启动计数。".into());
        }
        Ok(Some(StatIdentity {
            parent_pid,
            created_at: (*created_at).to_owned(),
        }))
    }

    /// 仅当 PID 创建身份仍匹配且当前用户能向该 PID 发信号时，报告可停止。
    pub(crate) fn can_terminate(identity: &ProcessIdentity) -> Result<bool, String> {
        match query_process(identity.pid)? {
            Some(process) if process.created_at == identity.created_at => {}
            _ => return Ok(false),
        }
        let pid = native_pid(identity.pid)?;
        let result = unsafe { libc::kill(pid, 0) };
        if result == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) => Ok(false),
            Some(libc::ESRCH) => Ok(false),
            _ => Err("无法检查当前用户是否有权停止该 Linux 进程。".into()),
        }
    }

    /// 先发 SIGTERM 给单 PID，等候 5 秒后仅对仍匹配身份的 PID 发 SIGKILL。
    pub(crate) fn terminate(identity: &ProcessIdentity) -> Result<(), String> {
        refuse_self_or_ancestor(identity.pid)?;
        if !identity_matches(identity)? {
            return Err("进程已退出或 PID 身份已变化，已拒绝停止。".into());
        }
        send_signal_if_same(identity, libc::SIGTERM)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if !identity_matches(identity)? {
                return Ok(());
            }
        }

        if !identity_matches(identity)? {
            return Ok(());
        }
        send_signal_if_same(identity, libc::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if !identity_matches(identity)? {
                return Ok(());
            }
        }
        Err("已发送 SIGKILL，但进程仍可见；保留错误状态供用户重试。".into())
    }

    fn refuse_self_or_ancestor(pid: u32) -> Result<(), String> {
        let mut current = std::process::id();
        for _ in 0..128 {
            if current == pid {
                return Err("当前程序及其祖先进程不可停止。".into());
            }
            // PID 1 是操作系统的 init/launchd，始终保护；不尝试读取其通常受限的身份。
            if current <= 1 {
                break;
            }
            let Some(process) = query_process(current)? else {
                break;
            };
            let Some(parent) = process.parent_pid else {
                break;
            };
            if parent == current {
                break;
            }
            current = parent;
        }
        Ok(())
    }

    fn identity_matches(identity: &ProcessIdentity) -> Result<bool, String> {
        Ok(query_process(identity.pid)?
            .map(|process| process.created_at == identity.created_at)
            .unwrap_or(false))
    }

    fn send_signal_if_same(identity: &ProcessIdentity, signal: i32) -> Result<(), String> {
        if !identity_matches(identity)? {
            return Ok(());
        }
        let pid = native_pid(identity.pid)?;
        if unsafe { libc::kill(pid, signal) } == 0 {
            return Ok(());
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) if !identity_matches(identity)? => Ok(()),
            Some(libc::EPERM) | Some(libc::EACCES) => {
                Err("当前用户没有停止该 Linux 进程的权限，且不会自动提权。".into())
            }
            _ => Err("向 Linux 进程发送停止信号失败。".into()),
        }
    }

    fn native_pid(pid: u32) -> Result<i32, String> {
        i32::try_from(pid).map_err(|_| "进程 PID 超出 Linux 支持范围。".into())
    }
}

#[cfg(target_os = "macos")]
mod macos_control {
    use super::NativeProcessInfo;
    use crate::models::ProcessIdentity;
    use std::io;
    use std::thread;
    use std::time::{Duration, Instant};

    pub(crate) fn query_process(pid: u32) -> Result<Option<NativeProcessInfo>, String> {
        super::macos::query_process(pid)
    }

    pub(crate) fn can_terminate(identity: &ProcessIdentity) -> Result<bool, String> {
        match query_process(identity.pid)? {
            Some(process) if process.created_at == identity.created_at => {}
            _ => return Ok(false),
        }
        let pid = native_pid(identity.pid)?;
        if unsafe { libc::kill(pid, 0) } == 0 {
            return Ok(true);
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::EPERM) | Some(libc::EACCES) | Some(libc::ESRCH) => Ok(false),
            _ => Err("无法检查当前用户是否有权停止该 macOS 进程。".into()),
        }
    }

    /// macOS 只向经过创建时间复核的单一 PID 发送 TERM/KILL，不使用 killpg。
    pub(crate) fn terminate(identity: &ProcessIdentity) -> Result<(), String> {
        refuse_self_or_ancestor(identity.pid)?;
        if !identity_matches(identity)? {
            return Err("进程已退出或 PID 身份已变化，已拒绝停止。".into());
        }
        send_signal_if_same(identity, libc::SIGTERM)?;

        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if !identity_matches(identity)? {
                return Ok(());
            }
        }

        if !identity_matches(identity)? {
            return Ok(());
        }
        send_signal_if_same(identity, libc::SIGKILL)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline {
            thread::sleep(Duration::from_millis(50));
            if !identity_matches(identity)? {
                return Ok(());
            }
        }
        Err("已发送 SIGKILL，但进程仍可见；保留错误状态供用户重试。".into())
    }

    fn refuse_self_or_ancestor(pid: u32) -> Result<(), String> {
        let mut current = std::process::id();
        for _ in 0..128 {
            if current == pid {
                return Err("当前程序及其祖先进程不可停止。".into());
            }
            // init/systemd 为平台根进程，始终保护且无须越权查询其详情。
            if current <= 1 {
                break;
            }
            let Some(process) = query_process(current)? else {
                break;
            };
            let Some(parent) = process.parent_pid else {
                break;
            };
            if parent == current {
                break;
            }
            current = parent;
        }
        Ok(())
    }

    fn identity_matches(identity: &ProcessIdentity) -> Result<bool, String> {
        Ok(query_process(identity.pid)?
            .map(|process| process.created_at == identity.created_at)
            .unwrap_or(false))
    }

    fn send_signal_if_same(identity: &ProcessIdentity, signal: i32) -> Result<(), String> {
        if !identity_matches(identity)? {
            return Ok(());
        }
        let pid = native_pid(identity.pid)?;
        if unsafe { libc::kill(pid, signal) } == 0 {
            return Ok(());
        }
        match io::Error::last_os_error().raw_os_error() {
            Some(libc::ESRCH) if !identity_matches(identity)? => Ok(()),
            Some(libc::EPERM) | Some(libc::EACCES) => {
                Err("当前用户没有停止该 macOS 进程的权限，且不会自动提权。".into())
            }
            _ => Err("向 macOS 进程发送停止信号失败。".into()),
        }
    }

    fn native_pid(pid: u32) -> Result<i32, String> {
        i32::try_from(pid).map_err(|_| "进程 PID 超出 macOS 支持范围。".into())
    }
}

#[cfg(target_os = "linux")]
pub(super) use linux::{can_terminate, query_process, terminate};

#[cfg(target_os = "macos")]
pub(super) use macos_control::{can_terminate, query_process, terminate};

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
mod other_unix {
    use super::NativeProcessInfo;
    use crate::models::ProcessIdentity;

    pub(crate) fn query_process(_pid: u32) -> Result<Option<NativeProcessInfo>, String> {
        Err("当前 Unix 平台未实现高精度原生进程身份读取。".into())
    }

    pub(crate) fn can_terminate(_identity: &ProcessIdentity) -> Result<bool, String> {
        Ok(false)
    }

    pub(crate) fn terminate(_identity: &ProcessIdentity) -> Result<(), String> {
        Err("当前 Unix 平台不支持外部进程停止。".into())
    }
}

#[cfg(all(unix, not(any(target_os = "macos", target_os = "linux"))))]
pub(super) use other_unix::{can_terminate, query_process, terminate};

#[cfg(not(any(unix, windows)))]
mod unsupported {
    use super::NativeProcessInfo;
    use crate::models::ProcessIdentity;

    pub(crate) fn query_process(_pid: u32) -> Result<Option<NativeProcessInfo>, String> {
        Err("当前平台不支持高精度进程身份读取。".into())
    }

    pub(crate) fn can_terminate(_identity: &ProcessIdentity) -> Result<bool, String> {
        Ok(false)
    }

    pub(crate) fn terminate(_identity: &ProcessIdentity) -> Result<(), String> {
        Err("当前平台不支持外部进程停止。".into())
    }
}

#[cfg(not(any(unix, windows)))]
pub(super) use unsupported::{can_terminate, query_process, terminate};
