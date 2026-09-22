//! macOS 桌面应用适配层：通过 Launch Services/AppKit 操作 NSRunningApplication。
//! 不调用 AppleScript/osascript，也不向进程发送信号作为正常退出的替代路径。

use super::{NativeApplication, NativeEnumeration};
use crate::models::{ApplicationInspection, ProcessIdentity};
use block2::RcBlock;
use objc2_app_kit::{
    NSApplicationActivationOptions, NSRunningApplication, NSWorkspace, NSWorkspaceOpenConfiguration,
};
use objc2_foundation::{NSArray, NSAutoreleasePool, NSDictionary, NSError, NSString, NSURL};
use std::collections::BTreeMap;
use std::ffi::{c_char, c_void};
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

const PROC_PIDTBSDINFO: i32 = 3;
const STATUS_ZOMBIE: u32 = 5;

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
    fn proc_pidinfo(pid: i32, flavor: i32, arg: u64, buffer: *mut c_void, buffersize: i32) -> i32;
}

/// 枚举 Launch Services 正在跟踪的应用实例；窗口数使用本机窗口快照按 owner PID 统计。
pub(super) fn enumerate() -> Result<NativeEnumeration, String> {
    let _pool = unsafe { NSAutoreleasePool::new() };
    let running = NSWorkspace::sharedWorkspace().runningApplications();
    let windows = window_counts();
    let mut applications = Vec::new();
    let mut warnings = Vec::new();
    let mut hidden_count = 0usize;

    for index in 0..running.count() {
        let application = running.objectAtIndex(index);
        let pid = application.processIdentifier();
        if pid <= 0 || pid as u32 == std::process::id() || application.isTerminated() {
            continue;
        }
        let Some(url) = application.bundleURL() else {
            // Launch Services 可能返回没有 bundle 的后台代理；不能只凭进程名猜测应用路径。
            continue;
        };
        let Some(path_value) = url.path() else {
            continue;
        };
        let raw_path = path_value.to_string();
        let Ok(path) = validate_application_path(&raw_path) else {
            continue;
        };
        let identity = match read_identity(pid as u32) {
            Ok(Some(identity)) => identity,
            Ok(None) => continue,
            Err(error) => {
                if warnings.len() < 8 {
                    warnings.push(format!("PID {pid}：{error}"));
                } else {
                    hidden_count += 1;
                }
                continue;
            }
        };
        let name = application
            .localizedName()
            .map(|name| name.to_string())
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| application_name(&path));
        let bundle_id = application
            .bundleIdentifier()
            .map(|value| value.to_string());
        applications.push(NativeApplication {
            window_count: windows.get(&(pid as u32)).copied().unwrap_or_default(),
            identity,
            path,
            name,
            bundle_id,
        });
    }

    Ok(NativeEnumeration {
        applications,
        warning: (!warnings.is_empty()).then(|| {
            if hidden_count > 0 {
                warnings.push(format!("另有 {hidden_count} 个应用身份不可读"));
            }
            format!(
                "部分 macOS 应用因系统权限无法核验；当前列表不完整。{}",
                warnings.join("；")
            )
        }),
    })
}

pub(super) fn can_request_quit(_window_count: u32) -> bool {
    // NSRunningApplication 可向无窗口的后台应用发起正常退出请求；是否接受由应用自行决定。
    true
}

