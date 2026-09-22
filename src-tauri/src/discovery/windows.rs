//! Windows 原生身份、Authenticode 发布者与 Service Control Manager 只读查询。
//!
//! 查询与终止共用同一个进程 handle 来核验 FILETIME 创建身份，确保即使 PID 后续被复用，
//! `TerminateProcess` 仍只作用于最初打开并核验的那个内核进程对象。

use super::{NativeProcessInfo, ServiceHostInfo};
use crate::models::ProcessIdentity;
use std::collections::HashMap;
use std::ffi::c_void;
use std::hash::{Hash, Hasher};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, FILETIME, HANDLE, WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows_sys::Win32::Security::Cryptography::{
    CertGetNameStringW, CERT_NAME_SIMPLE_DISPLAY_TYPE,
};
use windows_sys::Win32::Security::WinTrust::{
    WTHelperGetProvCertFromChain, WTHelperGetProvSignerFromChain, WTHelperProvDataFromStateData,
    WinVerifyTrust, WINTRUST_ACTION_GENERIC_VERIFY_V2, WINTRUST_DATA, WINTRUST_FILE_INFO,
    WTD_CHOICE_FILE, WTD_REVOCATION_CHECK_NONE, WTD_REVOKE_NONE, WTD_STATEACTION_CLOSE,
    WTD_STATEACTION_VERIFY, WTD_UI_NONE,
};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_NORMAL,
    FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Services::{
    CloseServiceHandle, EnumServicesStatusExW, OpenSCManagerW, SC_ENUM_PROCESS_INFO, SC_HANDLE,
    SC_MANAGER_ENUMERATE_SERVICE, SERVICE_STATE_ALL, SERVICE_WIN32, SERVICE_WIN32_SHARE_PROCESS,
};
use windows_sys::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, QueryFullProcessImageNameW, TerminateProcess,
    WaitForSingleObject, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE, PROCESS_TERMINATE,
};

const ERROR_ACCESS_DENIED_CODE: u32 = 5;
const ERROR_INVALID_PARAMETER_CODE: u32 = 87;
const ERROR_MORE_DATA_CODE: u32 = 234;

/// 打开有限权限的进程 handle 并读取精确 Windows FILETIME 创建时间。
pub(crate) fn query_process(pid: u32) -> Result<Option<NativeProcessInfo>, String> {
    let handle = match open_process(pid, PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE) {
        Ok(handle) => handle,
        Err(code) if code == ERROR_INVALID_PARAMETER_CODE => return Ok(None),
        Err(code) if code == ERROR_ACCESS_DENIED_CODE => {
            return Err("Windows 拒绝读取该进程的原生创建身份。".into())
        }
        Err(code) => {
            return Err(format!(
                "打开 Windows 进程查询 handle 失败（错误码 {code}）。"
            ))
        }
    };
    let _guard = HandleGuard(handle);

    match unsafe { WaitForSingleObject(handle, 0) } {
        WAIT_OBJECT_0 => return Ok(None),
        WAIT_TIMEOUT => {}
        WAIT_FAILED => return Err(win_error("检查 Windows 进程是否仍运行失败")),
        _ => return Err("Windows 返回了未知的进程等待状态。".into()),
    }

    let Some(created_at) = creation_time(handle)? else {
        return Ok(None);
    };
    let executable = query_executable_path(handle);

    // 同一 handle 永远指向同一个进程对象；离开函数前再确认它没有在查询期间退出。
    if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
        return Ok(None);
    }
    Ok(Some(NativeProcessInfo {
        created_at,
        executable,
        parent_pid: None,
    }))
}

/// 探测当前令牌是否能打开带 TERMINATE 权限的 handle，不会请求权限或触发 UAC。
pub(crate) fn can_terminate(identity: &ProcessIdentity) -> Result<bool, String> {
    let handle = match open_process(
        identity.pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
    ) {
        Ok(handle) => handle,
        Err(code) if code == ERROR_ACCESS_DENIED_CODE || code == ERROR_INVALID_PARAMETER_CODE => {
            return Ok(false)
        }
        Err(code) => return Err(format!("检查 Windows 停止权限失败（错误码 {code}）。")),
    };
    let _guard = HandleGuard(handle);
    if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
        return Ok(false);
    }
    Ok(creation_time(handle)?.as_deref() == Some(identity.created_at.as_str()))
}

