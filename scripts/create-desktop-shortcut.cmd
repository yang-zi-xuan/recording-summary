@echo off
rem ===================================================================
rem  Create a desktop shortcut for the app.
rem
rem  This file is deliberately ASCII-only: cmd.exe on a zh-CN Windows
rem  decodes .cmd files as GBK, so Chinese text here would be mangled.
rem  The Chinese shortcut name lives in the .ps1 (which is UTF-8 BOM).
rem
rem  Why a .cmd wrapper instead of asking the user to run the .ps1:
rem  PowerShell's default execution policy blocks unsigned .ps1 files,
rem  and instructing users to pass -ExecutionPolicy Bypass is a bad habit.
rem ===================================================================
setlocal

set "HERE=%~dp0"
set "PS1=%HERE%create-desktop-shortcut.ps1"

if not exist "%PS1%" (
  echo [ERROR] Not found: %PS1%
  pause
  exit /b 1
)

powershell -NoProfile -ExecutionPolicy Bypass -File "%PS1%"

echo.
pause
