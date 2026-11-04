#!/bin/sh
# 在 Mac 真机上验证 Metal 渲染：
#
# 1. 跑 Metal 回放与 CPU 绘制的逐像素对照单元测试（每经过一次混合每通道最多差 1，不需要窗口）
# 2. 真实 congminal 分别用 cpu、gpu、auto 跑同一段输出，按 trace 里的窗口编号用 screencapture 截取窗口：
#    gpu、auto（Apple GPU 不是软件光栅器，应当用 Metal）与 CPU 每通道最多差 MAX_DIFF
# 3. 回退：创建时、运行中第 3 帧注入故障，都必须退回 CPU，截图与 CPU 逐字节相同，trace 里写明原因
#
#   ci/check-macos-metal.sh [可执行文件]
#
# 终端要有“屏幕录制”权限（系统设置 → 隐私与安全性），否则截不到别的窗口的内容。
# 跑的时候别动鼠标键盘，新窗口要保持在最前面。结果和 trace 留在打印出来的目录里。
set -eu

BIN=${1:-target/release/congminal}
MAX_DIFF=${MAX_DIFF:-3}
WORK=$(mktemp -d /tmp/congminal-metal.XXXXXX)
STATUS=0

cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
}
trap cleanup EXIT INT TERM

[ "$(uname)" = Darwin ] || { echo "只在 macOS 上运行"; exit 1; }
[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN（先 cargo build --release）"; exit 1; }

echo "macOS Metal 渲染（结果目录 $WORK）"
if [ -n "${CARGO_TEST:-1}" ] && command -v cargo >/dev/null 2>&1; then
    if cargo test --release --bin congminal platform::macos::metal -- --ignored --nocapture >"$WORK/unit.log" 2>&1; then
        echo "  ✓ 回放对照单元测试（$(grep -m1 -o 'Metal：.*' "$WORK/unit.log" || true)）"
    else
        echo "  ✗ 回放对照单元测试失败："
        sed 's/^/    /' "$WORK/unit.log" | tail -20
        STATUS=1
    fi
fi

# 框线、中文、emoji、16 色、粗体、下划线、滚动；之后隔一会儿再输出几次，保证至少画到第 3 帧
cat >"$WORK/load.sh" <<'SH'
printf '┌──────┐\n│ 终端 │\n└──────┘\n🦀 ❤️\n'
i=0
while [ $i -lt 60 ]; do
    printf '\033[3%dmline %d 中文 \033[1;4%dmbold\033[0m \033[4munder\033[0m\n' $((i % 7)) "$i" $((i % 7))
    i=$((i + 1))
done
for i in 1 2 3 4 5 6; do sleep 0.3; printf 'tick %d\n' "$i"; done
printf '\033[?25lDONE'
touch "$1"
exec cat
SH

# 用法：run 名称 渲染方式 [环境变量]；截图 $WORK/名称.bmp，trace $WORK/名称.log
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
    env CONGMINAL_TRACE=1 ${3:-} "$BIN" --config "$WORK/$1.toml" >/dev/null 2>"$WORK/$1.log" &
    APP_PID=$!
    for _ in $(seq 1 60); do [ -f "$WORK/done" ] && break; sleep 0.2; done
    sleep 1.5
    window=$(sed -n 's/.*窗口编号 \([0-9]*\).*/\1/p' "$WORK/$1.log" | head -1)
    if [ -z "$window" ] || [ ! -f "$WORK/done" ]; then
        echo "  ✗ $1：窗口或输出没有就绪"
        STATUS=1
    else
        screencapture -x -o -t bmp -l "$window" "$WORK/$1.bmp" || true
    fi
    kill "$APP_PID" 2>/dev/null || true
    wait "$APP_PID" 2>/dev/null || true
    APP_PID=
    sleep 0.5
}

# 用法：compare 甲 乙 容差；打印 “最大差 不同像素数”，超过容差返回 1
compare() {
    python3 -I - "$WORK/$1.bmp" "$WORK/$2.bmp" "$3" <<'PY'
import struct, sys
def load(path):
    data = open(path, "rb").read()
    offset, = struct.unpack_from("<I", data, 10)
    width, height, _, bits = struct.unpack_from("<iiHH", data, 18)
    return (width, abs(height), bits), data[offset:]
try:
    (a_size, a), (b_size, b) = load(sys.argv[1]), load(sys.argv[2])
except (OSError, struct.error) as error:
    print(f"读不了截图：{error}")
    sys.exit(1)
if a_size != b_size or len(a) != len(b):
    print(f"尺寸不同 {a_size} {b_size}")
    sys.exit(1)
step = a_size[2] // 8
# 只比较颜色通道（BMP 是 BGR/BGRA）
worst, pixels = 0, 0
for i in range(0, len(a) - step + 1, step):
    d = max(abs(a[i + c] - b[i + c]) for c in range(3))
    if d:
        pixels += 1
        worst = max(worst, d)
print(f"最大差 {worst}，不同像素 {pixels}/{len(a) // step}")
sys.exit(0 if worst <= int(sys.argv[3]) else 1)
PY
}

run cpu cpu
run gpu gpu
run auto auto
run init gpu CONGMINAL_GPU_FAULT=init
run present gpu CONGMINAL_GPU_FAULT=present

if [ -f "$WORK/cpu.bmp" ] && python3 -I -c "
import sys; d = open(sys.argv[1], 'rb').read()[54:]
sys.exit(0 if len(set(d[i:i + 3] for i in range(0, min(len(d), 400000), 4))) > 4 else 1)" "$WORK/cpu.bmp"; then
    :
else
    echo "  ✗ CPU 截图几乎是纯色：终端可能没有“屏幕录制”权限"
    STATUS=1
fi

expect_log() {
    if grep -q "$2" "$WORK/$1.log"; then
        echo "  ✓ $3"
    else
        echo "  ✗ $3：trace 里没有“$2”"
        grep renderer "$WORK/$1.log" | sed 's/^/    /' || true
        STATUS=1
    fi
}
expect_log gpu "renderer: .*创建用时" "gpu：创建了 Metal 设备（$(grep -m1 -o 'renderer: .*' "$WORK/gpu.log" || true)）"
expect_log auto "renderer: .*创建用时" "auto：Apple GPU 上使用 Metal"
for name in gpu auto; do
    if grep -q "退回 CPU\|GPU 不可用" "$WORK/$name.log"; then
        echo "  ✗ $name：意外退回了 CPU：$(grep -m1 '退回 CPU\|GPU 不可用' "$WORK/$name.log")"
        STATUS=1
    fi
done
expect_log init "GPU 不可用，使用 CPU：CONGMINAL_GPU_FAULT" "创建时故障：退回 CPU"
expect_log present "GPU 出错，退回 CPU：CONGMINAL_GPU_FAULT" "运行中故障：退回 CPU"

for name in gpu auto; do
    if result=$(compare cpu $name "$MAX_DIFF"); then
        echo "  ✓ $name 与 CPU 每通道差 ≤ $MAX_DIFF（$result）"
    else
        echo "  ✗ $name 与 CPU 不一致（$result）"
        STATUS=1
    fi
done
for name in init present; do
    if result=$(compare cpu $name 0); then
        echo "  ✓ $name 回退后与 CPU 逐字节相同"
    else
        echo "  ✗ $name 回退后与 CPU 不同（$result）"
        STATUS=1
    fi
done

echo "截图与 trace：$WORK"
if [ "$STATUS" -eq 0 ]; then echo "结论：通过"; else echo "结论：失败"; fi
exit "$STATUS"