pub(super) fn inspect_identity(
    identity: &ProcessIdentity,
) -> Result<Option<NativeApplication>, String> {
    let _pool = unsafe { NSAutoreleasePool::new() };
    let Some(current) = read_identity(identity.pid)? else {
        return Ok(None);
    };
    if current.created_at != identity.created_at {
        return Ok(None);
    }
    let Some(application) =
        NSRunningApplication::runningApplicationWithProcessIdentifier(identity.pid as libc::pid_t)
    else {
        return Err("进程创建身份仍存在，但 Launch Services 尚未返回对应应用实例。".into());
    };
    if application.isTerminated() {
        return Err("Launch Services 正在更新应用状态，暂时无法确认该实例是否退出。".into());
    }
    let Some(url) = application.bundleURL() else {
        return Err("该 PID 仍运行，但没有可核验的 macOS 应用包路径。".into());
    };
    let Some(path_value) = url.path() else {
        return Err("无法读取已运行应用的 bundle 路径。".into());
    };
    let path = validate_application_path(&path_value.to_string())?;
    let windows = window_counts();
    let name = application
        .localizedName()
        .map(|name| name.to_string())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| application_name(&path));
    let bundle_id = application
        .bundleIdentifier()
        .map(|value| value.to_string());

    // 回读系统进程创建时间，避免读取 bundle 元数据期间 PID 已被复用。
    let Some(after) = read_identity(identity.pid)? else {
        return Ok(None);
    };
    if after != current {
        return Ok(None);
    }
    Ok(Some(NativeApplication {
        identity: current,
        path,
        name,
        bundle_id,
        window_count: windows.get(&identity.pid).copied().unwrap_or_default(),
    }))
}

pub(super) fn validate_application_path(path: &str) -> Result<String, String> {
    let source = PathBuf::from(path.trim());
    if !source.is_absolute() {
        return Err("macOS 应用路径必须是绝对路径。".into());
    }
    let canonical = std::fs::canonicalize(&source)
        .map_err(|error| format!("无法访问 macOS 应用路径 {}：{error}", source.display()))?;
    if !canonical.is_dir()
        || !canonical
            .extension()
            .and_then(|extension| extension.to_str())
            .map(|extension| extension.eq_ignore_ascii_case("app"))
            .unwrap_or(false)
        || !canonical.join("Contents/Info.plist").is_file()
        || !canonical.join("Contents/MacOS").is_dir()
    {
        return Err(
            "请选择完整的 macOS .app 应用包（需包含 Contents/Info.plist 和可执行目录）。".into(),
        );
    }
    Ok(canonical.to_string_lossy().into_owned())
}

pub(super) fn inspect_path(path: &str) -> Result<ApplicationInspection, String> {
    let canonical = validate_application_path(path)?;
    let name = application_name(&canonical);
    let bundle_id = inspect_bundle_id(&canonical);
    Ok(ApplicationInspection {
        path: canonical,
        name,
        bundle_id,
        platform: "macos".into(),
    })
}

pub(super) fn application_executable(_app_path: &str) -> Option<String> {
    // .app 的真实可执行文件名由其 Info.plist 决定；运行实例的可靠路径来自 libproc。
    None
}

