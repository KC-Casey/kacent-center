//! Windows 桌面应用适配层。
//!
//! 正常退出通过核验后的顶层 HWND 发送 WM_CLOSE；只有用户单独执行强制结束时才对同一进程
//! handle 调用 TerminateProcess。应用不会加入 PTY 使用的 Job Object，也不会自动提权。

use super::{NativeApplication, NativeEnumeration};
use crate::models::{ApplicationInspection, ProcessIdentity};
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsString;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::ptr::null;
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, FILETIME, HANDLE, HWND, INVALID_HANDLE_VALUE, LPARAM, WAIT_FAILED,
    WAIT_OBJECT_0, WAIT_TIMEOUT, WPARAM,
};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, CREATE_UNICODE_ENVIRONMENT, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE, STARTUPINFOW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindow, GetWindowThreadProcessId, IsIconic, IsWindowVisible,
    SendMessageTimeoutW, SetForegroundWindow, ShowWindow, GW_OWNER, SMTO_ABORTIFHUNG, SW_RESTORE,
    WM_CLOSE,
};

const QUERY_RIGHTS: u32 = PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE;
const HANDLE_POLL_MS: u32 = 0;
const CREATE_FLAGS: PROCESS_CREATION_FLAGS = CREATE_UNICODE_ENVIRONMENT;

struct HandleGuard(HANDLE);
// Windows 进程句柄允许跨线程使用；所有持久句柄由互斥容器持有，Drop 仅关闭句柄，绝不终止应用。
unsafe impl Send for HandleGuard {}
impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
}

#[derive(Default)]
struct WindowSnapshot {
    by_pid: BTreeMap<u32, Vec<HWND>>,
}
fn launched_handles() -> &'static Mutex<HashMap<(u32, String), HandleGuard>> {
    static HANDLES: OnceLock<Mutex<HashMap<(u32, String), HandleGuard>>> = OnceLock::new();
    HANDLES.get_or_init(|| Mutex::new(HashMap::new()))
}

/// 同时依据窗口归属及 PE GUI 子系统发现应用，托盘或暂未建立窗口的应用也能出现。
/// 单个受保护进程不可读只产生提示，不让整次扫描失败；没有证据的进程不猜测为 GUI 应用。
pub(super) fn enumerate() -> Result<NativeEnumeration, String> {
    let windows = enumerate_windows()?;
    launched_handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .retain(|_, handle| unsafe { WaitForSingleObject(handle.0, 0) } == WAIT_TIMEOUT);
    let raw = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if raw == INVALID_HANDLE_VALUE {
        return Err(win_error("枚举 Windows 应用进程失败"));
    }
    let _snapshot = HandleGuard(raw);
    let mut entry: PROCESSENTRY32W = unsafe { zeroed() };
    entry.dwSize = size_of::<PROCESSENTRY32W>() as u32;
    let mut present = unsafe { Process32FirstW(raw, &mut entry) } != 0;
    let mut applications = Vec::new();
    let mut unreadable_windows = 0usize;
    while present {
        let pid = entry.th32ProcessID;
        present = unsafe { Process32NextW(raw, &mut entry) } != 0;
        if pid == 0 || pid == std::process::id() {
            continue;
        }
        let count = windows.by_pid.get(&pid).map_or(0, Vec::len);
        let application = match inspect_pid(pid) {
            Ok(Some(application)) => application,
            Ok(None) => continue,
            Err(_) => {
                if count > 0 {
                    unreadable_windows += 1;
                }
                continue;
            }
        };
        if count == 0 && !is_gui_executable(&application.path) {
            continue;
        }
        applications.push(NativeApplication {
            window_count: count.min(u32::MAX as usize) as u32,
            identity: application.identity,
            path: application.path,
            name: application.name,
            bundle_id: None,
        });
    }
    Ok(NativeEnumeration {
        applications,
        warning: (unreadable_windows > 0).then(|| {
            format!("有 {unreadable_windows} 个窗口所属进程无法读取身份，已跳过且未尝试提权。")
        }),
    })
}

