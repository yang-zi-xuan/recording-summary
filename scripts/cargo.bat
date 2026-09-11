@echo off
REM ---------------------------------------------------------------------------
REM cargo 构建包装脚本
REM
REM 为什么需要它:本机 Rust 是 x86_64-pc-windows-msvc,链接需要 MSVC 的
REM link.exe 与 Windows SDK,但它们默认不在 PATH 上,必须先加载 vcvars64.bat。
REM 没有这一步会报 "can't find crate for `core`" 之类的误导性错误。
REM
REM 用法:
REM   scripts\cargo.bat build
REM   scripts\cargo.bat test
REM   scripts\cargo.bat run -- probe
REM ---------------------------------------------------------------------------
setlocal

set "VCVARS=C:\Program Files\Microsoft Visual Studio\2022\Community\VC\Auxiliary\Build\vcvars64.bat"
if not exist "%VCVARS%" (
    echo [错误] 未找到 vcvars64.bat: %VCVARS%
    echo        请修改本脚本中的 VCVARS 路径。
    exit /b 1
)

call "%VCVARS%" >nul 2>&1
if errorlevel 1 (
    echo [错误] 加载 MSVC 环境失败
    exit /b 1
)

cd /d "%~dp0.."
cargo %*
exit /b %ERRORLEVEL%