pub(super) fn launch(
    path: &str,
    args: &[String],
    _start_dir: &str,
    environment: &BTreeMap<String, String>,
) -> Result<ProcessIdentity, String> {
    let canonical = validate_application_path(path)?;
    let _pool = unsafe { NSAutoreleasePool::new() };
    let url = NSURL::fileURLWithPath(&NSString::from_str(&canonical));
    let configuration = NSWorkspaceOpenConfiguration::configuration();
    // 同路径正在运行的唯一实例已在公共层筛选；这里仍允许 Launch Services 按默认规则复用。
    configuration.setCreatesNewApplicationInstance(false);
    configuration.setActivates(true);

    let argument_strings = args
        .iter()
        .map(|argument| NSString::from_str(argument))
        .collect::<Vec<_>>();
    let argument_refs = argument_strings
        .iter()
        .map(|argument| &**argument)
        .collect::<Vec<_>>();
    let argument_array = NSArray::from_slice(&argument_refs);
    configuration.setArguments(&argument_array);

    // NSWorkspace 要求配置一个完整启动环境；继承当前用户进程环境后仅覆盖用户明确设置的项。
    let mut merged_environment = std::env::vars_os()
        .filter_map(|(key, value)| Some((key.into_string().ok()?, value.into_string().ok()?)))
        .collect::<BTreeMap<_, _>>();
    merged_environment.extend(
        environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone())),
    );
    let environment_keys = merged_environment
        .keys()
        .map(|key| NSString::from_str(key))
        .collect::<Vec<_>>();
    let environment_values = merged_environment
        .values()
        .map(|value| NSString::from_str(value))
        .collect::<Vec<_>>();
    let key_refs = environment_keys
        .iter()
        .map(|value| &**value)
        .collect::<Vec<_>>();
    let value_refs = environment_values
        .iter()
        .map(|value| &**value)
        .collect::<Vec<_>>();
    let environment_dictionary = NSDictionary::from_slices(&key_refs, &value_refs);
    configuration.setEnvironment(&environment_dictionary);

    let (sender, receiver) = mpsc::channel();
    let completion = RcBlock::new(
        move |application: *mut NSRunningApplication, error: *mut NSError| {
            let result = if application.is_null() {
                let message = if error.is_null() {
                    "Launch Services 未返回应用实例。".into()
                } else {
                    unsafe { (&*error).localizedDescription().to_string() }
                };
                Err(message)
            } else {
                // 回调参数只在 block 调用期间借用；仅发送 PID，随后仍用原生创建时间复核。
                Ok(unsafe { (&*application).processIdentifier() as u32 })
            };
            let _ = sender.send(result);
        },
    );
    NSWorkspace::sharedWorkspace().openApplicationAtURL_configuration_completionHandler(
        &url,
        &configuration,
        Some(&completion),
    );

    let deadline = Instant::now() + Duration::from_secs(30);
    let mut callback_pid = None;
    while Instant::now() < deadline {
        if callback_pid.is_none() {
            match receiver.recv_timeout(Duration::from_millis(80)) {
                Ok(Ok(pid)) if pid != 0 => callback_pid = Some(pid),
                Ok(Err(error)) => return Err(format!("macOS 拒绝启动该应用：{error}")),
                Ok(_) => return Err("Launch Services 拒绝启动该应用。".into()),
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return Err("macOS 应用启动回调已中断。".into());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
        }
        if let Some(pid) = callback_pid {
            if let Some(identity) = read_identity(pid)? {
                if let Some(application) = inspect_identity(&identity)? {
                    if same_canonical_path(&application.path, &canonical) {
                        return Ok(identity);
                    }
                }
            }
        }
        thread::sleep(Duration::from_millis(80));
    }
    Err("macOS 未能在 30 秒内返回可核验的应用实例；请检查系统启动提示并重新发现。".into())
}

pub(super) fn request_quit(identity: &ProcessIdentity, target_path: &str) -> Result<(), String> {
    let _pool = unsafe { NSAutoreleasePool::new() };
    let application = checked_running_application(identity, target_path)?;
    if application.terminate() {
        Ok(())
    } else {
        Err("macOS 未接受正常退出请求；应用可能正在显示未保存提示。".into())
    }
}

pub(super) fn force_quit(identity: &ProcessIdentity, target_path: &str) -> Result<(), String> {
    let _pool = unsafe { NSAutoreleasePool::new() };
    let application = checked_running_application(identity, target_path)?;
    if application.forceTerminate() {
        Ok(())
    } else {
        Err("macOS 未接受对已核验实例的强制结束请求。".into())
    }
}

pub(super) fn focus(identity: &ProcessIdentity, target_path: &str) -> Result<(), String> {
    let _pool = unsafe { NSAutoreleasePool::new() };
    let application = checked_running_application(identity, target_path)?;
    let options = NSApplicationActivationOptions::ActivateAllWindows;
    if application.activateWithOptions(options) {
        Ok(())
    } else {
        Err("系统拒绝激活该应用；请检查是否有可显示窗口或当前桌面权限。".into())
    }
}

fn checked_running_application(
    identity: &ProcessIdentity,
    target_path: &str,
) -> Result<objc2::rc::Retained<NSRunningApplication>, String> {
    let current = inspect_identity(identity)?
        .ok_or_else(|| "关联应用已退出或 PID 创建身份已变化。".to_owned())?;
    if !same_canonical_path(&current.path, target_path) {
        return Err("应用路径与已关联目标不一致，拒绝执行原生操作。".into());
    }
    NSRunningApplication::runningApplicationWithProcessIdentifier(identity.pid as libc::pid_t)
        .ok_or_else(|| "macOS 找不到已核验的 NSRunningApplication 实例。".into())
}

