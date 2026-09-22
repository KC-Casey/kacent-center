//! Windows 进程树隔离引导器。
//!
//! portable-pty 的 ChildKiller 只能终止 PTY 根进程。这里让 PTY 运行一个很小的同程序
//! helper；helper 先创建 Job Object，再以 suspended 状态创建目标、AssignProcessToJobObject
//! 后才 ResumeThread。helper 被关闭时 Job 句柄随之关闭，受管进程树由系统整体回收。

use super::WorkerSpec;
use std::ffi::{c_void, OsStr};
use std::mem::size_of;
use std::os::windows::ffi::OsStrExt;
use std::ptr::{null, null_mut};
use windows_sys::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_FAILED};
use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE};
use windows_sys::Win32::Storage::FileSystem::{
    CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows_sys::Win32::System::Console::{
    GetConsoleMode, SetConsoleCtrlHandler, CTRL_BREAK_EVENT, CTRL_C_EVENT,
};
use windows_sys::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
    SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows_sys::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, INFINITE, PROCESS_INFORMATION, STARTUPINFOW,
};

pub(super) const WORKER_ARGUMENT: &str = "--kacent-runtime-worker";
pub(super) const WORKER_SPEC_ENV: &str = "KACENT_RUNTIME_WORKER_SPEC";
pub(super) const WORKER_BOOTSTRAP_ERROR_EXIT_CODE: u32 = 0xE000_2001;

/// 由主程序启动路径在 Tauri 初始化前识别 helper，读取一次性进程描述并等待目标退出。
pub(super) fn run_from_environment() -> Result<u32, String> {
    verify_conpty_console()?;
    install_graceful_interrupt_handler()?;
    let encoded = std::env::var(WORKER_SPEC_ENV)
        .map_err(|_| "Windows PTY 引导进程缺少目标配置。".to_owned())?;
    std::env::remove_var(WORKER_SPEC_ENV);
    let spec: WorkerSpec = serde_json::from_str(&encoded)
        .map_err(|error| format!("Windows PTY 目标配置无效：{error}"))?;
    validate_spec(&spec)?;
    run_target_in_job(&spec)
}

/// 终端输入中的 Ctrl+C/Ctrl+Break 会送到共享 ConPTY 的控制台进程。helper 只吞掉发给
/// 自己的控制事件并继续等待目标，让目标仍有机会按自己的处理逻辑清理；管理器等候最多
/// 五秒后才关闭 helper 的 Job Object。这里注册具名 handler，而不用 NULL/TRUE 的全局
/// 忽略标志，避免把“忽略 Ctrl+C”属性传给其启动的目标进程。
fn install_graceful_interrupt_handler() -> Result<(), String> {
    let installed = unsafe { SetConsoleCtrlHandler(Some(handle_control_event), 1) } != 0;
    if installed {
        Ok(())
    } else {
        Err(win_error("注册 Windows PTY 中断处理器失败"))
    }
}

unsafe extern "system" fn handle_control_event(control_type: u32) -> i32 {
    if control_type == CTRL_C_EVENT || control_type == CTRL_BREAK_EVENT {
        1
    } else {
        0
    }
}

