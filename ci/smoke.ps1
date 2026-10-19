# 冒烟测试（Windows）：ci/smoke.ps1 -Bin <congminal.exe> -Shot <截图路径>
# 检查内容和 smoke.sh 相同。
param(
    [Parameter(Mandatory)][string]$Bin,
    [Parameter(Mandatory)][string]$Shot
)
$ErrorActionPreference = 'Stop'

New-Item -ItemType Directory -Force -Path (Split-Path $Shot) | Out-Null
$env:CONGMINAL_SHELL = "pwsh -NoProfile -ExecutionPolicy Bypass -File `"$PSScriptRoot\demo.ps1`""

$process = Start-Process -FilePath $Bin -PassThru
Start-Sleep -Seconds 5
if ($process.HasExited) {
    Write-Host "::error::congminal 启动后退出了，退出码 $($process.ExitCode)"
    exit 1
}

try {
    Add-Type -AssemblyName System.Windows.Forms, System.Drawing
    $bounds = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
    $bitmap = New-Object System.Drawing.Bitmap $bounds.Width, $bounds.Height
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $graphics.CopyFromScreen($bounds.Location, [System.Drawing.Point]::Empty, $bounds.Size)
    $bitmap.Save((Join-Path (Get-Location) $Shot))
} catch {
    Write-Host "::warning::截图失败：$_"
}

if (-not $process.WaitForExit(20000)) {
    Stop-Process -Id $process.Id
    Write-Host '::error::shell 退出后 congminal 没有退出'
    exit 1
}
if ($process.ExitCode -ne 0) {
    Write-Host "::error::congminal 退出码 $($process.ExitCode)"
    exit 1
}
Write-Host '冒烟测试通过'
