#!/bin/sh
# 无头验证 X11 窗口被直接销毁时程序会退出：起一个 Xvfb，跑真实 congminal，
# 用 xdotool windowclose 走 XDestroyWindow（绕过 WM_DELETE_WINDOW），确认进程与
# 子进程都结束。
#
#   ci/check-x11-close.sh [可执行文件]
#
# 为什么不是单元测试：DestroyNotify 是 X 服务器发的事件，只有真的开窗口、真的销毁
# 才收得到；没有这个处理时进程会没有窗口地留在后台，连带 shell 子进程一起。
set -eu

BIN=${1:-target/release/congminal}
DISPLAY_NUM=${DISPLAY_NUM:-:94}
WORK=$(mktemp -d)
STATUS=0

cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

command -v Xvfb >/dev/null 2>&1 || { echo "缺少 Xvfb，跳过"; exit 0; }
command -v xdotool >/dev/null 2>&1 || { echo "缺少 xdotool，跳过"; exit 0; }
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN（先 cargo build --release）"; exit 1; }

Xvfb "$DISPLAY_NUM" -screen 0 800x480x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 2

cat >"$WORK/c.toml" <<'EOF'
[shell]
program = "/bin/sh"
args = ["-c", "printf READY; cat"]
EOF

echo "X11 窗口销毁后退出（$DISPLAY_NUM）"
DISPLAY=$DISPLAY_NUM "$BIN" --config "$WORK/c.toml" >/dev/null 2>&1 &
APP_PID=$!
sleep 3
window=$(DISPLAY=$DISPLAY_NUM xdotool search --name congminal 2>/dev/null | head -1)
child=$(pgrep -P "$APP_PID" 2>/dev/null | head -1 || true)
if [ -z "$window" ]; then
    echo "  ✗ 没找到窗口"
    STATUS=1
else
    DISPLAY=$DISPLAY_NUM xdotool windowclose "$window" 2>/dev/null || true
    sleep 3
    alive=0
    for pid in "$APP_PID" ${child:-}; do
        kill -0 "$pid" 2>/dev/null && alive=1
    done
    if [ "$alive" -eq 0 ]; then
        echo "  ✓ 窗口销毁后进程与子进程都已退出"
    else
        echo "  ✗ 窗口销毁后仍有进程在跑（app=$APP_PID child=${child:-无}）"
        STATUS=1
    fi
fi
# 不在这里 wait：无参数 wait 会连 Xvfb 一起等，脚本会卡住。Xvfb 交给 cleanup。
APP_PID=

if [ "$STATUS" -eq 0 ]; then echo "结论：通过"; else echo "结论：失败"; fi
exit "$STATUS"
