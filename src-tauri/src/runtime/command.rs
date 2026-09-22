//! 命令与环境准备。所有变量只写入即将创建的子进程，不修改 Tauri 进程的全局环境。

use super::{SessionKind, WorkerSpec};
use crate::models::{AgentProfile, ServiceConfig};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
use std::sync::OnceLock;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

#[derive(Clone)]
pub(super) struct ProcessTarget {
    pub executable: String,
    pub args: Vec<String>,
    pub environment: HashMap<String, String>,
}

pub(super) fn build_target(
    config: &ServiceConfig,
    kind: SessionKind,
    agent: Option<&AgentProfile>,
    gui_path: &str,
) -> Result<ProcessTarget, String> {
    let environment = build_environment(config, agent, gui_path);

    if let Some(agent) = agent {
        let path = resolve_executable_in_environment(&agent.executable, &environment)
            .ok_or_else(|| format!("找不到 Agent 可执行文件：{}", agent.executable))?;
        let executable = path.to_string_lossy().into_owned();
        let args = agent.args.clone();
        #[cfg(windows)]
        let (executable, args) = {
            let mut executable = executable;
            let mut args = args;
            if has_script_extension(&path) {
                let mut script_parts = vec![powershell_quote(&executable)];
                script_parts.extend(args.iter().map(|argument| powershell_quote(argument)));
                let script = format!("& {{ & {}; exit $LASTEXITCODE }}", script_parts.join(" "));
                executable = resolve_powershell(&environment);
                args = vec![
                    "-NoLogo".into(),
                    "-NoProfile".into(),
                    "-Command".into(),
                    script,
                ];
            }
            (executable, args)
        };
        return Ok(ProcessTarget {
            executable,
            args,
            environment,
        });
    }

    if matches!(kind, SessionKind::Service) {
        if let crate::models::LaunchConfig::Frontend {
            package_manager,
            script,
            args,
            ..
        } = &config.launch
        {
            // 包管理器包装器也可能隐式下载运行时或依赖。只在本次前端进程环境中关闭这些
            // 行为，不改用户配置；缺少工具或依赖时让启动明确失败，交由用户自行准备。
            let mut environment = environment;
            for (key, value) in [
                ("COREPACK_ENABLE_NETWORK", "0"),
                ("PNPM_CONFIG_VERIFY_DEPS_BEFORE_RUN", "false"),
                ("PNPM_CONFIG_MANAGE_PACKAGE_MANAGER_VERSIONS", "false"),
                ("PNPM_CONFIG_PM_ON_FAIL", "error"),
            ] {
                set_env(&mut environment, key.into(), value.into());
            }
            // 只把脚本名和参数作为 argv 传递；package.json 中的脚本由所选包管理器解释，
            // 不把项目路径、用户参数或脚本原文拼接成未转义的 Shell 代码。
            let path = resolve_executable_in_environment(package_manager, &environment)
                .ok_or_else(|| {
                    format!("找不到包管理器 {package_manager}，请先安装或配置其可执行文件目录。")
                })?;
            let executable = path.to_string_lossy().into_owned();
            let mut arguments = if package_manager == "bun" {
                vec!["run".to_owned(), "--no-install".to_owned(), script.clone()]
            } else {
                vec!["run".to_owned(), script.clone()]
            };
            if package_manager == "npm" && !args.is_empty() {
                arguments.push("--".into());
            }
            arguments.extend(args.iter().cloned());
            #[cfg(windows)]
            let (executable, arguments) = if has_script_extension(&path) {
                let mut parts = vec![powershell_quote(&executable)];
                parts.extend(arguments.iter().map(|argument| powershell_quote(argument)));
                (
                    resolve_powershell(&environment),
                    vec![
                        "-NoLogo".into(),
                        "-NoProfile".into(),
                        "-Command".into(),
                        format!("& {{ & {}; exit $LASTEXITCODE }}", parts.join(" ")),
                    ],
                )
            } else {
                (executable, arguments)
            };
            return Ok(ProcessTarget {
                executable,
                args: arguments,
                environment,
            });
        }
    }

    match kind {
        SessionKind::Service => {
            let shell = resolve_shell(&config.shell, &environment);
            #[cfg(windows)]
            {
                if is_cmd(&shell) {
                    // cmd 的 /D 禁用 AutoRun 注册表脚本，避免用户配置改变服务命令环境。
                    Ok(ProcessTarget {
                        executable: shell,
                        args: vec![
                            "/D".into(),
                            "/S".into(),
                            "/C".into(),
                            config.command.clone(),
                        ],
                        environment,
                    })
                } else if is_powershell(&shell) {
                    // 把 PowerShell 的非终止错误提升为终止错误，保证命令不存在或解析失败
                    // 时不会落到空的 LASTEXITCODE 并误报成功；原生程序仍返回自身退出码。
                    let command = format!(
                        "$ErrorActionPreference = 'Stop'; & {{ {}; exit $LASTEXITCODE }}",
                        config.command
                    );
                    Ok(ProcessTarget {
                        executable: shell,
                        args: vec![
                            "-NoLogo".into(),
                            "-NoProfile".into(),
                            "-Command".into(),
                            command,
                        ],
                        environment,
                    })
                } else {
                    Err(format!(
                        "Windows 服务仅支持 cmd.exe 或 PowerShell，当前 shell 为：{shell}"
                    ))
                }
            }
            #[cfg(unix)]
            {
                Ok(ProcessTarget {
                    executable: shell,
                    args: vec!["-c".into(), config.command.clone()],
                    environment,
                })
            }
        }
        SessionKind::Shell => {
            let shell = resolve_shell(&config.shell, &environment);
            #[cfg(windows)]
            {
                if is_powershell(&shell) {
                    Ok(ProcessTarget {
                        executable: shell,
                        args: vec!["-NoLogo".into(), "-NoProfile".into()],
                        environment,
                    })
                } else if is_cmd(&shell) {
                    Ok(ProcessTarget {
                        executable: shell,
                        args: vec!["/D".into()],
                        environment,
                    })
                } else {
                    Err(format!(
                        "Windows 交互终端仅支持 cmd.exe 或 PowerShell，当前 shell 为：{shell}"
                    ))
                }
            }
            #[cfg(unix)]
            {
                let mut environment = environment;
                let args = interactive_shell_args(&shell)?;
                let shell_name = Path::new(&shell)
                    .file_name()
                    .and_then(OsStr::to_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                configure_interactive_prompt(config, &shell, &mut environment);
                if matches!(shell_name.as_str(), "sh" | "dash") {
                    // POSIX sh 的 ENV 文件会在交互初始化阶段执行，允许它改写隔离变量。
                    unset_env(&mut environment, "ENV");
                }
                Ok(ProcessTarget {
                    executable: shell,
                    args,
                    environment,
                })
            }
        }
        SessionKind::Agent => Err("Agent 会话缺少 Agent 配置。".into()),
    }
}

pub(super) fn resolve_agent(
    agent: &AgentProfile,
    config: Option<&ServiceConfig>,
    gui_path: Option<&str>,
) -> Option<PathBuf> {
    let gui_path = gui_path.map(str::to_owned).unwrap_or_else(macos_gui_path);
    let environment = match config {
        Some(config) => build_environment(config, Some(agent), &gui_path),
        None => {
            let mut environment = std::env::vars().collect::<HashMap<_, _>>();
            for (key, value) in &agent.env {
                set_env(&mut environment, key.clone(), value.clone());
            }
            if !gui_path.is_empty() && !agent.env.keys().any(|key| env_key_eq(key, "PATH")) {
                set_env(&mut environment, "PATH".into(), gui_path);
            }
            environment
        }
    };
    resolve_executable_in_environment(&agent.executable, &environment)
}

/// macOS 应用由 Finder 启动时通常拿不到终端 PATH。只读取登录 shell 打印的 PATH，
/// 严格限制在 2 秒内，并缓存结果，避免在服务或 Agent 配置里执行 shell 启动逻辑。
pub(super) fn macos_gui_path() -> String {
    static GUI_PATH: OnceLock<String> = OnceLock::new();
    GUI_PATH
        .get_or_init(|| {
            #[cfg(target_os = "macos")]
            {
                read_macos_login_path().unwrap_or_default()
            }
            #[cfg(not(target_os = "macos"))]
            {
                String::new()
            }
        })
        .clone()
}

fn build_environment(
    config: &ServiceConfig,
    agent: Option<&AgentProfile>,
    gui_path: &str,
) -> HashMap<String, String> {
    let mut environment = std::env::vars().collect::<HashMap<_, _>>();
    for (key, value) in &config.env {
        set_env(&mut environment, key.clone(), value.clone());
    }
    if let Some(agent) = agent {
        for (key, value) in &agent.env {
            set_env(&mut environment, key.clone(), value.clone());
        }
    }

    // Finder 启动的 macOS GUI 常带一个过短但非空的 PATH。以登录 PATH 为默认基底；
    // 服务或 Agent 显式配置 PATH 时保留其值，再由环境目录前缀补充到最前面。
    let path_is_configured = config.env.keys().any(|key| env_key_eq(key, "PATH"))
        || agent
            .map(|agent| agent.env.keys().any(|key| env_key_eq(key, "PATH")))
            .unwrap_or(false);
    if !gui_path.is_empty() && !path_is_configured {
        set_env(&mut environment, "PATH".into(), gui_path.to_owned());
    }

    let mut prefixes = config
        .executable_dirs
        .iter()
        .map(PathBuf::from)
        .collect::<Vec<_>>();
    if !config.environment_dir.is_empty() {
        let root = PathBuf::from(&config.environment_dir);
        match config.environment_type.as_str() {
            "venv" => {
                #[cfg(windows)]
                prefixes.push(root.join("Scripts"));
                #[cfg(unix)]
                prefixes.push(root.join("bin"));
                set_env(
                    &mut environment,
                    "VIRTUAL_ENV".into(),
                    config.environment_dir.clone(),
                );
            }
            "custom" => {
                // custom 目录常见布局直接纳入自身及 bin/Scripts；不存在的目录不会被创建。
                prefixes.push(root.clone());
                prefixes.push(root.join("bin"));
                prefixes.push(root.join("Scripts"));
            }
            _ => {}
        }
    }

    let base_path = get_env(&environment, "PATH").unwrap_or_default();
    let mut path_entries = prefixes
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|path| !path.is_empty())
        .collect::<Vec<_>>();
    path_entries.extend(
        std::env::split_paths(OsStr::new(&base_path))
            .map(|path| path.to_string_lossy().into_owned()),
    );
    if let Ok(path) = std::env::join_paths(path_entries.iter().map(OsStr::new)) {
        set_env(
            &mut environment,
            "PATH".into(),
            path.to_string_lossy().into_owned(),
        );
    }
    environment
}