/// 用经过创建时间复核的进程 handle 只终止单个 PID，不接触 Job、子进程或进程组。
pub(crate) fn terminate(identity: &ProcessIdentity) -> Result<(), String> {
    let handle = open_process(
        identity.pid,
        PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
    )
    .map_err(|code| match code {
        ERROR_ACCESS_DENIED_CODE => {
            "当前用户没有停止该 Windows 进程的权限，且不会自动提权。".into()
        }
        ERROR_INVALID_PARAMETER_CODE => "进程已退出，已拒绝停止。".into(),
        code => format!("打开 Windows 进程停止 handle 失败（错误码 {code}）。"),
    })?;
    let _guard = HandleGuard(handle);

    if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
        return Err("进程已退出，已拒绝停止。".into());
    }
    let actual = creation_time(handle)?
        .ok_or_else(|| "Windows 无法读取进程创建身份，已拒绝停止。".to_owned())?;
    if actual != identity.created_at {
        return Err("PID 已对应到其他 Windows 进程，已拒绝停止。".into());
    }

    if unsafe { TerminateProcess(handle, 1) } == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_ACCESS_DENIED_CODE {
            return Err("当前用户没有停止该 Windows 进程的权限，且不会自动提权。".into());
        }
        return Err(format!("TerminateProcess 失败（Windows 错误码 {code}）。"));
    }
    match unsafe { WaitForSingleObject(handle, 5_000) } {
        WAIT_OBJECT_0 => Ok(()),
        WAIT_TIMEOUT => Err("Windows 已请求停止进程，但未在 5 秒内确认退出。".into()),
        WAIT_FAILED => Err(win_error("等待 Windows 进程退出失败")),
        _ => Err("Windows 返回了未知的进程退出等待状态。".into()),
    }
}

/// 根据系统目录、文件身份与签名验证结果识别 Windows 内建二进制。
///
/// 只有位于 Windows 受保护系统子目录且由 Microsoft Windows 发布者签名的二进制，
/// 才被分类为 system；普通 Authenticode 可信链或第三方受信签名不足以证明系统归属。
pub(super) fn is_protected_system_path(path: &Path) -> bool {
    let windows_root = std::env::var_os("SystemRoot")
        .or_else(|| std::env::var_os("WINDIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"));
    let candidates = [
        windows_root.join("System32"),
        windows_root.join("SysWOW64"),
        windows_root.join("WinSxS"),
        windows_root.join("SystemApps"),
    ];
    let normalized_path = normalize_windows_path(path);
    candidates.iter().any(|root| {
        let normalized_root = normalize_windows_path(root);
        normalized_path == normalized_root
            || normalized_path
                .strip_prefix(&normalized_root)
                .map(|suffix| suffix.starts_with('\\') || suffix.starts_with('/'))
                .unwrap_or(false)
    })
}

/// 返回 true 仅表示有效的 Microsoft Windows 发布者签名，false 表示已验证但不可信归属，
/// None 表示文件身份/签名状态不可读取；结果按真实文件身份缓存，避免周期扫描重复验签。
pub(super) fn authenticode_trusted(path: &Path) -> Option<bool> {
    // 系统组件分类需要可复核的文件身份。canonicalize 或 Win32 文件 ID 读取失败时
    // 不做签名分类，调用方将保守地标记 unknown。
    let canonical_path = std::fs::canonicalize(path).ok()?;
    let fingerprint = file_fingerprint(&canonical_path)?;
    let cache = SIGNATURE_CACHE.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(cache) = cache.lock() {
        if let Some(trusted) = cache.get(&fingerprint) {
            return Some(*trusted);
        }
    }

    let trusted = verify_microsoft_windows_signature(&canonical_path);
    let mut cache = cache.lock().unwrap_or_else(|error| error.into_inner());
    if cache.len() >= 512 {
        cache.clear();
    }
    cache.insert(fingerprint, trusted);
    Some(trusted)
}

#[derive(Clone, Debug, Eq)]
struct FileFingerprint {
    path: String,
    volume_serial: u32,
    file_index: u64,
    file_size: u64,
    creation_time: u64,
    last_write_time: u64,
}

impl PartialEq for FileFingerprint {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.volume_serial == other.volume_serial
            && self.file_index == other.file_index
            && self.file_size == other.file_size
            && self.creation_time == other.creation_time
            && self.last_write_time == other.last_write_time
    }
}

impl Hash for FileFingerprint {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.path.hash(state);
        self.volume_serial.hash(state);
        self.file_index.hash(state);
        self.file_size.hash(state);
        self.creation_time.hash(state);
        self.last_write_time.hash(state);
    }
}

static SIGNATURE_CACHE: OnceLock<Mutex<HashMap<FileFingerprint, bool>>> = OnceLock::new();

