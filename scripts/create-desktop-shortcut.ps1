# 在桌面创建「录音转总结」快捷方式。
#
# 由 create-desktop-shortcut.cmd 调用(它会带 -ExecutionPolicy Bypass,
# 所以不需要手动改执行策略)。
#
# 编码:本文件建议保存为 **UTF-8 with BOM**。
# Windows PowerShell 5.1 会把无 BOM 的 .ps1 按 GBK 解码,
# 里面的中文会被打乱,快捷方式名就成了乱码。

$ErrorActionPreference = 'Stop'

# 项目根 = 本脚本所在目录的上一级
$repo = Split-Path -Parent $PSScriptRoot
$exe = Join-Path $repo 'target\debug\rs-gui.exe'
$icon = Join-Path $repo 'src-tauri\icons\icon.ico'

Write-Host "项目目录 : $repo"
Write-Host "程序     : $exe"
Write-Host ""

if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host "❌ 找不到 rs-gui.exe" -ForegroundColor Red
    Write-Host "   请先构建:scripts\cargo.ps1 build"
    exit 1
}

# 图标缺失不致命 —— 退回用 exe 自带的图标
if (-not (Test-Path -LiteralPath $icon)) {
    Write-Host "⚠ 找不到 icon.ico,快捷方式将使用程序自带图标"
    $icon = $exe
}

# 残留进程会锁住 exe,让下次构建报「拒绝访问」—— 顺手提示
$running = Get-Process rs-gui -ErrorAction SilentlyContinue
if ($running) {
    Write-Host "⚠ rs-gui 正在运行(PID $($running.Id -join ', '))"
    Write-Host "  之后构建若报「拒绝访问」,先执行:"
    Write-Host "    Get-Process rs-gui | Stop-Process -Force"
    Write-Host ""
}

# --- 找桌面 ---------------------------------------------------------------
#
# ⚠️ 不能只看 [Environment]::GetFolderPath('Desktop')。
#    桌面被 OneDrive 重定向时,这个 API 与实际位置可能不一致 ——
#    结果快捷方式建到了用户没在看的地方,表现为"没看到图标"。
#
# 所以列出所有候选,**挑第一个真实存在的目录**。

$candidates = @()
$api = [Environment]::GetFolderPath('Desktop')
if ($api) { $candidates += $api }
if ($env:OneDrive) { $candidates += (Join-Path $env:OneDrive 'Desktop') }
if ($env:OneDriveConsumer) { $candidates += (Join-Path $env:OneDriveConsumer 'Desktop') }
$candidates += (Join-Path $env:USERPROFILE 'Desktop')
$candidates += 'C:\Users\Public\Desktop'
$candidates = $candidates | Where-Object { $_ } | Select-Object -Unique

$desktop = $null
foreach ($c in $candidates) {
    if (Test-Path -LiteralPath $c) {
        $desktop = $c
        break
    }
}

if (-not $desktop) {
    Write-Host "❌ 找不到桌面目录。候选位置都不存在:" -ForegroundColor Red
    $candidates | ForEach-Object { Write-Host "     $_" }
    exit 1
}

Write-Host "桌面目录 : $desktop"
Write-Host ""

# --- 创建 -------------------------------------------------------------

$lnk = Join-Path $desktop '录音转总结.lnk'

$shell = New-Object -ComObject WScript.Shell
$s = $shell.CreateShortcut($lnk)
$s.TargetPath = $exe
# ★ 工作目录必须是项目根 —— 程序按相对路径找 models/ 与 binaries/。
#   paths.rs 有多位置搜索兜底,但设对了最稳。
$s.WorkingDirectory = $repo
$s.IconLocation = "$icon,0"
$s.Description = '录音转总结 —— 本地转写,云端纪要'
$s.WindowStyle = 1
$s.Save()

if (Test-Path -LiteralPath $lnk) {
    Write-Host "✅ 已创建快捷方式" -ForegroundColor Green
    Write-Host "   $lnk"
    Write-Host ""

    # 把解析出来的属性回读一遍 —— 用户能立刻看出对不对
    $check = $shell.CreateShortcut($lnk)
    Write-Host "回读确认:"
    Write-Host "   目标     : $($check.TargetPath)"
    Write-Host "   起始位置 : $($check.WorkingDirectory)"
    Write-Host "   图标     : $($check.IconLocation)"
    Write-Host ""
    Write-Host "双击桌面上的「录音转总结」即可启动。"
} else {
    Write-Host "❌ 创建失败" -ForegroundColor Red
    exit 1
}