fn env_key_eq(left: &str, right: &str) -> bool {
    if cfg!(windows) {
        left.eq_ignore_ascii_case(right)
    } else {
        left == right
    }
}

fn set_env(environment: &mut HashMap<String, String>, key: String, value: String) {
    if cfg!(windows) {
        if let Some(existing) = environment
            .keys()
            .find(|existing| existing.eq_ignore_ascii_case(&key))
            .cloned()
        {
            environment.remove(&existing);
        }
    }
    environment.insert(key, value);
}

fn unset_env(environment: &mut HashMap<String, String>, key: &str) {
    if let Some(existing) = environment
        .keys()
        .find(|existing| env_key_eq(existing, key))
        .cloned()
    {
        environment.remove(&existing);
    }
}

fn get_env(environment: &HashMap<String, String>, key: &str) -> Option<String> {
    environment
        .iter()
        .find(|(name, _)| {
            if cfg!(windows) {
                name.eq_ignore_ascii_case(key)
            } else {
                name.as_str() == key
            }
        })
        .map(|(_, value)| value.clone())
}

fn resolve_shell(configured: &str, environment: &HashMap<String, String>) -> String {
    let configured = configured.trim();
    if !configured.is_empty() {
        if let Some(path) = resolve_executable_in_environment(configured, environment) {
            return path.to_string_lossy().into_owned();
        }
        return configured.to_owned();
    }
    #[cfg(windows)]
    {
        resolve_powershell(environment)
    }
    #[cfg(unix)]
    {
        get_env(environment, "SHELL")
            .filter(|shell| Path::new(shell).is_file())
            .unwrap_or_else(|| "/bin/sh".into())
    }
}