pub(super) fn inspect_identity(
    identity: &ProcessIdentity,
) -> Result<Option<NativeApplication>, String> {
    let Some(application) =
        inspect_pid(identity.pid)?.filter(|application| application.identity == *identity)
    else {
        return Ok(None);
    };
    Ok(Some(NativeApplication {
        identity: application.identity,
        path: application.path,
        name: application.name,
        bundle_id: None,
        window_count: windows_for_pid(identity.pid)?.len().min(u32::MAX as usize) as u32,
    }))
}

pub(super) fn can_request_quit(window_count: u32) -> bool {
    window_count > 0
}

/// 只读取 PE 头，不加载或执行待检查文件；限制偏移以拒绝损坏的可执行文件。
fn is_gui_executable(path: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let check = || -> std::io::Result<bool> {
        let mut file = std::fs::File::open(path)?;
        let mut dos = [0u8; 64];
        file.read_exact(&mut dos)?;
        if &dos[..2] != b"MZ" {
            return Ok(false);
        }
        let offset = u32::from_le_bytes(dos[60..64].try_into().unwrap()) as u64;
        if !(64..=16 * 1024 * 1024).contains(&offset) {
            return Ok(false);
        }
        file.seek(SeekFrom::Start(offset))?;
        let mut header = [0u8; 24];
        file.read_exact(&mut header)?;
        if &header[..4] != b"PE\0\0" || u16::from_le_bytes([header[20], header[21]]) < 70 {
            return Ok(false);
        }
        let mut optional = [0u8; 70];
        file.read_exact(&mut optional)?;
        Ok(matches!(
            u16::from_le_bytes([optional[0], optional[1]]),
            0x10b | 0x20b
        ) && u16::from_le_bytes([optional[68], optional[69]]) == 2)
    };
    check().unwrap_or(false)
}

pub(super) fn validate_application_path(path: &str) -> Result<String, String> {
    let source = PathBuf::from(path.trim());
    if !source.is_absolute() {
        return Err("Windows 应用路径必须是绝对路径。".into());
    }
    let canonical = std::fs::canonicalize(&source)
        .map_err(|error| format!("无法访问 Windows 应用路径 {}：{error}", source.display()))?;
    if !canonical.is_file()
        || !canonical
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("exe"))
            .unwrap_or(false)
    {
        return Err("请选择存在且可访问的 Windows .exe 应用文件。".into());
    }
    Ok(canonical.to_string_lossy().into_owned())
}

pub(super) fn inspect_path(path: &str) -> Result<ApplicationInspection, String> {
    let canonical = validate_application_path(path)?;
    Ok(ApplicationInspection {
        name: application_name(&canonical),
        path: canonical,
        bundle_id: None,
        platform: "windows".into(),
    })
}

pub(super) fn application_executable(path: &str) -> Option<String> {
    Some(path.to_owned())
}

