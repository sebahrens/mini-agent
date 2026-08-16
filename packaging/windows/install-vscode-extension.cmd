@echo off
setlocal

set "MINI_AGENT_VSIX=%~dp0mini-agent-win32-x64.vsix"
if not exist "%MINI_AGENT_VSIX%" exit /b 0

where code.cmd >nul 2>nul
if not errorlevel 1 (
  call code.cmd --install-extension "%MINI_AGENT_VSIX%" --force >nul 2>nul
  exit /b 0
)

if exist "%LOCALAPPDATA%\Programs\Microsoft VS Code\bin\code.cmd" (
  call "%LOCALAPPDATA%\Programs\Microsoft VS Code\bin\code.cmd" --install-extension "%MINI_AGENT_VSIX%" --force >nul 2>nul
  exit /b 0
)

if exist "%ProgramFiles%\Microsoft VS Code\bin\code.cmd" (
  call "%ProgramFiles%\Microsoft VS Code\bin\code.cmd" --install-extension "%MINI_AGENT_VSIX%" --force >nul 2>nul
  exit /b 0
)

if exist "%ProgramFiles(x86)%\Microsoft VS Code\bin\code.cmd" (
  call "%ProgramFiles(x86)%\Microsoft VS Code\bin\code.cmd" --install-extension "%MINI_AGENT_VSIX%" --force >nul 2>nul
)

exit /b 0
