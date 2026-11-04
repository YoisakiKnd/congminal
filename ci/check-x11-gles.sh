#!/bin/sh
# 无头验证 Linux GPU 渲染（EGL + GLES 3）：起一个 Xvfb（Mesa llvmpipe），
#
# 1. 跑 GPU 回放与 CPU 绘制的逐像素对照单元测试（每经过一次混合每通道最多差 1）
# 2. 真实 congminal 分别用 CPU 和 GPU 跑同一段输出，再分屏、关回单窗格，每一步截图：
#    再改窗口尺寸：GPU 与 CPU 每通道最多差 MAX_DIFF；单窗格与工作区切换时两份绘制记录轮流呈现，也在这里覆盖
# 3. 回退：`auto` 遇到软件光栅器、创建时注入故障、运行中第 3 帧注入故障，都必须退回 CPU，
#    退回之后的截图与 CPU 逐字节相同，trace 里写明原因
#
#   ci/check-x11-gles.sh [可执行文件]
#
# 为什么不全是单元测试：EGL 表面要绑定真实 X 窗口，画面也要经服务器读出来才能比较。
# llvmpipe 是软件光栅器，这里只验证正确性与回退，性能数字不代表 GPU。
set -eu

BIN=${1:-target/release/congminal}
DISPLAY_NUM=${DISPLAY_NUM:-:96}
MAX_DIFF=${MAX_DIFF:-3}
WORK=$(mktemp -d)
STATUS=0

cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    [ -n "${XVFB_PID:-}" ] && kill "$XVFB_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM

for tool in Xvfb xdotool xwd convert python3; do
    command -v "$tool" >/dev/null 2>&1 || { echo "缺少 $tool，跳过"; exit 0; }
done
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN（先 cargo build --release）"; exit 1; }
[ -e /usr/lib/x86_64-linux-gnu/libEGL.so.1 ] || ldconfig -p 2>/dev/null | grep -q 'libEGL.so.1 ' || {
    echo "缺少 libEGL.so.1（apt install libegl1 libgles2 libegl-mesa0），跳过"
    exit 0
}

Xvfb "$DISPLAY_NUM" -screen 0 1024x768x24 >/dev/null 2>&1 &
XVFB_PID=$!
sleep 2

echo "Linux GPU 渲染（$DISPLAY_NUM）"
if [ -n "${CARGO_TEST:-1}" ] && command -v cargo >/dev/null 2>&1; then
    if DISPLAY=$DISPLAY_NUM cargo test -q --bin congminal gles -- --ignored >"$WORK/unit.log" 2>&1; then
        echo "  ✓ 回放对照单元测试（$(grep -m1 -o 'test result: ok. [0-9]* passed' "$WORK/unit.log")）"
    else
        echo "  ✗ 回放对照单元测试失败："
        sed 's/^/    /' "$WORK/unit.log" | tail -15
        STATUS=1
    fi
fi

# 彩色、中文、框线、滚动都覆盖；最后停在固定画面上，光标隐藏、不闪烁
cat >"$WORK/load.sh" <<'SH'
printf '┌──────┐\n│ 终端 │\n└──────┘\n🦀 ❤️\n'
i=0
while [ $i -lt 60 ]; do
    printf '\033[3%dmline %d 中文 \033[1;4%dmbold\033[0m \033[4munder\033[0m\n' $((i % 7)) "$i" $((i % 7))
    i=$((i + 1))
done
printf '\033[?25lDONE'
touch "$1"
exec cat
SH

# 用法：run 名称 渲染方式 [环境变量]；截图写到 $WORK/名称-{single,split,back,resize}.rgb，trace 写到 $WORK/名称.log
run() {
    cat >"$WORK/$1.toml" <<EOF
[shell]
program = "/bin/sh"
args = ["$WORK/load.sh", "$WORK/done"]
[window]
columns = 80
rows = 24
renderer = "$2"
[appearance]
cursor-blink = false
EOF
    rm -f "$WORK/done"
    env DISPLAY="$DISPLAY_NUM" CONGMINAL_TRACE=1 ${3:-} "$BIN" --config "$WORK/$1.toml" >/dev/null 2>"$WORK/$1.log" &
    APP_PID=$!
    for _ in $(seq 1 50); do [ -f "$WORK/done" ] && break; sleep 0.2; done
    sleep 1
    window=$(DISPLAY=$DISPLAY_NUM xdotool search --name congminal 2>/dev/null | head -1)
    if [ -z "$window" ] || [ ! -f "$WORK/done" ]; then
        echo "  ✗ $1：窗口或输出没有就绪"
        STATUS=1
    else
        shot "$1-single" "$window"
        rm -f "$WORK/done"
        DISPLAY=$DISPLAY_NUM xdotool windowfocus --sync "$window" key ctrl+shift+d
        for _ in $(seq 1 50); do [ -f "$WORK/done" ] && break; sleep 0.2; done
        sleep 1
        shot "$1-split" "$window"
        DISPLAY=$DISPLAY_NUM xdotool windowfocus --sync "$window" key ctrl+shift+w
        sleep 1.5
        shot "$1-back" "$window"
        # 渲染目标按新尺寸重建
        DISPLAY=$DISPLAY_NUM xdotool windowsize --sync "$window" 900 560
        sleep 1.5
        shot "$1-resize" "$window"
    fi
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=
    sleep 0.5
}