#[cfg(unix)]
fn interactive_shell_args(shell: &str) -> Result<Vec<String>, String> {
    let name = Path::new(shell)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match name.as_str() {
        // 关闭用户 rc/profile，避免交互初始化覆盖服务独立设置的 PATH、cwd 与 venv。
        "bash" => Ok(vec!["--noprofile".into(), "--norc".into(), "-i".into()]),
        "zsh" => Ok(vec!["-f".into(), "-i".into()]),
        "fish" => Ok(vec!["--no-config".into(), "-i".into()]),
        "csh" | "tcsh" => Ok(vec!["-f".into(), "-i".into()]),
        // POSIX sh/dash 不加载交互启动文件；清理潜在的 ENV 由调用端完成。
        "sh" | "dash" => Ok(vec!["-i".into()]),
        _ => Err(format!(
            "无法保证所选交互 shell 的启动脚本不会覆盖隔离环境：{shell}"
        )),
    }
}

/// 为禁用用户启动脚本的交互 shell 提供带动态工作目录的提示符。
///
/// 交互 shell 使用各自的无配置参数启动，因而 bash/zsh 会退回到不含 cwd 的
/// 默认提示符。这里传入 shell 原生支持的提示符展开式，而不是把配置中的工作
/// 目录写死到环境变量；这样用户执行 `cd` 后，下一次提示符会由 shell 根据真实
/// 当前目录重新展开，同时不会加载用户 rc 文件或破坏隔离的 PATH/venv。
#[cfg(unix)]
fn configure_interactive_prompt(
    config: &ServiceConfig,
    shell: &str,
    environment: &mut HashMap<String, String>,
) {
    let shell_name = Path::new(shell)
        .file_name()
        .and_then(OsStr::to_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    match shell_name.as_str() {
        // bash 在 PS1 中支持 \\w（当前目录，HOME 下路径会缩写为 ~）和 \\$（用户/特权
        // 提示符）。值会由 bash 在每次打印提示符时展开，因此 cd 后不会停留在旧路径。
        "bash" => {
            // 用户在服务配置中显式设置 PS1 时保留其选择；宿主继承的 PS1 则由隔离
            // 终端的默认提示符替换。PROMPT 对 bash 不起作用，可以原样保留。
            if !config.env.keys().any(|key| env_key_eq(key, "PS1")) {
                set_env(environment, "PS1".into(), r"\w \$ ".into());
            }
        }
        // zsh 使用 PROMPT（PS1 是其别名）；%~ 同样按当前目录动态展开，%# 保留普通
        // 用户与特权用户的提示符区别。zsh 会把 PS1 与 PROMPT 导入为同一个 shell
        // 参数，两个变量同时存在时结果会受 HashMap 遍历顺序影响，因此这里先归一化。
        "zsh" => {
            let configured_prompt = config
                .env
                .iter()
                .find(|(key, _)| env_key_eq(key, "PROMPT"))
                .map(|(_, value)| value.clone());
            let configured_ps1 = config
                .env
                .iter()
                .find(|(key, _)| env_key_eq(key, "PS1"))
                .map(|(_, value)| value.clone());
            match (configured_prompt, configured_ps1) {
                (Some(prompt), Some(_ps1)) => {
                    // 两者都显式配置时定义 PROMPT 优先，删除 PS1，避免 zsh 按导入顺序
                    // 产生不确定结果。
                    unset_env(environment, "PS1");
                    set_env(environment, "PROMPT".into(), prompt);
                }
                (Some(prompt), None) => {
                    // 只显式配置 PROMPT 时清除宿主继承的 PS1，避免别名覆盖自定义值。
                    unset_env(environment, "PS1");
                    set_env(environment, "PROMPT".into(), prompt);
                }
                (None, Some(ps1)) => {
                    // 只显式配置 PS1 时保留它，并清除另一个继承的别名。
                    unset_env(environment, "PROMPT");
                    set_env(environment, "PS1".into(), ps1);
                }
                (None, None) => {
                    // 没有显式配置时清除宿主 PS1，再注入默认动态 PROMPT；此时不会把
                    // 配置工作目录写入提示符，cd 后仍由 zsh 读取真实 cwd。
                    unset_env(environment, "PS1");
                    set_env(environment, "PROMPT".into(), "%~ %# ".into());
                }
            }
        }
        // POSIX sh/dash 没有 bash 的 \\w 扩展，但会在每次交互提示时重新展开命令替换。
        // pwd 是 shell 内建命令；提示符只读取真实 cwd，不执行用户启动脚本。
        "sh" | "dash" => {
            // PROMPT 对 POSIX sh 不起作用；只要用户没有显式设置 PS1，就可以安全地
            // 替换宿主继承值。
            if !config.env.keys().any(|key| env_key_eq(key, "PS1")) {
                set_env(environment, "PS1".into(), "$(pwd) \\$ ".into());
            }
        }
        _ => {}
    }
}

pub(super) fn resolve_executable_in_environment(
    executable: &str,
    environment: &HashMap<String, String>,
) -> Option<PathBuf> {
    let executable = executable.trim();
    if executable.is_empty() {
        return None;
    }
    let path = PathBuf::from(executable);
    let contains_separator = executable.contains('/') || executable.contains('\\');
    if path.is_absolute() || contains_separator {
        return candidate_executable(&path);
    }

    let search_path = get_env(environment, "PATH").unwrap_or_default();
    for directory in std::env::split_paths(OsStr::new(&search_path)) {
        let direct = directory.join(executable);
        if let Some(found) = candidate_executable(&direct) {
            return Some(found);
        }
        #[cfg(windows)]
        if Path::new(executable).extension().is_none() {
            let extensions =
                get_env(environment, "PATHEXT").unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".into());
            for extension in extensions.split(';') {
                let candidate = directory.join(format!("{executable}{extension}"));
                if let Some(found) = candidate_executable(&candidate) {
                    return Some(found);
                }
            }
        }
    }
    None
}

fn candidate_executable(path: &Path) -> Option<PathBuf> {
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return None;
        }
    }
    #[cfg(windows)]
    {
        if !matches!(
            path.extension()
                .and_then(OsStr::to_str)
                .unwrap_or("")
                .to_ascii_lowercase()
                .as_str(),
            "exe" | "com" | "bat" | "cmd"
        ) {
            return None;
        }
    }
    Some(path.to_path_buf())
}

