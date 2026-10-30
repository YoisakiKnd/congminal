# Native GUI regression: installation-directory launch, shell cwd and OSC caption overrides.
param([Parameter(Mandatory)][string]$Bin)
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path $Bin).Path
$launchDirectory = Split-Path $binary
$root = Join-Path $env:RUNNER_TEMP ('congminal-startup-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $root | Out-Null
function Toml-String([string]$value) { return '"' + $value.Replace('\', '\\').Replace('"', '\"') + '"' }
try {
    $probe = Join-Path $root 'probe.ps1'
    @'
param([string]$Report)
$ErrorActionPreference = 'Stop'
$Host.UI.RawUI.WindowTitle = 'C:\scoop\shims\pwsh.exe'
Start-Sleep -Milliseconds 500
@{cwd=(Get-Location).Path} | ConvertTo-Json -Compress | Set-Content -Encoding utf8 $Report
Start-Sleep -Seconds 3
'@ | Set-Content -Encoding utf8 $probe
    $explicitDirectory = Join-Path $root ('cwd with spaces ' + [char]0x4E2D)
    New-Item -ItemType Directory -Path $explicitDirectory | Out-Null
    $cases = @(
        @{name='default'; directory=$null; expected=$env:USERPROFILE},
        @{name='explicit'; directory=$explicitDirectory; expected=$explicitDirectory},
        @{name='inherit'; directory='.'; expected=$launchDirectory}
    )
    foreach ($case in $cases) {
        $report = Join-Path $root ($case.name + '.json')
        $config = Join-Path $root ($case.name + '.toml')
        $arguments = @('-NoProfile', '-File', $probe, $report) | ForEach-Object { Toml-String $_ }
        $text = "[shell]`nargs = [" + ($arguments -join ', ') + "]`n"
        if ($null -ne $case.directory) { $text += 'working-directory = ' + (Toml-String $case.directory) + "`n" }
        $text | Set-Content -Encoding utf8 $config
        $process = Start-Process -FilePath $binary -ArgumentList @('--config', ('"' + $config + '"')) -WorkingDirectory $launchDirectory -PassThru
        try {
            $timeout = [Diagnostics.Stopwatch]::StartNew()
            while (-not (Test-Path -LiteralPath $report)) {
                if ($process.HasExited) { throw "Startup fixture exited early: $($case.name)" }
                if ($timeout.Elapsed.TotalSeconds -gt 20) { throw "Startup report timed out: $($case.name)" }
                Start-Sleep -Milliseconds 50
            }
            $actual = Get-Content -LiteralPath $report -Raw | ConvertFrom-Json
            if ($actual.cwd -ne $case.expected) { throw "Wrong cwd for $($case.name): $($actual.cwd); expected $($case.expected)" }
            $stable = [Diagnostics.Stopwatch]::StartNew()
            while ($stable.Elapsed.TotalMilliseconds -lt 750) {
                $process.Refresh()
                if ($process.MainWindowTitle -cne 'congminal') { throw "Shell overwrote application caption: $($process.MainWindowTitle)" }
                Start-Sleep -Milliseconds 50
            }
            if (-not $process.WaitForExit(10000)) { throw 'Startup fixture did not exit' }
            if ($process.ExitCode -ne 0) { throw "Startup fixture failed: $($process.ExitCode)" }
            Write-Host "STARTUP_OK case=$($case.name) cwd=$($actual.cwd) caption=congminal"
        } finally {
            if (-not $process.HasExited) { $process.Kill($true); $process.WaitForExit() }
            $process.Dispose()
        }
    }
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force
}
