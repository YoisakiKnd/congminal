#!/usr/bin/env bash
# macOS 输入法测试：ci/ime_test_macos.sh <congminal 可执行文件> <输出目录> [--strict]
#
# 切换到简体拼音，用 System Events 模拟键盘输入 "nihao"、空格（选第一个候选词）、回车，
# 检查 shell 收到的是不是"你好"。输入过程中截图。
#
# 每一步做完立刻写进报告（<输出目录>/report.md），脚本中途退出也能知道走到了哪里。
# 注意：macOS 自带的是 bash 3.2。
set -uo pipefail

bin=$1
out=$2
strict=${3:-}
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
result="$out/ime-result.txt"
log="$out/ime-trace.log"
rm -f "$result"

report="$out/report.md"
: > "$report"

# 每一步的结果追加到报告里（CI 的下一步会把它发布成一个 check run，不登录也能通过 API 读到）
note() {
    printf '%s\n\n' "$1" | tee -a "$report"
}

trap 'status=$?; note "脚本退出，退出码 $status"' EXIT

CONGMINAL_SHELL="$here/macos/read_line.sh" CONGMINAL_IME_RESULT="$result" CONGMINAL_TRACE=1 "$bin" 2>"$log" &
pid=$!
sleep 4
note "1. congminal 已启动（pid $pid），进程还在：$(kill -0 "$pid" 2>&1 && echo 是 || echo 否)"

front=$(osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $pid) to true" 2>&1)
note "2. 切到前台：$front"

source_output=$(swift "$here/macos/select_input_source.swift" com.apple.inputmethod.SCIM.ITABC 2>&1 | tail -8)
note "3. 切换输入法：$source_output"
sleep 1

typed=$(osascript -e 'tell application "System Events" to keystroke "nihao"' 2>&1)
note "4. 模拟输入 nihao：${typed:-成功}"
sleep 1.5
screencapture -x "$out/ime-composing.png" >/dev/null 2>&1
osascript -e 'tell application "System Events" to keystroke " "' >/dev/null 2>&1
sleep 1
osascript -e 'tell application "System Events" to key code 36' >/dev/null 2>&1
sleep 2
screencapture -x "$out/ime-done.png" >/dev/null 2>&1
kill "$pid" 2>/dev/null

got=$(cat "$result" 2>/dev/null || echo "（没有收到）")
note "5. shell 收到：$got"
note "6. trace 日志（最后 40 行）：
\`\`\`
$(grep -v '字体' "$log" 2>/dev/null | tail -40)
\`\`\`"

if [ "$got" = "你好" ]; then
    printf '\n::notice::输入法测试通过：shell 收到 %s\n' "$got"
    exit 0
fi
printf '\n::error::输入法测试失败：shell 收到 %s（期望 你好）\n' "$got"
if [ "$strict" = "--strict" ]; then
    exit 1
fi
exit 0
