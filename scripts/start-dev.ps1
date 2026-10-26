[CmdletBinding()]
param (
    [switch]$Check
)

# 所有相对路径均以本脚本所在工程为基准，避免中文或空格路径依赖调用者当前目录。
$ErrorActionPreference = 'Stop'
$script:ProjectRoot = [System.IO.Path]::GetFullPath((Join-Path -Path $PSScriptRoot -ChildPath '..'))
$script:Problems = New-Object System.Collections.ArrayList
$script:NodePath = $null
$script:PnpmPath = $null
$script:CargoPath = $null
$script:RustcPath = $null

function Add-Problem {
    param (
        [Parameter(Mandatory = $true)]
        [string]$Message
    )

    [void]$script:Problems.Add($Message)
}

function Find-ExecutablePath {
    param (
        [Parameter(Mandatory = $true)]
        [string]$Name
    )

    $command = Get-Command -Name $Name -CommandType Application -ErrorAction SilentlyContinue |
        Select-Object -First 1
    if ($null -eq $command) {
        return $null
    }

    if (-not [string]::IsNullOrWhiteSpace($command.Source)) {
        return $command.Source
    }

    return $command.Definition
}

function Invoke-NativeCommandForText {
    param (
        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    # Windows PowerShell 5.1 会把原生命令的 stderr 写入错误流；这里临时允许它继续，
    # 以便同时收集完整输出和 $LASTEXITCODE，再由调用方给出清楚的中文错误。
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        $nativeOutput = & $Path @Arguments 2>&1
        $nativeExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    $outputLines = @()
    if ($null -ne $nativeOutput) {
        $outputLines = @($nativeOutput | ForEach-Object {
            if ($null -eq $_) {
                ''
            }
            else {
                [string]$_
            }
        })
    }

    return [pscustomobject]@{
        ExitCode = $nativeExitCode
        Output   = ($outputLines -join [Environment]::NewLine).Trim()
    }
}

function Invoke-PnpmCommand {
    param (
        [Parameter(Mandatory = $true)]
        [string[]]$Arguments
    )

    # 安装和启动的输出直接显示在控制台，同时从原生命令读取真实退出码供 BAT 传递。
    $previousErrorActionPreference = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & $script:PnpmPath @Arguments 2>&1 | ForEach-Object {
            Write-Host $_
        }
        $nativeExitCode = $LASTEXITCODE
    }
    finally {
        $ErrorActionPreference = $previousErrorActionPreference
    }

    return [int]$nativeExitCode
}

function Test-VersionRequirement {
    param (
        [Parameter(Mandatory = $true)]
        [string]$ToolName,

        [Parameter(Mandatory = $true)]
        [string]$Path,

        [Parameter(Mandatory = $true)]
        [version]$MinimumVersion
    )

    $result = Invoke-NativeCommandForText -Path $Path -Arguments @('--version')
    if ($result.ExitCode -ne 0) {
        Add-Problem "$ToolName 版本检查失败（退出码 $($result.ExitCode)）。"
        return
    }

    if ($result.Output -notmatch '(?m)^\s*[vV]?(?<version>\d+\.\d+\.\d+)\s*$') {
        Add-Problem "$ToolName 输出了无法识别的版本：$($result.Output)"
        return
    }

    $foundVersion = [version]::Parse($Matches['version'])
    if ($foundVersion -lt $MinimumVersion) {
        Add-Problem "$ToolName 版本为 $foundVersion，项目要求 $MinimumVersion 或更高。"
        return
    }

    Write-Host "$ToolName 版本：$foundVersion" -ForegroundColor Green
}

function Test-ProjectFiles {
    foreach ($relativePath in @('package.json', 'pnpm-lock.yaml', 'src-tauri\tauri.conf.json')) {
        $fullPath = Join-Path -Path $script:ProjectRoot -ChildPath $relativePath
        if (-not (Test-Path -LiteralPath $fullPath -PathType Leaf)) {
            Add-Problem "工程文件不存在：$relativePath"
        }
    }
}