pub(super) fn launch(
    path: &str,
    args: &[String],
    start_dir: &str,
    environment: &BTreeMap<String, String>,
) -> Result<ProcessIdentity, String> {
    let canonical = validate_application_path(path)?;
    let executable_wide = to_wide(&canonical);
    let mut command = quote_windows_argument(&canonical);
    for argument in args {
        command.push(' ');
        command.push_str(&quote_windows_argument(argument));
    }
    let mut command_line = to_wide(&command);
    let working_directory = if start_dir.trim().is_empty() {
        None
    } else {
        let path = std::fs::canonicalize(start_dir)
            .map_err(|error| format!("Windows 启动目录无效：{error}"))?;
        if !path.is_dir() {
            return Err("Windows 应用启动目录必须是有效文件夹。".into());
        }
        Some(to_wide(&path.to_string_lossy()))
    };
    let mut environment_block = build_environment_block(environment)?;

    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut process: PROCESS_INFORMATION = unsafe { zeroed() };
    // lpApplicationName 单独传入已规范化目标；命令行按 Windows CRT 转义规则构造，参数不经过 shell。
    let created = unsafe {
        CreateProcessW(
            executable_wide.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_FLAGS,
            environment_block.as_mut_ptr().cast(),
            working_directory
                .as_ref()
                .map(|directory| directory.as_ptr())
                .unwrap_or(null()),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(win_error("Windows 创建应用进程失败"));
    }
    let process_handle = HandleGuard(process.hProcess);
    let thread_handle = HandleGuard(process.hThread);
    let identity = read_identity_from_handle(process.dwProcessId, process_handle.0)?
        .ok_or_else(|| "Windows 已创建应用，但无法读取其原生创建身份。".to_owned())?;
    drop(thread_handle);
    launched_handles()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert((identity.pid, identity.created_at.clone()), process_handle);

    let Some(application) = inspect_pid(identity.pid)? else {
        // 新进程可能尚未创建顶层窗口。只返回创建身份，公共层会按身份和路径尝试核验，
        // 未经确认的启动器/代理进程不会被猜测性关联。
        return Ok(identity);
    };
    if !same_path(&application.path, &canonical) {
        return Err("Windows 启动器创建的进程路径与目标 .exe 不一致。".into());
    }
    Ok(identity)
}

pub(super) fn request_quit(identity: &ProcessIdentity, target_path: &str) -> Result<(), String> {
    let _handle = checked_handle(identity, target_path, QUERY_RIGHTS)?;
    let windows = windows_for_pid(identity.pid)?;
    if windows.is_empty() {
        return Err("该应用当前没有可发送正常关闭请求的顶层窗口；进程仍会保留跟踪。".into());
    }
    let mut delivered = false;
    for hwnd in windows {
        if !identity_matches_handle(identity, _handle.0)? {
            return Err("请求关闭期间进程身份发生变化，已停止发送消息。".into());
        }
        if !window_belongs_to(hwnd, identity.pid) {
            continue;
        }
        let mut result = 0usize;
        let sent = unsafe {
            SendMessageTimeoutW(
                hwnd,
                WM_CLOSE,
                0 as WPARAM,
                0 as LPARAM,
                SMTO_ABORTIFHUNG,
                1_000,
                &mut result,
            )
        };
        delivered |= sent != 0;
    }
    if delivered {
        Ok(())
    } else {
        Err("Windows 未能把正常关闭消息送达该应用的窗口。".into())
    }
}

pub(super) fn force_quit(identity: &ProcessIdentity, target_path: &str) -> Result<(), String> {
    let handle = checked_handle(identity, target_path, QUERY_RIGHTS | PROCESS_TERMINATE)?;
    if !identity_matches_handle(identity, handle.0)? {
        return Err("强制结束前应用 PID 创建身份已变化。".into());
    }
    if unsafe { TerminateProcess(handle.0, 1) } == 0 {
        return Err(win_error("Windows 强制结束已核验的应用进程失败"));
    }
    match unsafe { WaitForSingleObject(handle.0, 2_000) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err("Windows 接受了强制结束请求，但进程仍未退出。".into()),
        WAIT_FAILED => Err(win_error("等待 Windows 应用进程退出失败")),
        _ => Err("Windows 返回了未知的进程等待状态。".into()),
    }
}

pub(super) fn focus(identity: &ProcessIdentity, target_path: &str) -> Result<(), String> {
    let _handle = checked_handle(identity, target_path, QUERY_RIGHTS)?;
    let windows = windows_for_pid(identity.pid)?;
    let Some(hwnd) = windows.into_iter().next() else {
        return Err("该应用当前没有可显示的顶层窗口。".into());
    };
    if !identity_matches_handle(identity, _handle.0)? || !window_belongs_to(hwnd, identity.pid) {
        return Err("应用窗口归属已经变化，已取消激活。".into());
    }
    if unsafe { IsIconic(hwnd) } != 0 {
        unsafe {
            ShowWindow(hwnd, SW_RESTORE);
        }
    }
    if unsafe { SetForegroundWindow(hwnd) } == 0 {
        return Err("Windows 拒绝切换到该应用；请从桌面手动激活窗口后重试。".into());
    }
    Ok(())
}

struct CheckedApplication {
    identity: ProcessIdentity,
    path: String,
    name: String,
}

fn inspect_pid(pid: u32) -> Result<Option<CheckedApplication>, String> {
    let handle = match open_process(pid, QUERY_RIGHTS) {
        Ok(handle) => handle,
        Err(code) if code == 87 => return Ok(None),
        Err(code) if code == 5 => return Err("Windows 拒绝读取该应用的原生进程身份。".into()),
        Err(code) => {
            return Err(format!(
                "打开 Windows 应用查询 handle 失败（错误码 {code}）。"
            ))
        }
    };
    let _guard = HandleGuard(handle);
    if unsafe { WaitForSingleObject(handle, HANDLE_POLL_MS) } == WAIT_OBJECT_0 {
        return Ok(None);
    }
    let Some(identity) = read_identity_from_handle(pid, handle)? else {
        return Ok(None);
    };
    let Some(executable) = query_executable_path(handle)? else {
        return Ok(None);
    };
    let path = validate_application_path(&executable)?;
    if !same_path(&path, &executable) {
        return Ok(None);
    }
    if unsafe { WaitForSingleObject(handle, HANDLE_POLL_MS) } == WAIT_OBJECT_0 {
        return Ok(None);
    }
    Ok(Some(CheckedApplication {
        identity,
        name: application_name(&path),
        path,
    }))
}

fn checked_handle(
    identity: &ProcessIdentity,
    target_path: &str,
    rights: u32,
) -> Result<HandleGuard, String> {
    let handle = open_process(identity.pid, rights).map_err(|code| match code {
        5 => "当前用户没有控制此应用的权限，且不会自动提权。".to_owned(),
        87 => "关联应用已退出或 PID 不再有效。".to_owned(),
        _ => format!("打开已关联应用 handle 失败（错误码 {code}）。"),
    })?;
    let guard = HandleGuard(handle);
    if unsafe { WaitForSingleObject(guard.0, HANDLE_POLL_MS) } == WAIT_OBJECT_0 {
        return Err("关联应用已退出。".into());
    }
    if !identity_matches_handle(identity, guard.0)? {
        return Err("PID 创建身份已变化，拒绝控制新的进程实例。".into());
    }
    let path = query_executable_path(guard.0)?
        .ok_or_else(|| "无法读取关联应用的可执行文件路径。".to_owned())?;
    if !same_path(&path, target_path) {
        return Err("当前进程可执行路径与关联目标不同，拒绝控制。".into());
    }
    Ok(guard)
}

fn identity_matches_handle(identity: &ProcessIdentity, handle: HANDLE) -> Result<bool, String> {
    Ok(read_identity_from_handle(identity.pid, handle)?
        .map(|current| current == *identity)
        .unwrap_or(false))
}

fn read_identity_from_handle(pid: u32, handle: HANDLE) -> Result<Option<ProcessIdentity>, String> {
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut creation, &mut exit, &mut kernel, &mut user) } == 0 {
        return Err(win_error("读取 Windows 应用原生创建身份失败"));
    }
    if unsafe { WaitForSingleObject(handle, HANDLE_POLL_MS) } == WAIT_OBJECT_0 {
        return Ok(None);
    }
    let created = ((creation.dwHighDateTime as u64) << 32) | creation.dwLowDateTime as u64;
    Ok(Some(ProcessIdentity {
        pid,
        created_at: created.to_string(),
    }))
}

