#!/bin/sh
# 无头验证 X11 整窗不透明度：起一个 Xvfb，跑真实 congminal，用 xprop 读回
# `_NET_WM_WINDOW_OPACITY`，和配置值逐个比对。
#
#   ci/check-opacity-x11.sh
#
# 为什么不是单元测试：`_NET_WM_WINDOW_OPACITY` 是 X 服务器上的属性，只有真的开窗口、
# 真的发 XChangeProperty 才存在。Xvfb 没有合成器，所以看不到“半透明效果”，
# 但属性值本身正是合成器读取的东西——这里验证的正是那一层。
set -eu

BIN=${1:-target/debug/congminal}
DISPLAY_NUM=${DISPLAY_NUM:-:93}
WORK=$(mktemp -d)
STATUS=0

cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

command -v Xvfb >/dev/null 2>&1 || { echo "缺少 Xvfb，跳过"; exit 0; }
command -v xprop >/dev/null 2>&1 || { echo "缺少 xprop，跳过"; exit 0; }
command -v xwininfo >/dev/null 2>&1 || { echo "缺少 xwininfo，跳过"; exit 0; }
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN（先 cargo build）"; exit 1; }

Xvfb "$DISPLAY_NUM" -screen 0 800x480x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 2

# 期望值 = round(f32(opacity) × 2^32-1)：配置是 f64，但 App::window_opacity 返回 f32
# （和 Xlib 的定点数精度一致），所以 0.85 会先变成 0.8500000238…，换算结果不是 f64 的那个值。
check() {
    opacity=$1
    expected=$2
    cat >"$WORK/c.toml" <<EOF
[appearance]
opacity = $opacity

[shell]
program = "/bin/sh"
args = ["-c", "printf READY; cat"]
EOF
    DISPLAY=$DISPLAY_NUM "$BIN" --config "$WORK/c.toml" >/dev/null 2>&1 &
    APP_PID=$!
    sleep 3
    window=$(DISPLAY=$DISPLAY_NUM xwininfo -root -children 2>/dev/null | awk '/congminal/{print $1; exit}')
    if [ -z "$window" ]; then
        echo "  ✗ opacity=$opacity：没找到窗口"
        STATUS=1
    else
        value=$(DISPLAY=$DISPLAY_NUM xprop -id "$window" _NET_WM_WINDOW_OPACITY 2>/dev/null | sed 's/.*= //')
        if [ "$value" = "$expected" ]; then
            echo "  ✓ opacity=$opacity → _NET_WM_WINDOW_OPACITY=$value"
        else
            echo "  ✗ opacity=$opacity → 期望 $expected，实际“${value:-无}”"
            STATUS=1
        fi
    fi
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=
    sleep 1
}

echo "X11 整窗不透明度（$DISPLAY_NUM）"
check 0.5 2147483648
check 0.85 3650722303
# 默认不透明：完全不设属性，窗口保持平台默认行为。
cat >"$WORK/c.toml" <<'EOF'
[shell]
program = "/bin/sh"
args = ["-c", "printf READY; cat"]
EOF
DISPLAY=$DISPLAY_NUM "$BIN" --config "$WORK/c.toml" >/dev/null 2>&1 &
APP_PID=$!
sleep 3
window=$(DISPLAY=$DISPLAY_NUM xwininfo -root -children 2>/dev/null | awk '/congminal/{print $1; exit}')
if [ -n "$window" ] && ! DISPLAY=$DISPLAY_NUM xprop -id "$window" _NET_WM_WINDOW_OPACITY 2>/dev/null | grep -q CARDINAL; then
    echo "  ✓ 默认 1.0 → 不设属性"
else
    echo "  ✗ 默认 1.0 仍然设了 _NET_WM_WINDOW_OPACITY"
    STATUS=1
fi

if [ "$STATUS" -eq 0 ]; then echo "结论：通过"; else echo "结论：失败"; fi
exit "$STATUS"
