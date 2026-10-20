#!/usr/bin/env bash
# macOS 输入法测试：ci/ime_test_macos.sh <congminal 可执行文件> <输出目录> [--strict]
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

# 把结果和日志写进注解。macOS 自带的是 bash 3.2，避免用 local（含空格的参数会被拆开）。
# 日志按 8 行一组拆成几条注解，每条注解里的换行编码成 %0A。
report() {
    echo "::$1::$2"
    cat "$out/input-source.txt" "$log" 2>/dev/null | grep -v '字体' | tail -48 | sed 's/%/%25/g' \
        | awk 'NR % 8 == 1 { if (chunk != "") print "::notice::" chunk; chunk = "" } { chunk = chunk $0 "%0A" }
               END { if (chunk != "") print "::notice::" chunk }'
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
    report notice "输入法测试通过：shell 收到 $got"
    exit 0
fi
report error "输入法测试失败：shell 收到 $got（期望 你好）"
# 加 --strict 时失败返回非零（让 CI 失败）
[ "${3:-}" = "--strict" ] && exit 1
exit 0