fn query_executable_path(handle: HANDLE) -> Result<Option<String>, String> {
    let mut buffer = vec![0u16; 32768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } == 0 {
        let code = unsafe { GetLastError() };
        if code == 5 {
            return Err("Windows 拒绝读取应用可执行文件路径。".into());
        }
        return Err(format!("读取 Windows 应用路径失败（错误码 {code}）。"));
    }
    if length == 0 || length as usize > buffer.len() {
        return Ok(None);
    }
    Ok(Some(
        OsString::from_wide(&buffer[..length as usize])
            .to_string_lossy()
            .into_owned(),
    ))
}

fn enumerate_windows() -> Result<WindowSnapshot, String> {
    let mut snapshot = WindowSnapshot::default();
    let result = unsafe {
        EnumWindows(
            Some(collect_window),
            (&mut snapshot as *mut WindowSnapshot) as LPARAM,
        )
    };
    if result == 0 {
        return Err(win_error("枚举 Windows 应用窗口失败"));
    }
    Ok(snapshot)
}

unsafe extern "system" fn collect_window(hwnd: HWND, data: LPARAM) -> i32 {
    if hwnd.is_null() || unsafe { IsWindowVisible(hwnd) } == 0 {
        return 1;
    }
    // 忽略 owned popup/工具窗口，避免一个主窗口被其菜单、浮层重复计为多个应用窗口。
    if !unsafe { GetWindow(hwnd, GW_OWNER) }.is_null() {
        return 1;
    }
    let mut pid = 0u32;
    if unsafe { GetWindowThreadProcessId(hwnd, &mut pid) } == 0 || pid == 0 {
        return 1;
    }
    let snapshot = unsafe { &mut *(data as *mut WindowSnapshot) };
    snapshot.by_pid.entry(pid).or_default().push(hwnd);
    1
}

