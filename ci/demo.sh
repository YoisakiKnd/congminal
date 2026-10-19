#!/bin/sh
# 冒烟测试时代替 shell 在 congminal 里运行：输出一屏测试内容，8 秒后退出。
printf '\033]0;congminal smoke test\007'
printf 'congminal smoke test on %s\n' "$(uname -sm)"
printf 'TERM=%s COLORTERM=%s size=%s\n\n' "$TERM" "$COLORTERM" "$(stty size)"
printf '\033[1;31mbold red\033[0m \033[32mgreen\033[0m \033[38;2;255;160;0mtruecolor\033[0m '
printf '\033[4munderline\033[0m \033[7minverse\033[0m \033[9mstrike\033[0m\n'
i=0
while [ $i -lt 16 ]; do
    printf '\033[48;5;%dm  ' $i
    i=$((i + 1))
done
printf '\033[0m\n'
# DEC 画线字符集只包住边框，否则中间的字母也会被翻译成符号
printf '\033(0lqqqqqqk\033(B\n\033(0x\033(B box  \033(0x\033(B\n\033(0mqqqqqqj\033(B\n'
sleep 8