function Add-CargoBinToProcessPathIfNeeded {
    # 先保留 PATH 中已经选中的工具链；只在 cargo 或 rustc 缺失时补目录，
    # 并按 CARGO_HOME、USERPROFILE 的顺序追加到 PATH 尾部，避免遮蔽现有自定义工具链。
    $cargoPath = Find-ExecutablePath -Name 'cargo.exe'
    $rustcPath = Find-ExecutablePath -Name 'rustc.exe'
    if (($null -ne $cargoPath) -and ($null -ne $rustcPath)) {
        $script:CargoPath = $cargoPath
        $script:RustcPath = $rustcPath
        return
    }

    $candidateDirectories = @()
    if (-not [string]::IsNullOrWhiteSpace($env:CARGO_HOME)) {
        $candidateDirectories += (Join-Path -Path $env:CARGO_HOME -ChildPath 'bin')
    }
    if (-not [string]::IsNullOrWhiteSpace($env:USERPROFILE)) {
        $candidateDirectories += (Join-Path -Path $env:USERPROFILE -ChildPath '.cargo\bin')
    }

    foreach ($candidateDirectory in $candidateDirectories) {
        if (-not (Test-Path -LiteralPath $candidateDirectory -PathType Container)) {
            continue
        }

        $candidateCargoPath = Join-Path -Path $candidateDirectory -ChildPath 'cargo.exe'
        $candidateRustcPath = Join-Path -Path $candidateDirectory -ChildPath 'rustc.exe'
        $hasMissingTool = (($null -eq $cargoPath) -and (Test-Path -LiteralPath $candidateCargoPath -PathType Leaf)) -or
            (($null -eq $rustcPath) -and (Test-Path -LiteralPath $candidateRustcPath -PathType Leaf))
        if (-not $hasMissingTool) {
            continue
        }

        $normalizedDirectory = $candidateDirectory.TrimEnd('\')
        $alreadyInPath = $false
        foreach ($entry in ($env:PATH -split ';')) {
            if ($entry.Trim().TrimEnd('\') -ieq $normalizedDirectory) {
                $alreadyInPath = $true
                break
            }
        }

        if (-not $alreadyInPath) {
            # 仅调整本次启动进程及其子进程的 PATH，不修改用户或系统环境变量。
            if ([string]::IsNullOrWhiteSpace($env:PATH)) {
                $env:PATH = $candidateDirectory
            }
            else {
                $env:PATH = "$env:PATH;$candidateDirectory"
            }
            Write-Host "已为本次启动补充 $candidateDirectory。"
        }

        if (($null -eq $cargoPath) -and (Test-Path -LiteralPath $candidateCargoPath -PathType Leaf)) {
            $cargoPath = $candidateCargoPath
        }
        if (($null -eq $rustcPath) -and (Test-Path -LiteralPath $candidateRustcPath -PathType Leaf)) {
            $rustcPath = $candidateRustcPath
        }
        if (($null -ne $cargoPath) -and ($null -ne $rustcPath)) {
            break
        }
    }

    $script:CargoPath = $cargoPath
    $script:RustcPath = $rustcPath
}

function Test-Toolchain {
    $script:NodePath = Find-ExecutablePath -Name 'node.exe'
    $script:PnpmPath = Find-ExecutablePath -Name 'pnpm.cmd'
    Add-CargoBinToProcessPathIfNeeded
    if ($null -eq (Find-ExecutablePath -Name 'cmake.exe')) {
        Add-Problem '本地语音引擎需要 CMake；请安装 CMake 并加入 PATH。'
    }

    if ($null -eq $script:NodePath) {
        Add-Problem '找不到 Node.js（node.exe）；需要 Node.js 22.12 或更高版本。'
    }
    else {
        Test-VersionRequirement -ToolName 'Node.js' -Path $script:NodePath -MinimumVersion ([version]'22.12.0')
    }

    if ($null -eq $script:PnpmPath) {
        Add-Problem '找不到 pnpm.cmd；需要 pnpm 11 或更高版本。'
    }
    else {
        Test-VersionRequirement -ToolName 'pnpm' -Path $script:PnpmPath -MinimumVersion ([version]'11.0.0')
    }

    if ($null -eq $script:CargoPath) {
        Add-Problem '找不到 Cargo（cargo.exe）。'
    }
    else {
        $cargoResult = Invoke-NativeCommandForText -Path $script:CargoPath -Arguments @('--version')
        if ($cargoResult.ExitCode -ne 0) {
            Add-Problem "Cargo 无法运行（退出码 $($cargoResult.ExitCode)）。"
        }
        else {
            Write-Host 'Cargo 可用。' -ForegroundColor Green
        }
    }

    if ($null -eq $script:RustcPath) {
        Add-Problem '找不到 Rust 编译器（rustc.exe）。'
    }
    else {
        $rustcResult = Invoke-NativeCommandForText -Path $script:RustcPath -Arguments @('-vV')
        if ($rustcResult.ExitCode -ne 0) {
            Add-Problem "rustc 无法运行（退出码 $($rustcResult.ExitCode)）。"
        }
        elseif ($rustcResult.Output -notmatch '(?m)^release:\s*(?<release>\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?)\s*$') {
            Add-Problem '无法识别 rustc 版本；请安装 Rust 稳定版 MSVC 工具链。'
        }
        else {
            # 先缓存版本，后续正则匹配会覆盖 PowerShell 的 $Matches 自动变量。
            $rustRelease = $Matches['release']
            if ($rustRelease -notmatch '^\d+\.\d+\.\d+$') {
                Add-Problem "rustc 当前版本为 $rustRelease；项目要求 Rust 稳定版。"
            }
            else {
                Write-Host "rustc 稳定版：$rustRelease" -ForegroundColor Green
            }

            if ($rustcResult.Output -notmatch '(?m)^host:\s*[A-Za-z0-9_]+-pc-windows-msvc\s*$') {
                Add-Problem 'rustc 当前目标不是 Windows MSVC 工具链；请安装 Rust 稳定版 MSVC 工具链。'
            }
        }
    }
}

function Write-CheckResult {
    if ($script:Problems.Count -gt 0) {
        Write-Host ''
        Write-Host '环境检查未通过：' -ForegroundColor Red
        foreach ($problem in $script:Problems) {
            Write-Host " - $problem" -ForegroundColor Red
        }
        Write-Host '请先准备项目所需的开发工具后重试；启动器不会自动安装系统工具。'
        return $false
    }

    Write-Host '环境检查通过。' -ForegroundColor Green
    return $true
}

function Start-DesktopDevelopment {
    Set-Location -LiteralPath $script:ProjectRoot -ErrorAction Stop
    Test-ProjectFiles
    Test-Toolchain

    if (-not (Write-CheckResult)) {
        return 1
    }

    if ($Check) {
        Write-Host '检查模式已结束；没有安装依赖或启动桌面程序。'
        return 0
    }

    if ($null -eq $env:KACENT_DATA_DIR) {
        # 未设置数据目录时把调试数据库留在工程内，避免碰到正式应用数据库。
        $developmentDataDirectory = Join-Path -Path $script:ProjectRoot -ChildPath '.local-dev\data'
        [void][System.IO.Directory]::CreateDirectory($developmentDataDirectory)
        $env:KACENT_DATA_DIR = $developmentDataDirectory
        Write-Host "本次调试数据目录：$developmentDataDirectory"
    }
    else {
        # 用户显式指定的数据目录优先保留，且只传递给本次启动的子进程。
        Write-Host "本次沿用已设置的 KACENT_DATA_DIR：$env:KACENT_DATA_DIR"
    }

    Write-Host ''
    Write-Host '正在执行 pnpm install --frozen-lockfile...' -ForegroundColor Cyan
    $installExitCode = Invoke-PnpmCommand -Arguments @('install', '--frozen-lockfile')
    if ($installExitCode -ne 0) {
        Write-Host "依赖安装失败（pnpm 退出码 $installExitCode）；请查看上方输出。" -ForegroundColor Red
        Write-Host '按任意键关闭此窗口。'
        return $installExitCode
    }

    Write-Host ''
    Write-Host '提示：关闭应用窗口后，程序会留在系统托盘；需要退出时，请在托盘菜单选择“停止并退出”。' -ForegroundColor Yellow
    Write-Host '正在执行 pnpm desktop...' -ForegroundColor Cyan
    $desktopExitCode = Invoke-PnpmCommand -Arguments @('desktop')
    if ($desktopExitCode -ne 0) {
        Write-Host "桌面调试启动失败或已异常结束（pnpm 退出码 $desktopExitCode）；请查看上方输出。" -ForegroundColor Red
        Write-Host '按任意键关闭此窗口。'
        return $desktopExitCode
    }

    Write-Host '桌面调试已结束。按任意键关闭此窗口。' -ForegroundColor Green
    return 0
}

try {
    $processExitCode = Start-DesktopDevelopment
}
catch {
    Write-Host "启动器发生错误：$($_.Exception.Message)" -ForegroundColor Red
    Write-Host '按任意键关闭此窗口。'
    $processExitCode = 1
}

exit $processExitCode