fn windows_for_pid(pid: u32) -> Result<Vec<HWND>, String> {
    Ok(enumerate_windows()?.by_pid.remove(&pid).unwrap_or_default())
}
fn window_belongs_to(hwnd: HWND, pid: u32) -> bool {
    let mut actual = 0;
    unsafe { GetWindowThreadProcessId(hwnd, &mut actual) != 0 && actual == pid }
}

fn build_environment_block(overrides: &BTreeMap<String, String>) -> Result<Vec<u16>, String> {
    let mut values = BTreeMap::<String, (OsString, OsString)>::new();
    for (key, value) in std::env::vars_os() {
        let folded = key.to_string_lossy().to_lowercase();
        if !folded.is_empty() && !folded.contains('\0') && !value.to_string_lossy().contains('\0') {
            values.insert(folded, (key, value));
        }
    }
    for (key, value) in overrides {
        if key.is_empty() || key.contains('=') || key.contains('\0') || value.contains('\0') {
            return Err(format!("Windows 环境变量名称或值无效：{key}"));
        }
        values.insert(
            key.to_lowercase(),
            (OsString::from(key), OsString::from(value)),
        );
    }
    let mut entries = values.into_values().collect::<Vec<_>>();
    entries.sort_by(|left, right| {
        left.0
            .to_string_lossy()
            .to_lowercase()
            .cmp(&right.0.to_string_lossy().to_lowercase())
    });
    let mut block = Vec::new();
    for (key, value) in entries {
        block.extend(key.encode_wide());
        block.push('=' as u16);
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    if block.len() == 1 {
        block.push(0);
    }
    Ok(block)
}

/// CreateProcessW 命令行按 Windows 参数解析惯例转义空格、引号和尾部反斜线。
fn quote_windows_argument(argument: &str) -> String {
    if !argument.is_empty()
        && !argument
            .chars()
            .any(|character| character.is_whitespace() || character == '"')
    {
        return argument.to_owned();
    }
    let mut quoted = String::from("\"");
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                quoted.push_str(&"\\".repeat(backslashes * 2 + 1));
                quoted.push('"');
                backslashes = 0;
            }
            _ => {
                quoted.push_str(&"\\".repeat(backslashes));
                quoted.push(character);
                backslashes = 0;
            }
        }
    }
    quoted.push_str(&"\\".repeat(backslashes * 2));
    quoted.push('"');
    quoted
}

fn open_process(pid: u32, rights: u32) -> Result<HANDLE, u32> {
    let handle = unsafe { OpenProcess(rights, 0, pid) };
    if handle.is_null() {
        Err(unsafe { GetLastError() })
    } else {
        Ok(handle)
    }
}

fn to_wide(value: &str) -> Vec<u16> {
    std::ffi::OsStr::new(value)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

fn application_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| path.to_owned())
}

fn same_path(left: &str, right: &str) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| PathBuf::from(left));
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| PathBuf::from(right));
    left.to_string_lossy()
        .eq_ignore_ascii_case(&right.to_string_lossy())
}

fn win_error(prefix: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{prefix}（Windows 错误码 {code}）。")
}