/// Tauri release 本身是 GUI 子系统程序，不能假设它自动有 console。ConPTY helper 必须
/// 确认伪控制台输入、输出设备都可打开并通过 GetConsoleMode，未附着时绝不启动目标进程。
fn verify_conpty_console() -> Result<(), String> {
    let input_name = wide_nul(OsStr::new("CONIN$"));
    let output_name = wide_nul(OsStr::new("CONOUT$"));
    let input = unsafe {
        CreateFileW(
            input_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if input == INVALID_HANDLE_VALUE {
        return Err("Windows PTY 引导进程未附着到 ConPTY：无法打开 CONIN$。".into());
    }
    let input_guard = HandleGuard(input);
    let output = unsafe {
        CreateFileW(
            output_name.as_ptr(),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            null(),
            OPEN_EXISTING,
            0,
            null_mut(),
        )
    };
    if output == INVALID_HANDLE_VALUE {
        return Err("Windows PTY 引导进程未附着到 ConPTY：无法打开 CONOUT$。".into());
    }
    let output_guard = HandleGuard(output);
    let mut input_mode = 0;
    let mut output_mode = 0;
    let input_ok = unsafe { GetConsoleMode(input, &mut input_mode) } != 0;
    let output_ok = unsafe { GetConsoleMode(output, &mut output_mode) } != 0;
    drop(output_guard);
    drop(input_guard);
    if !input_ok || !output_ok {
        return Err("Windows PTY 引导进程未附着到有效 ConPTY console。".into());
    }
    Ok(())
}

fn run_target_in_job(spec: &WorkerSpec) -> Result<u32, String> {
    let job = unsafe { CreateJobObjectW(null(), null()) };
    if job.is_null() {
        return Err(win_error("创建 Windows 进程 Job 失败"));
    }
    let job_guard = HandleGuard(job);

    let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    let configured = unsafe {
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast::<c_void>(),
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    };
    if configured == 0 {
        return Err(win_error("配置 Windows 进程 Job 失败"));
    }

    let application = wide_nul(OsStr::new(&spec.executable));
    let current_directory = wide_nul(OsStr::new(&spec.working_dir));
    let mut command_line = command_line(spec)?;
    let mut environment = environment_block(&spec.environment)?;
    let mut startup = STARTUPINFOW::default();
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    let mut process = PROCESS_INFORMATION::default();

    // 暂停目标初始线程，直到进程已属于 Job；目标无法在隔离建立前创建孙进程。
    let created = unsafe {
        CreateProcessW(
            application.as_ptr(),
            command_line.as_mut_ptr(),
            null(),
            null(),
            0,
            CREATE_SUSPENDED | CREATE_UNICODE_ENVIRONMENT,
            environment.as_mut_ptr().cast::<c_void>(),
            current_directory.as_ptr(),
            &startup,
            &mut process,
        )
    };
    if created == 0 {
        return Err(win_error("创建受管 Windows 目标进程失败"));
    }
    let process_guard = HandleGuard(process.hProcess);
    let thread_guard = HandleGuard(process.hThread);

    if unsafe { AssignProcessToJobObject(job, process.hProcess) } == 0 {
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = WaitForSingleObject(process.hProcess, INFINITE);
        }
        return Err(win_error("将目标进程加入 Windows Job 失败"));
    }
    if unsafe { ResumeThread(process.hThread) } == u32::MAX {
        unsafe {
            let _ = TerminateProcess(process.hProcess, 1);
            let _ = WaitForSingleObject(process.hProcess, INFINITE);
        }
        return Err(win_error("恢复受管 Windows 目标进程失败"));
    }

    let waited = unsafe { WaitForSingleObject(process.hProcess, INFINITE) };
    if waited == WAIT_FAILED {
        // 函数离开时关闭 Job 会清理目标及其后代，不会留下失去管理的进程。
        return Err(win_error("等待受管 Windows 目标进程失败"));
    }
    let mut exit_code = 1u32;
    if unsafe { GetExitCodeProcess(process.hProcess, &mut exit_code) } == 0 {
        return Err(win_error("读取受管 Windows 目标退出码失败"));
    }

    // 句柄按作用域关闭；Job 在目标自然退出后关闭，不影响已经结束的进程。
    drop(thread_guard);
    drop(process_guard);
    drop(job_guard);
    Ok(exit_code)
}

fn validate_spec(spec: &WorkerSpec) -> Result<(), String> {
    if spec.executable.is_empty() || spec.working_dir.is_empty() {
        return Err("Windows PTY 目标缺少可执行文件或工作目录。".into());
    }
    let contains_nul = spec.executable.contains('\0')
        || spec.working_dir.contains('\0')
        || spec.args.iter().any(|argument| argument.contains('\0'))
        || spec
            .environment
            .iter()
            .any(|(key, value)| key.contains('\0') || key.contains('=') || value.contains('\0'));
    if contains_nul {
        return Err("Windows PTY 目标参数包含无效的 NUL 或环境变量名称。".into());
    }
    Ok(())
}

fn command_line(spec: &WorkerSpec) -> Result<Vec<u16>, String> {
    // Windows 将整条命令行交给目标自行解析；按 CommandLineToArgvW 规则处理反斜杠与引号。
    let mut arguments = Vec::with_capacity(spec.args.len() + 1);
    arguments.push(spec.executable.as_str());
    arguments.extend(spec.args.iter().map(String::as_str));
    let encoded = arguments
        .iter()
        .map(|argument| quote_windows_argument(argument))
        .collect::<Vec<_>>()
        .join(" ");
    if encoded.contains('\0') {
        return Err("Windows PTY 命令行包含无效 NUL 字符。".into());
    }
    Ok(OsStr::new(&encoded).encode_wide().chain(Some(0)).collect())
}

fn quote_windows_argument(argument: &str) -> String {
    let mut quoted = String::with_capacity(argument.len() + 2);
    quoted.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        if character == '\\' {
            backslashes += 1;
        } else if character == '"' {
            quoted.extend(std::iter::repeat('\\').take(backslashes * 2 + 1));
            quoted.push('"');
            backslashes = 0;
        } else {
            quoted.extend(std::iter::repeat('\\').take(backslashes));
            quoted.push(character);
            backslashes = 0;
        }
    }
    quoted.extend(std::iter::repeat('\\').take(backslashes * 2));
    quoted.push('"');
    quoted
}

fn environment_block(
    environment: &std::collections::HashMap<String, String>,
) -> Result<Vec<u16>, String> {
    let mut values = environment.iter().collect::<Vec<_>>();
    values.sort_by(|left, right| {
        left.0
            .to_ascii_uppercase()
            .cmp(&right.0.to_ascii_uppercase())
    });
    let mut block = Vec::new();
    for (key, value) in values {
        let entry = format!("{key}={value}");
        if entry.contains('\0') {
            return Err("Windows PTY 环境变量包含无效 NUL 字符。".into());
        }
        block.extend(OsStr::new(&entry).encode_wide());
        block.push(0);
    }
    if block.is_empty() {
        block.push(0);
    }
    block.push(0);
    Ok(block)
}

fn wide_nul(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain(Some(0)).collect()
}

fn win_error(context: &str) -> String {
    let code = unsafe { GetLastError() };
    format!("{context}（Windows 错误码 {code}）")
}

const INVALID_HANDLE_VALUE: HANDLE = -1isize as HANDLE;

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
