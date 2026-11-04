# 在 Windows 真机上验证 Direct3D 11 渲染：
#
# 1. 跑 D3D11 回放与 CPU 绘制的逐像素对照单元测试（每经过一次混合每通道最多差 1，不需要窗口）
# 2. 真实 congminal 分别用 cpu、gpu、auto 跑同一段输出，截取窗口客户区：gpu 与 CPU 每通道最多差 MaxDiff；
#    auto 在独立/集成显卡上应当用 GPU，只有 WARP（虚拟机、远程桌面）时退回 CPU
# 3. 回退：创建时、运行中第 3 帧注入故障，都必须退回 CPU，截图与 CPU 逐字节相同，trace 里写明原因
#
#   pwsh ci/check-windows-d3d11.ps1 [-Bin target\release\congminal.exe] [-SkipUnit]
#
# 跑的时候别动鼠标键盘，新窗口要保持在最前面。结果和 trace 留在打印出来的目录里。
param([string]$Bin = 'target\release\congminal.exe', [int]$MaxDiff = 3, [switch]$SkipUnit)
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path $Bin).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ('congminal-d3d11-' + [guid]::NewGuid().ToString('N').Substring(0, 8))
New-Item -ItemType Directory -Path $work | Out-Null
$failed = $false
function Fail([string]$message) { Write-Host "  ✗ $message"; $script:failed = $true }
function Pass([string]$message) { Write-Host "  ✓ $message" }

Add-Type -ReferencedAssemblies System.Drawing -TypeDefinition @'
using System;
using System.Drawing;
using System.Drawing.Imaging;
using System.Runtime.InteropServices;
public static class Shot {
    [StructLayout(LayoutKind.Sequential)] struct RECT { public int Left, Top, Right, Bottom; }
    [StructLayout(LayoutKind.Sequential)] struct POINT { public int X, Y; }
    [DllImport("user32.dll")] static extern bool GetClientRect(IntPtr hwnd, out RECT rect);
    [DllImport("user32.dll")] static extern bool ClientToScreen(IntPtr hwnd, ref POINT point);
    [DllImport("user32.dll")] static extern bool SetForegroundWindow(IntPtr hwnd);
    [DllImport("user32.dll")] static extern bool SetProcessDPIAware();
    public static void Capture(IntPtr hwnd, string path) {
        SetProcessDPIAware();
        SetForegroundWindow(hwnd);
        System.Threading.Thread.Sleep(500);
        RECT rect; GetClientRect(hwnd, out rect);
        POINT origin = new POINT(); ClientToScreen(hwnd, ref origin);
        using (var bitmap = new Bitmap(rect.Right, rect.Bottom, PixelFormat.Format32bppRgb))
        using (var graphics = Graphics.FromImage(bitmap)) {
            graphics.CopyFromScreen(origin.X, origin.Y, 0, 0, bitmap.Size);
            bitmap.Save(path, ImageFormat.Bmp);
        }
    }
    // 返回 {最大通道差, 不同像素数, 总像素数}；尺寸不同时返回 null
    public static int[] Compare(string a, string b) {
        using (var x = new Bitmap(a)) using (var y = new Bitmap(b)) {
            if (x.Width != y.Width || x.Height != y.Height) return null;
            var area = new Rectangle(0, 0, x.Width, x.Height);
            var dx = x.LockBits(area, ImageLockMode.ReadOnly, PixelFormat.Format32bppRgb);
            var dy = y.LockBits(area, ImageLockMode.ReadOnly, PixelFormat.Format32bppRgb);
            var bx = new byte[dx.Stride * x.Height]; var by = new byte[dy.Stride * y.Height];
            Marshal.Copy(dx.Scan0, bx, 0, bx.Length); Marshal.Copy(dy.Scan0, by, 0, by.Length);
            x.UnlockBits(dx); y.UnlockBits(dy);
            int worst = 0, pixels = 0;
            for (int i = 0; i + 3 < bx.Length; i += 4) {
                int d = Math.Max(Math.Abs(bx[i] - by[i]), Math.Max(Math.Abs(bx[i + 1] - by[i + 1]), Math.Abs(bx[i + 2] - by[i + 2])));
                if (d > 0) { pixels++; worst = Math.Max(worst, d); }
            }
            return new int[] { worst, pixels, x.Width * x.Height };
        }
    }
}
'@

Write-Host "Windows Direct3D 11 渲染（结果目录 $work）"
if (-not $SkipUnit) {
    $unit = Join-Path $work 'unit.log'
    & cargo test --release --bin congminal platform::win32::d3d11 -- --ignored --nocapture *> $unit
    if ($LASTEXITCODE -eq 0) {
        Pass "回放对照单元测试（$((Select-String -Path $unit -Pattern 'Direct3D 11：.*' | Select-Object -First 1).Matches.Value)）"
    } else {
        Fail '回放对照单元测试失败：'
        Get-Content $unit -Tail 20 | ForEach-Object { Write-Host "    $_" }
    }
}

