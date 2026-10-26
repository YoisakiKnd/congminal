$ErrorActionPreference = 'Stop'
$fixture = Join-Path $PSScriptRoot 'fixture.exe'
$test = Join-Path $PSScriptRoot 'tests.exe'
$stdout = Join-Path $PSScriptRoot 'native.stdout.log'
$stderr = Join-Path $PSScriptRoot 'native.stderr.log'
$oldFixture = $env:CONGMINAL_TEST_CAPTURE_FIXTURE
$exitCode = 1
$failure = $null
try {
    $env:CONGMINAL_TEST_CAPTURE_FIXTURE = $fixture
    $arguments = @('--exact', 'ssh::capture::platform::tests::native_job_capture_preserves_arguments_bounds_output_and_reaps_descendants', '--ignored', '--nocapture', '--test-threads=1')
    $process = Start-Process -FilePath $test -ArgumentList $arguments -NoNewWindow -Wait -PassThru -RedirectStandardOutput $stdout -RedirectStandardError $stderr
    $process.Refresh()
    if ($null -eq $process.ExitCode) { throw 'Unable to read the native test exit code.' }
    $exitCode = [int]$process.ExitCode
} catch {
    $failure = $_.Exception.Message
} finally {
    $env:CONGMINAL_TEST_CAPTURE_FIXTURE = $oldFixture
}
$stdoutText = $(if (Test-Path $stdout) { Get-Content -Raw -Encoding UTF8 $stdout } else { '' })
$stderrText = $(if (Test-Path $stderr) { Get-Content -Raw -Encoding UTF8 $stderr } else { '' })
$verified = ($exitCode -eq 0) -and ($stdoutText -match 'test result: ok\. 1 passed;') -and ($stderrText -match 'WINDOWS_CAPTURE .* duplex=ok backpressure=ok .* final=0')
if (-not $verified -and ($null -eq $exitCode -or $exitCode -eq 0)) { $exitCode = 1; $failure = 'Expected native fixture completion marker was missing.' }
$result = [ordered]@{
    scope = 'Native Windows Job/argv/output/duplex/backpressure/cancellation/descendant cleanup fixture; no user SSH files or credentials.'
    os = [Environment]::OSVersion.VersionString
    is64BitProcess = [Environment]::Is64BitProcess
    exit = $exitCode
    native_runtime_verified = $verified
    error = $failure
    fixtureSha256 = (Get-FileHash -Algorithm SHA256 $fixture).Hash
    testSha256 = (Get-FileHash -Algorithm SHA256 $test).Hash
    stdout = $stdoutText
    stderr = $stderrText
}
$result | ConvertTo-Json -Depth 5 | Set-Content -Encoding UTF8 (Join-Path $PSScriptRoot 'native-result.json')
Write-Output "Windows capture fixture exit=$exitCode; see native-result.json"
exit $exitCode
