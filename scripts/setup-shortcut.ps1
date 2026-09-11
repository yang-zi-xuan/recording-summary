# 一次搞定:加 BOM + 创建快捷方式。
#
# 为什么需要这个:
#  1. 用文件工具写入的 .ps1 不带 BOM,而 Windows PowerShell 5.1 会把
#     无 BOM 的 .ps1 按 GBK 解码 —— 里面的中文会被打乱。
#  2. PowerShell 的 `cd` 只切目录不切盘,跨盘必须用 -LiteralPath。
#     这里全部用绝对路径,不受当前工作目录影响。
#
# 用法(把这一段整块粘进 PowerShell):
#   & "D:\program\recording summary\scripts\setup-shortcut.ps1"

$ErrorActionPreference = 'Stop'

$repo = 'D:\program\recording summary'
$ps1  = Join-Path $repo 'scripts\create-desktop-shortcut.ps1'

Write-Host "=== 1. 给创建脚本加 UTF-8 BOM ==="
if (-not (Test-Path -LiteralPath $ps1)) {
    Write-Host "❌ 找不到 $ps1" -ForegroundColor Red
    exit 1
}
$text = [IO.File]::ReadAllText($ps1, [Text.Encoding]::UTF8)
[IO.File]::WriteAllText($ps1, $text, (New-Object Text.UTF8Encoding($true)))
$b = [IO.File]::ReadAllBytes($ps1)
$hasBom = ($b.Length -ge 3 -and $b[0] -eq 0xEF -and $b[1] -eq 0xBB -and $b[2] -eq 0xBF)
Write-Host ("   前3字节: {0}  {1}" -f (($b[0..2] | ForEach-Object { $_.ToString('X2') }) -join ' '),
                                          $(if ($hasBom) { '✅' } else { '❌' }))
Write-Host ""

Write-Host "=== 2. 检查构建产物 ==="
$exe = Join-Path $repo 'target\debug\rs-gui.exe'
if (Test-Path -LiteralPath $exe) {
    $f = Get-Item -LiteralPath $exe
    Write-Host ("   ✅ rs-gui.exe  {0:N0} bytes,修改于 {1}" -f $f.Length, $f.LastWriteTime)
} else {
    Write-Host "   ❌ 不存在,请先执行: .\scripts\cargo.ps1 build" -ForegroundColor Red
    exit 1
}
Write-Host ""

Write-Host "=== 3. 创建快捷方式 ==="
& $ps1
