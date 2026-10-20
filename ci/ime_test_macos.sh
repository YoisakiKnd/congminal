#!/usr/bin/env bash
# macOS 输入法测试：ci/ime_test_macos.sh <congminal 可执行文件> <输出目录>
#
# 切换到简体拼音，用 System Events 模拟键盘输入 "nihao"、空格（选第一个候选词）、回车，
# 检查 shell 收到的是不是"你好"。输入过程中截图，trace 日志写进注解（不登录 GitHub 也能看到）。
set -uo pipefail

bin=$1
out=$2
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$out"
result="$out/ime-result.txt"
log="$out/ime-trace.log"
rm -f "$result"

annotate() {
    local level=$1 title=$2
    local body
    body=$(cat "$out/input-source.txt" "$log" 2>/dev/null | grep -v '字体' | tail -40 | sed 's/%/%25/g' | awk '{printf "%s%%0A", $0}')
    echo "::$level::$title%0A$body"
}

CONGMINAL_SHELL="$here/macos/read_line.sh" CONGMINAL_IME_RESULT="$result" CONGMINAL_TRACE=1 "$bin" 2>"$log" &
pid=$!
sleep 4

osascript -e "tell application \"System Events\" to set frontmost of (first process whose unix id is $pid) to true" \
    || echo "::warning::无法把 congminal 切到前台"
swift "$here/macos/select_input_source.swift" com.apple.inputmethod.SCIM.ITABC > "$out/input-source.txt" 2>&1
cat "$out/input-source.txt"
sleep 1

osascript -e 'tell application "System Events" to keystroke "nihao"' \
    || echo "::warning::osascript 无法模拟按键（可能没有辅助功能权限）"
sleep 1.5
screencapture -x "$out/ime-composing.png" || true
osascript -e 'tell application "System Events" to keystroke " "'
sleep 1
osascript -e 'tell application "System Events" to key code 36'
sleep 2
screencapture -x "$out/ime-done.png" || true
kill "$pid" 2>/dev/null

got=$(cat "$result" 2>/dev/null || echo "（没有收到）")
echo "shell 收到：$got"
echo "--- trace log ---"
cat "$log"
if [ "$got" = "你好" ]; then
    annotate notice "输入法测试通过：shell 收到 $got"
    exit 0
fi
annotate error "输入法测试失败：shell 收到 $got（期望 你好）"
exit 1