fn read_identity(pid: u32) -> Result<Option<ProcessIdentity>, String> {
    let native_pid = i32::try_from(pid).map_err(|_| "应用 PID 超出 macOS 支持范围。")?;
    let Some(first) = read_bsd_info(native_pid)? else {
        return Ok(None);
    };
    if first.pbi_pid != pid || first.pbi_status == STATUS_ZOMBIE {
        return Ok(None);
    }
    let Some(second) = read_bsd_info(native_pid)? else {
        return Ok(None);
    };
    if second.pbi_pid != pid
        || second.pbi_status == STATUS_ZOMBIE
        || start_time(&first) != start_time(&second)
    {
        return Ok(None);
    }
    Ok(Some(ProcessIdentity {
        pid,
        created_at: start_time(&first),
    }))
}

fn read_bsd_info(pid: i32) -> Result<Option<ProcBsdInfo>, String> {
    let mut info = ProcBsdInfo::default();
    unsafe {
        *libc::__error() = 0;
    }
    let result = unsafe {
        proc_pidinfo(
            pid,
            PROC_PIDTBSDINFO,
            0,
            (&mut info as *mut ProcBsdInfo).cast::<c_void>(),
            std::mem::size_of::<ProcBsdInfo>() as i32,
        )
    };
    if result == std::mem::size_of::<ProcBsdInfo>() as i32 {
        return Ok(Some(info));
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(libc::ESRCH) | Some(libc::ENOENT) => Ok(None),
        Some(libc::EPERM) | Some(libc::EACCES) => {
            Err("macOS 拒绝读取该应用的原生进程创建身份。".into())
        }
        Some(code) => Err(format!("读取 macOS 进程身份失败（错误码 {code}）。")),
        None => Err("macOS 未返回进程身份，暂不能确认应用是否退出。".into()),
    }
}

fn start_time(info: &ProcBsdInfo) -> String {
    info.pbi_start_tvsec
        .saturating_mul(1_000_000)
        .saturating_add(info.pbi_start_tvusec)
        .to_string()
}

fn window_counts() -> BTreeMap<u32, u32> {
    use objc2_core_foundation::{CFArray, CFDictionary, CFNumber, CFString, CFType};
    use objc2_core_graphics::{
        kCGNullWindowID, kCGWindowOwnerPID, CGWindowListCopyWindowInfo, CGWindowListOption,
    };

    let Some(windows) = CGWindowListCopyWindowInfo(CGWindowListOption::OptionAll, kCGNullWindowID)
    else {
        return BTreeMap::new();
    };
    let windows_ptr: *const CFArray = &*windows;
    let windows = unsafe { &*windows_ptr.cast::<CFArray<CFDictionary<CFString, CFType>>>() };
    let mut counts = BTreeMap::new();
    for window in windows.iter() {
        // 每项是系统生成的不可变字典；只读取 owner PID，不尝试控制任何窗口或提权。
        let Some(owner_number) = (unsafe { window.get_unchecked(kCGWindowOwnerPID) })
            .and_then(|owner| owner.downcast_ref::<CFNumber>())
        else {
            continue;
        };
        let mut pid = 0i32;
        if unsafe {
            owner_number.value(
                objc2_core_foundation::CFNumberType::SInt32Type,
                (&mut pid as *mut i32).cast(),
            )
        } && pid > 0
        {
            *counts.entry(pid as u32).or_insert(0) += 1;
        }
    }
    counts
}

fn inspect_bundle_id(path: &str) -> Option<String> {
    // 不解析 plist 的原始二進位格式，也不啟動外部 plutil；当前未运行 bundle 的 ID 由 nil 表示。
    let _ = path;
    None
}

fn application_name(path: &str) -> String {
    Path::new(path)
        .file_stem()
        .map(|name| name.to_string_lossy().into_owned())
        .filter(|name| !name.trim().is_empty())
        .unwrap_or_else(|| path.to_owned())
}

fn same_canonical_path(left: &str, right: &str) -> bool {
    let left = std::fs::canonicalize(left).unwrap_or_else(|_| PathBuf::from(left));
    let right = std::fs::canonicalize(right).unwrap_or_else(|_| PathBuf::from(right));
    left == right
}