shot() {
    DISPLAY=$DISPLAY_NUM xwd -silent -id "$2" >"$WORK/$1.xwd" && convert "$WORK/$1.xwd" -depth 8 "rgb:$WORK/$1.rgb"
}

# 用法：compare 甲 乙 容差；打印 “最大差 不同像素数”，超过容差返回 1
compare() {
    python3 -I - "$WORK/$1.rgb" "$WORK/$2.rgb" "$3" <<'PY'
import sys
a, b = open(sys.argv[1], "rb").read(), open(sys.argv[2], "rb").read()
if not a or len(a) != len(b):
    print(f"尺寸不同 {len(a)} {len(b)} 字节")
    sys.exit(1)
worst = max(abs(x - y) for x, y in zip(a, b))
pixels = sum(1 for i in range(0, len(a), 3) if a[i:i + 3] != b[i:i + 3])
print(f"最大差 {worst}，不同像素 {pixels}/{len(a) // 3}")
sys.exit(0 if worst <= int(sys.argv[3]) else 1)
PY
}

run cpu cpu
run gpu gpu
run auto auto
run init gpu CONGMINAL_GPU_FAULT=init
run present gpu CONGMINAL_GPU_FAULT=present

expect_log() {
    if grep -q "$2" "$WORK/$1.log"; then
        echo "  ✓ $3"
    else
        echo "  ✗ $3：trace 里没有“$2”"
        sed 's/^/    /' "$WORK/$1.log" | grep renderer || true
        STATUS=1
    fi
}
expect_log gpu "renderer: .*OpenGL ES 3" "gpu：创建了 GLES 上下文（$(grep -m1 -o 'renderer: [^（]*' "$WORK/gpu.log" || true)）"
if grep -q "退回 CPU\|GPU 不可用" "$WORK/gpu.log"; then
    echo "  ✗ gpu：意外退回了 CPU"
    STATUS=1
fi
expect_log auto "GPU 不可用，使用 CPU：.*软件光栅器" "auto：软件光栅器下退回 CPU"
expect_log init "GPU 不可用，使用 CPU：CONGMINAL_GPU_FAULT" "创建时故障：退回 CPU"
expect_log present "GPU 出错，退回 CPU：CONGMINAL_GPU_FAULT" "运行中故障：退回 CPU"

for step in single split back resize; do
    if result=$(compare cpu-$step gpu-$step "$MAX_DIFF"); then
        echo "  ✓ $step：GPU 与 CPU 每通道差 ≤ $MAX_DIFF（$result）"
    else
        echo "  ✗ $step：GPU 与 CPU 不一致（$result）"
        STATUS=1
    fi
    for name in auto init present; do
        # 运行中故障在第 3 帧：初始画面可能还没画到第 3 帧，分屏之后一定已经退回
        if [ "$name-$step" = present-single ]; then
            if result=$(compare cpu-$step $name-$step "$MAX_DIFF"); then
                echo "  ✓ $step：$name 故障前后的画面与 CPU 每通道差 ≤ $MAX_DIFF（$result）"
            else
                echo "  ✗ $step：$name 与 CPU 不一致（$result）"
                STATUS=1
            fi
            continue
        fi
        if result=$(compare cpu-$step $name-$step 0); then
            echo "  ✓ $step：$name 回退后与 CPU 逐字节相同"
        else
            echo "  ✗ $step：$name 回退后与 CPU 不同（$result）"
            STATUS=1
        fi
    done
done

if [ "$STATUS" -eq 0 ]; then echo "结论：通过"; else echo "结论：失败"; fi
exit "$STATUS"