/// 通过稳定的 Win32 文件卷序列号与文件索引识别被签名的文件，避免同路径替换后复用旧缓存。
fn file_fingerprint(path: &Path) -> Option<FileFingerprint> {
    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let handle = unsafe {
        CreateFileW(
            wide_path.as_ptr(),
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            null(),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            null_mut(),
        )
    };
    if handle.is_null() || handle == (-1isize as HANDLE) {
        return None;
    }
    let _guard = HandleGuard(handle);
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    if unsafe { GetFileInformationByHandle(handle, &mut info) } == 0 {
        return None;
    }
    let to_u64 =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some(FileFingerprint {
        path: normalize_windows_path(path),
        volume_serial: info.dwVolumeSerialNumber,
        file_index: (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        file_size: (u64::from(info.nFileSizeHigh) << 32) | u64::from(info.nFileSizeLow),
        creation_time: to_u64(info.ftCreationTime),
        last_write_time: to_u64(info.ftLastWriteTime),
    })
}

/// 使用 Windows Trust API 校验签名链，再读取链首 signer 的证书主题确认 Microsoft Windows。
fn verify_microsoft_windows_signature(path: &Path) -> bool {
    let wide_path = path
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect::<Vec<_>>();
    let mut file_info = WINTRUST_FILE_INFO::default();
    file_info.cbStruct = size_of::<WINTRUST_FILE_INFO>() as u32;
    file_info.pcwszFilePath = wide_path.as_ptr();

    let mut data = WINTRUST_DATA::default();
    data.cbStruct = size_of::<WINTRUST_DATA>() as u32;
    data.dwUIChoice = WTD_UI_NONE;
    data.fdwRevocationChecks = WTD_REVOKE_NONE;
    data.dwUnionChoice = WTD_CHOICE_FILE;
    data.Anonymous.pFile = &mut file_info;
    data.dwStateAction = WTD_STATEACTION_VERIFY;
    data.dwProvFlags = WTD_REVOCATION_CHECK_NONE;

    let mut action = WINTRUST_ACTION_GENERIC_VERIFY_V2;
    let status = unsafe {
        WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        )
    };
    let microsoft_windows = status == 0 && signer_is_microsoft_windows(data.hWVTStateData);

    // VERIFY 会分配 Trust Provider 状态；即使签名失败也显式 CLOSE，避免每次扫描泄漏句柄。
    data.dwStateAction = WTD_STATEACTION_CLOSE;
    unsafe {
        let _ = WinVerifyTrust(
            null_mut(),
            &mut action,
            (&mut data as *mut WINTRUST_DATA).cast::<c_void>(),
        );
    }
    microsoft_windows
}

/// 从成功的 Trust Provider 状态中读取链首证书主题，不把第三方可信发布者当作 Windows 组件。
fn signer_is_microsoft_windows(state: HANDLE) -> bool {
    if state.is_null() {
        return false;
    }
    let provider = unsafe { WTHelperProvDataFromStateData(state) };
    if provider.is_null() {
        return false;
    }
    let signer = unsafe { WTHelperGetProvSignerFromChain(provider, 0, 0, 0) };
    if signer.is_null() {
        return false;
    }
    let certificate = unsafe { WTHelperGetProvCertFromChain(signer, 0) };
    if certificate.is_null() {
        return false;
    }
    let context = unsafe { (*certificate).pCert };
    if context.is_null() {
        return false;
    }

    let required = unsafe {
        CertGetNameStringW(
            context,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            null(),
            null_mut(),
            0,
        )
    } as usize;
    if required < 2 || required > 4096 {
        return false;
    }
    let mut name = vec![0u16; required];
    let written = unsafe {
        CertGetNameStringW(
            context,
            CERT_NAME_SIMPLE_DISPLAY_TYPE,
            0,
            null(),
            name.as_mut_ptr(),
            name.len() as u32,
        )
    } as usize;
    if written < 2 || written > name.len() {
        return false;
    }
    let publisher = String::from_utf16_lossy(&name[..written.saturating_sub(1)])
        .trim()
        .to_ascii_lowercase();
    matches!(
        publisher.as_str(),
        "microsoft windows" | "microsoft windows publisher"
    )
}

