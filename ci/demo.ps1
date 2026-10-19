# 冒烟测试时代替 shell 在 congminal 里运行（Windows 版 demo.sh）：输出一屏测试内容，8 秒后退出。
$e = [char]27
$Host.UI.RawUI.WindowTitle = 'congminal smoke test'
Write-Host "congminal smoke test on $([System.Environment]::OSVersion.VersionString)"
$size = $Host.UI.RawUI.WindowSize
Write-Host "size=$($size.Height) $($size.Width)"
Write-Host ''
Write-Host "$e[1;31mbold red$e[0m $e[32mgreen$e[0m $e[38;2;255;160;0mtruecolor$e[0m $e[4munderline$e[0m $e[7minverse$e[0m $e[9mstrike$e[0m"
Write-Host ((0..15 | ForEach-Object { "$e[48;5;${_}m  " }) -join '') -NoNewline
Write-Host "$e[0m"
# DEC 画线字符集只包住边框，否则中间的字母也会被翻译成符号
Write-Host "$e(0lqqqqqqk$e(B"
Write-Host "$e(0x$e(B box  $e(0x$e(B"
Write-Host "$e(0mqqqqqqj$e(B"
Start-Sleep -Seconds 8