#[cfg(windows)]
fn is_powershell(path: &str) -> bool {
    Path::new(path)
        .file_stem()
        .and_then(OsStr::to_str)
        .map(|name| name.eq_ignore_ascii_case("powershell") || name.eq_ignore_ascii_case("pwsh"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_cmd(path: &str) -> bool {
    Path::new(path)
        .file_stem()
        .and_then(OsStr::to_str)
        .map(|name| name.eq_ignore_ascii_case("cmd"))
        .unwrap_or(false)
}

#[cfg(windows)]
fn resolve_powershell(environment: &HashMap<String, String>) -> String {
    resolve_executable_in_environment("pwsh.exe", environment)
        .or_else(|| resolve_executable_in_environment("powershell.exe", environment))
        .map(|path| path.to_string_lossy().into_owned())
        .unwrap_or_else(|| "powershell.exe".into())
}

#[cfg(windows)]
fn has_script_extension(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(OsStr::to_str)
            .unwrap_or("")
            .to_ascii_lowercase()
            .as_str(),
        "cmd" | "bat"
    )
}

#[cfg(windows)]
fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(target_os = "macos")]
fn read_macos_login_path() -> Option<String> {
    use std::io::{ErrorKind, Read};
    use std::os::fd::AsRawFd;

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut child = Command::new(shell)
        // nvm 等用户工具经常配置在 .zshrc 而非 .zprofile。使用交互登录 shell 只取 PATH，
        // 否则 GUI 会误用 /usr/local/bin 中的旧 Node，导致新版包管理器无法启动。
        // 输出使用独立标记，忽略 shell 欢迎词和退出钩子；不把其他环境变量带回主进程。
        .args([
            "-i",
            "-l",
            "-c",
            "command printf '\\036KACENT_PATH:%s\\037' \"$PATH\"",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    };
    // 非阻塞读取让 PATH 探测严格限时；登录脚本即使留下继承管道的后台进程也不会卡住 GUI。
    let fd = stdout.as_raw_fd();
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 || unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        let _ = child.kill();
        let _ = child.wait();
        return None;
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut eof = false;
    let mut exited = false;
    'probe: while Instant::now() < deadline {
        loop {
            if bytes.len() >= 2 * 1024 * 1024 || Instant::now() >= deadline {
                break 'probe;
            }
            match stdout.read(&mut chunk) {
                Ok(0) => {
                    eof = true;
                    break;
                }
                Ok(count) => bytes.extend_from_slice(&chunk[..count]),
                Err(error) if error.kind() == ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == ErrorKind::WouldBlock => break,
                Err(_) => {
                    eof = true;
                    break;
                }
            }
        }
        match child.try_wait() {
            Ok(Some(_)) => {
                exited = true;
                if eof {
                    break;
                }
            }
            Ok(None) => {}
            Err(_) => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    if !exited {
        let _ = child.kill();
        let _ = child.wait();
    }
    let output = String::from_utf8_lossy(&bytes);
    output
        .rsplit_once("\u{1e}KACENT_PATH:")
        .and_then(|(_, value)| value.split_once('\u{1f}').map(|(path, _)| path))
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
}

#[cfg(not(target_os = "macos"))]
fn read_macos_login_path() -> Option<String> {
    None
}

#[allow(dead_code)]
fn worker_spec(target: &ProcessTarget, working_dir: &str) -> WorkerSpec {
    WorkerSpec {
        executable: target.executable.clone(),
        args: target.args.clone(),
        working_dir: working_dir.to_owned(),
        environment: target.environment.clone(),
    }
}

#[cfg(windows)]
pub(super) fn wrap_windows_worker(
    target: ProcessTarget,
    working_dir: &str,
) -> Result<ProcessTarget, String> {
    let helper = std::env::current_exe()
        .map_err(|error| format!("无法定位 Windows 终端引导程序：{error}"))?;
    let encoded = serde_json::to_string(&worker_spec(&target, working_dir))
        .map_err(|error| format!("无法准备 Windows 终端目标配置：{error}"))?;
    let mut environment = target.environment;
    set_env(
        &mut environment,
        super::windows_worker::WORKER_SPEC_ENV.into(),
        encoded,
    );
    Ok(ProcessTarget {
        executable: helper.to_string_lossy().into_owned(),
        args: vec![super::windows_worker::WORKER_ARGUMENT.into()],
        environment,
    })
}