/// 枚举 SCM 当前运行的 Win32 服务，按 PID 聚合服务名称与共享进程类型，只读不发控制请求。
pub(super) fn running_services() -> Result<HashMap<u32, ServiceHostInfo>, String> {
    let manager = unsafe { OpenSCManagerW(null(), null(), SC_MANAGER_ENUMERATE_SERVICE) };
    if manager.is_null() {
        return Err(win_error("打开 Windows 服务登记表失败"));
    }
    let _guard = ServiceHandleGuard(manager);

    let mut services = HashMap::<u32, ServiceHostInfo>::new();
    let mut resume = 0u32;
    let mut storage = vec![0usize; 16 * 1024];
    loop {
        let buffer_bytes = (storage.len() * size_of::<usize>()) as u32;
        let mut bytes_needed = 0u32;
        let mut returned = 0u32;
        let ok = unsafe {
            EnumServicesStatusExW(
                manager,
                SC_ENUM_PROCESS_INFO,
                SERVICE_WIN32,
                SERVICE_STATE_ALL,
                storage.as_mut_ptr().cast::<u8>(),
                buffer_bytes,
                &mut bytes_needed,
                &mut returned,
                &mut resume,
                null(),
            )
        };
        let error = if ok == 0 {
            unsafe { GetLastError() }
        } else {
            0
        };
        if ok == 0 && error != ERROR_MORE_DATA_CODE {
            return Err(format!("读取 Windows 服务登记失败（错误码 {error}）。"));
        }

        let entries = storage
            .as_ptr()
            .cast::<windows_sys::Win32::System::Services::ENUM_SERVICE_STATUS_PROCESSW>();
        for index in 0..returned as usize {
            let entry = unsafe { entries.add(index).read() };
            let pid = entry.ServiceStatusProcess.dwProcessId;
            if pid == 0 {
                continue;
            }
            let host = services.entry(pid).or_default();
            let name = unsafe { wide_pointer_to_string(entry.lpServiceName) };
            if !name.is_empty() {
                host.service_names.push(name);
            }
            host.shared |=
                entry.ServiceStatusProcess.dwServiceType & SERVICE_WIN32_SHARE_PROCESS != 0;
        }

        if ok != 0 || resume == 0 {
            break;
        }
        if returned == 0 && bytes_needed as usize > storage.len() * size_of::<usize>() {
            let words = (bytes_needed as usize).div_ceil(size_of::<usize>()).max(1);
            storage.resize(words, 0);
        }
    }

    for host in services.values_mut() {
        host.service_names.sort();
        host.service_names.dedup();
        host.shared |= host.service_names.len() > 1;
    }
    Ok(services)
}

unsafe fn wide_pointer_to_string(pointer: *const u16) -> String {
    if pointer.is_null() {
        return String::new();
    }
    let mut length = 0usize;
    while length < 1024 && unsafe { *pointer.add(length) } != 0 {
        length += 1;
    }
    String::from_utf16_lossy(unsafe { std::slice::from_raw_parts(pointer, length) })
}

fn normalize_windows_path(path: &Path) -> String {
    let mut path = path.to_string_lossy().replace('/', "\\");
    // Win32 extended-length 普通路径前缀是 `\\?\\`；UNC 形式要去掉 `UNC` 标签并
    // 保留双反斜线主机名。剩余路径仍按 Windows 大小写不敏感规则比较。
    if let Some(stripped) = path.strip_prefix("\\\\?\\UNC\\") {
        path = format!("\\\\{stripped}");
    } else if let Some(stripped) = path.strip_prefix("\\\\?\\") {
        path = stripped.to_owned();
    }
    while path.ends_with('\\') && path.len() > 3 {
        path.pop();
    }
    path.to_ascii_lowercase()
}

fn open_process(pid: u32, access: u32) -> Result<HANDLE, u32> {
    let handle = unsafe { OpenProcess(access, 0, pid) };
    if handle.is_null() {
        Err(unsafe { GetLastError() })
    } else {
        Ok(handle)
    }
}

fn creation_time(handle: HANDLE) -> Result<Option<String>, String> {
    let mut created = FILETIME::default();
    let mut exited = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    if unsafe { GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user) } == 0 {
        let code = unsafe { GetLastError() };
        if code == ERROR_ACCESS_DENIED_CODE {
            return Err("Windows 拒绝读取进程创建时间。".into());
        }
        return Err(format!("读取 Windows 进程创建时间失败（错误码 {code}）。"));
    }
    let ticks = (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime);
    if ticks == 0 {
        return Ok(None);
    }
    Ok(Some(ticks.to_string()))
}

fn query_executable_path(handle: HANDLE) -> Option<String> {
    let mut buffer = vec![0u16; 32_768];
    let mut length = buffer.len() as u32;
    if unsafe { QueryFullProcessImageNameW(handle, 0, buffer.as_mut_ptr(), &mut length) } == 0
        || length == 0
        || length as usize > buffer.len()
    {
        return None;
    }
    Some(String::from_utf16_lossy(&buffer[..length as usize]))
}

fn win_error(context: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{context}（Windows 错误码 {code}）。")
}

struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = CloseHandle(self.0);
            }
        }
    }
}

struct ServiceHandleGuard(SC_HANDLE);

impl Drop for ServiceHandleGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = CloseServiceHandle(self.0);
            }
        }
    }
}
