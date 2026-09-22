@echo off
setlocal EnableExtensions DisableDelayedExpansion
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0scripts\start-dev.ps1"
set "exitCode=%ERRORLEVEL%"
pause
exit /b %exitCode%
