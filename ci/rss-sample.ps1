# Windows 原生窗口手动采样：./ci/rss-sample.ps1 [-Binary ...] [-OutputDirectory ...]
param(
    [string]$Binary = 'target/release/congminal.exe',
    [string]$OutputDirectory = ('target/evaluations/rss-sample-' + (Get-Date -Format 'yyyyMMdd-HHmmss'))
)
$ErrorActionPreference = 'Stop'
$binaryPath = (Resolve-Path $Binary).Path
$null = New-Item -ItemType Directory -Force $OutputDirectory
$outputPath = (Resolve-Path $OutputDirectory).Path
$work = Join-Path ([IO.Path]::GetTempPath()) ('congminal-rss-' + [guid]::NewGuid())
$null = New-Item -ItemType Directory $work
$process = $null
try {
    # 与 Unix 相同：固定 16 MiB 文件；写盘不在被测终端中执行。
    $payload = Join-Path $work 'payload.txt'
    $stream = [IO.File]::Create($payload)
    try {
        $line = [Text.Encoding]::ASCII.GetBytes(('x' * 100) + "`n")
        for ($i = 0; $i -lt 166111; $i++) { $stream.Write($line, 0, $line.Length) }
        $stream.Write($line, 0, 5)
    } finally { $stream.Dispose() }
    if ((Get-Item $payload).Length -ne 16777216) { throw '负载大小不符' }
    $load = Join-Path $work 'load.ps1'
    @'
param([string]$Work)
$ErrorActionPreference = 'Stop'
# 显式 CRLF；分批写入而不把两百万对象送入 PowerShell 的格式化管线。
$null = New-Item -ItemType File (Join-Path $Work 'ready')
while (-not (Test-Path (Join-Path $Work 'start'))) { Start-Sleep -Milliseconds 20 }
$writer = [Console]::Out
for ($i = 1; $i -le 2000000; $i++) { $writer.Write($i.ToString([Globalization.CultureInfo]::InvariantCulture)); $writer.Write("`r`n") }
$file = [IO.File]::OpenRead((Join-Path $Work 'payload.txt'))
$output = [Console]::OpenStandardOutput()
try { $writer.Flush(); $file.CopyTo($output); $output.Flush() } finally { $file.Dispose(); $output.Dispose() }
$null = New-Item -ItemType File (Join-Path $Work 'done')
while (-not (Test-Path (Join-Path $Work 'stop'))) { Start-Sleep -Milliseconds 200 }
'@ | Set-Content -Encoding UTF8 $load
    # congminal 会将参数数组直接传给 shell；没有中间命令字符串。
    $shell = (Get-Process -Id $PID).Path
    if ($work.Contains("'") -or $shell.Contains("'")) { throw '路径含单引号，无法生成配置' }
    $config = Join-Path $work 'config.toml'
    @"
[shell]
program = '$shell'
args = ['-NoLogo', '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', '$load', '$work']
working-directory = '$work'
[window]
columns = 200
rows = 60
[scrollback]
lines = 10000
"@ | Set-Content -Encoding UTF8 $config
    $process = Start-Process -FilePath $binaryPath -ArgumentList @('--config', ('"' + $config + '"')) -PassThru -RedirectStandardOutput (Join-Path $outputPath 'stdout.log') -RedirectStandardError (Join-Path $outputPath 'stderr.log')
    $samples = [IO.StreamWriter]::new((Join-Path $outputPath 'rss.txt'))
    [long]$peak = 0
    [long]$last = 0
    $start = 0
    $end = 0
    $idle = @()
    $idleAfter = $null
    $sample = 0
    try {
        $samples.WriteLine('sample rss_bytes')
        while (-not $process.HasExited) {
            $process.Refresh()
            $last = $process.WorkingSet64
            $peak = [Math]::Max($peak, $last)
            $samples.WriteLine("$sample $last")
            $sample++
            if ($start -eq 0 -and (Test-Path (Join-Path $work 'ready'))) {
                $start = $last
                $null = New-Item -ItemType File (Join-Path $work 'start')
            }
            if ($null -eq $idleAfter -and (Test-Path (Join-Path $work 'done'))) {
                $end = $last
                $idleAfter = [Diagnostics.Stopwatch]::StartNew()
            }
            if ($null -ne $idleAfter -and $idleAfter.Elapsed.TotalSeconds -ge 3) {
                $idle += $last
                if ($idle.Count -eq 5) { break }
            }
            if ($sample -ge 900) { throw '负载超时（180 秒）' }
            Start-Sleep -Milliseconds 200
        }
    } finally { $samples.Dispose() }
    if ($idle.Count -ne 5 -or $last -le 0) { throw '负载未完成或窗口提前退出' }
    $median = ($idle | Sort-Object)[2]
    "rss_start $start bytes`nrss_end $end bytes`nrss_idle $median bytes`nrss_peak $peak bytes" | Tee-Object -FilePath (Join-Path $outputPath 'summary.txt')
    $null = New-Item -ItemType File (Join-Path $work 'stop')
    if (-not $process.WaitForExit(10000)) { throw '终端未按期退出' }
    if ($process.ExitCode -ne 0) { throw "终端退出码：$($process.ExitCode)" }
} finally {
    if ($null -ne $process -and -not $process.HasExited) { & taskkill.exe /PID $process.Id /T /F | Out-Null; $process.WaitForExit() }
    Remove-Item -LiteralPath $work -Recurse -Force
}
