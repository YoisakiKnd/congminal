#!/bin/sh
# 无头验证 X11 共享内存提交：起一个 Xvfb，分别用 MIT-SHM 和 XPutImage 跑同一段输出，
# 窗口截图必须逐字节相同；退出后系统里不能多出共享内存段。
#
#   ci/check-x11-shm.sh [可执行文件]
#
# 为什么不是单元测试：共享段要由真实 X 服务器附加，像素也要经服务器读出来才能比较。
set -eu

BIN=${1:-target/release/congminal}
DISPLAY_NUM=${DISPLAY_NUM:-:95}
WORK=$(mktemp -d)
STATUS=0

cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

for tool in Xvfb xdotool xwd ipcs; do
    command -v "$tool" >/dev/null 2>&1 || { echo "缺少 $tool，跳过"; exit 0; }
done
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN（先 cargo build --release）"; exit 1; }

Xvfb "$DISPLAY_NUM" -screen 0 800x480x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 2

# 彩色、中文、滚动和光标都覆盖；最后停在固定画面上。
cat >"$WORK/load.sh" <<'SH'
i=0
while [ $i -lt 120 ]; do
    printf '\033[3%dmline %d 中文 \033[1;4%dmbold\033[0m\n' $((i % 7)) "$i" $((i % 7))
    i=$((i + 1))
done
printf '\033[?25lDONE'
touch "$1"
exec cat
SH
cat >"$WORK/c.toml" <<EOF
[shell]
program = "/bin/sh"
args = ["$WORK/load.sh", "$WORK/done"]
[window]
columns = 80
rows = 24
EOF

segments() { ipcs -m | awk 'NR > 3 && $1 ~ /^0x/' | wc -l; }
before=$(segments)

# 用法：shot 名称 [环境变量]；截图写到 $WORK/名称.xwd，trace 写到 $WORK/名称.log
shot() {
    rm -f "$WORK/done"
    env DISPLAY="$DISPLAY_NUM" CONGMINAL_TRACE=1 $2 "$BIN" --config "$WORK/c.toml" >/dev/null 2>"$WORK/$1.log" &
    APP_PID=$!
    for _ in $(seq 1 50); do [ -f "$WORK/done" ] && break; sleep 0.2; done
    sleep 1
    window=$(DISPLAY=$DISPLAY_NUM xdotool search --name congminal 2>/dev/null | head -1)
    if [ -z "$window" ] || [ ! -f "$WORK/done" ]; then
        echo "  ✗ $1：窗口或输出没有就绪"
        STATUS=1
    else
        DISPLAY=$DISPLAY_NUM xwd -silent -id "$window" >"$WORK/$1.xwd"
    fi
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=
    sleep 0.5
}

echo "X11 共享内存提交（$DISPLAY_NUM）"
shot shm ""
shot put "CONGMINAL_NO_SHM=1"

if grep -q "MIT-SHM 可用" "$WORK/shm.log"; then
    echo "  ✓ 启用了 MIT-SHM"
else
    echo "  ✗ 没有启用 MIT-SHM：$(grep -m1 'MIT-SHM' "$WORK/shm.log" || echo 无记录)"
    STATUS=1
fi
if grep -q "MIT-SHM 不可用" "$WORK/put.log"; then
    echo "  ✓ 对照组走 XPutImage"
else
    echo "  ✗ 对照组没有关闭 MIT-SHM"
    STATUS=1
fi
if [ -s "$WORK/shm.xwd" ] && cmp -s "$WORK/shm.xwd" "$WORK/put.xwd"; then
    echo "  ✓ 两种提交方式的窗口画面逐字节相同"
else
    echo "  ✗ 两种提交方式的画面不同"
    STATUS=1
fi
after=$(segments)
if [ "$after" -le "$before" ]; then
    echo "  ✓ 退出后没有残留共享内存段（$before → $after）"
else
    echo "  ✗ 共享内存段从 $before 增加到 $after"
    STATUS=1
fi

if [ "$STATUS" -eq 0 ]; then echo "结论：通过"; else echo "结论：失败"; fi
exit "$STATUS"
