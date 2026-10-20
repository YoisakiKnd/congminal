#!/bin/sh
# 输入法测试时代替 shell：读一行，原样存进 $CONGMINAL_IME_RESULT
printf '请输入：'
IFS= read -r line
printf '%s' "$line" > "$CONGMINAL_IME_RESULT"
printf '收到：%s\n' "$line"
sleep 3