# 彩色、中文、框线、滚动；之后隔一会儿再输出几次，保证至少画到第 3 帧
$load = Join-Path $work 'load.ps1'
@'
param([string]$Done)
[Console]::OutputEncoding = [Text.Encoding]::UTF8
$e = [char]27
Write-Host "┌──────┐`n│ 终端 │`n└──────┘"
for ($i = 0; $i -lt 60; $i++) {
    $c = $i % 7
    [Console]::Write("$e[3${c}mline $i 中文 $e[1;4${c}mbold$e[0m $e[4munder$e[0m`r`n")
}
for ($i = 1; $i -le 6; $i++) { Start-Sleep -Milliseconds 300; [Console]::Write("tick $i`r`n") }
[Console]::Write("$e[?25lDONE")
New-Item -ItemType File -Path $Done | Out-Null
Start-Sleep -Seconds 600
'@ | Set-Content -Encoding utf8 $load
function Toml-String([string]$value) { return '"' + $value.Replace('\', '\\').Replace('"', '\"') + '"' }

function Run([string]$name, [string]$renderer, [string]$fault) {
    $done = Join-Path $work "$name.done"
    $config = Join-Path $work "$name.toml"
    $arguments = @('-NoProfile', '-File', $load, $done) | ForEach-Object { Toml-String $_ }
    @"
[shell]
program = "powershell.exe"
args = [$($arguments -join ', ')]
[window]
columns = 80
rows = 24
renderer = "$renderer"
[appearance]
cursor-blink = false
"@ | Set-Content -Encoding utf8 $config
    $env:CONGMINAL_TRACE = '1'
    if ($fault) { $env:CONGMINAL_GPU_FAULT = $fault } else { Remove-Item Env:CONGMINAL_GPU_FAULT -ErrorAction SilentlyContinue }
    $process = Start-Process -FilePath $binary -ArgumentList @('--config', ('"' + $config + '"')) -PassThru `
        -RedirectStandardError (Join-Path $work "$name.log") -RedirectStandardOutput (Join-Path $work "$name.out")
    Remove-Item Env:CONGMINAL_GPU_FAULT -ErrorAction SilentlyContinue
    try {
        $timeout = [Diagnostics.Stopwatch]::StartNew()
        while (-not (Test-Path -LiteralPath $done)) {
            if ($process.HasExited) { Fail "$name：程序提前退出（$($process.ExitCode)）"; return }
            if ($timeout.Elapsed.TotalSeconds -gt 30) { Fail "$name：输出没有就绪"; return }
            Start-Sleep -Milliseconds 100
        }
        Start-Sleep -Milliseconds 1500
        $process.Refresh()
        [Shot]::Capture($process.MainWindowHandle, (Join-Path $work "$name.bmp"))
    } finally {
        if (-not $process.HasExited) { $process.Kill(); $process.WaitForExit() }
        $process.Dispose()
    }
}

Run 'cpu' 'cpu' $null
Run 'gpu' 'gpu' $null
Run 'auto' 'auto' $null
Run 'init' 'gpu' 'init'
Run 'present' 'gpu' 'present'

function Log([string]$name) { $path = Join-Path $work "$name.log"; if (Test-Path $path) { Get-Content $path -Raw } else { '' } }
$gpu = Log 'gpu'
if ($gpu -match 'renderer: (.*)创建用时') { Pass "gpu：创建了 Direct3D 11 设备（$($Matches[1])）" } else { Fail 'gpu：trace 里没有创建设备的记录' }
if ($gpu -match '退回 CPU|GPU 不可用') { Fail "gpu：意外退回了 CPU" }
$auto = Log 'auto'
$autoGpu = $auto -notmatch 'GPU 不可用'
if ($autoGpu) { Pass 'auto：使用了 GPU' } elseif ($auto -match '软件光栅器') { Pass 'auto：只有 WARP，退回 CPU' } else { Fail 'auto：没有使用 GPU，原因不是软件光栅器' }
if ((Log 'init') -match 'GPU 不可用，使用 CPU：CONGMINAL_GPU_FAULT') { Pass '创建时故障：退回 CPU' } else { Fail '创建时故障：trace 里没有退回记录' }
if ((Log 'present') -match 'GPU 出错，退回 CPU：CONGMINAL_GPU_FAULT') { Pass '运行中故障：退回 CPU' } else { Fail '运行中故障：trace 里没有退回记录' }

function Check([string]$name, [int]$limit) {
    $a = Join-Path $work 'cpu.bmp'; $b = Join-Path $work "$name.bmp"
    if (-not ((Test-Path $a) -and (Test-Path $b))) { Fail "$name：缺少截图"; return }
    $result = [Shot]::Compare($a, $b)
    if ($null -eq $result) { Fail "$name：截图尺寸不同"; return }
    $text = "最大差 $($result[0])，不同像素 $($result[1])/$($result[2])"
    if ($result[0] -le $limit) { Pass "$name 与 CPU 每通道差 ≤ $limit（$text）" } else { Fail "$name 与 CPU 不一致（$text）" }
}
Check 'gpu' $MaxDiff
Check 'auto' $(if ($autoGpu) { $MaxDiff } else { 0 })
Check 'init' 0
Check 'present' 0

Write-Host "截图与 trace：$work"
if ($failed) { Write-Host '结论：失败'; exit 1 } else { Write-Host '结论：通过' }
