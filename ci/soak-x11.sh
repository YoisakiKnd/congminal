#!/bin/sh
# 无头长测：在 Xvfb 里跑真实的 congminal，按固定间隔采样 RSS/线程/文件描述符/CPU，
# 期间用 xdotool 打字、开关右栏面板、改窗口大小，最后留一段静默期证明“静止界面不持续重绘”。
#
# 用法：ci/soak-x11.sh [秒数] [报告路径]
#   ci/soak-x11.sh 1800 /tmp/soak.txt     # 30 分钟，roadmap 要求的长测
#   ci/soak-x11.sh 60                     # 快速自检
#
# 退出码：0 表示所有阈值都过；1 表示有指标越界（报告里写明是哪一项）。
# 依赖：Xvfb、xdotool、以及已构建好的 target/debug/congminal（或 $CONGMINAL_BIN）。
set -eu

DURATION=${1:-1800}
REPORT=${2:-/tmp/congminal-soak.txt}
BIN=${CONGMINAL_BIN:-target/debug/congminal}
SAMPLE=${SOAK_SAMPLE:-10}
# 阈值：允许的增量上限。长测看的是“有没有无界增长”，不是绝对值。
MAX_RSS_GROWTH_KIB=${SOAK_MAX_RSS_GROWTH_KIB:-40960}   # 40 MiB
MAX_THREAD_GROWTH=${SOAK_MAX_THREAD_GROWTH:-4}
MAX_FD_GROWTH=${SOAK_MAX_FD_GROWTH:-10}
IDLE_CPU_LIMIT=${SOAK_IDLE_CPU:-2.0}                    # 静默期的 CPU 百分比上限

[ -x "$BIN" ] || { echo "找不到可执行文件 $BIN（先 cargo build）" >&2; exit 1; }
for tool in Xvfb xdotool; do
    command -v "$tool" >/dev/null 2>&1 || { echo "缺少 $tool" >&2; exit 1; }
done

WORK=$(mktemp -d)
DISPLAY_NUM=$((90 + $$ % 10))
export DISPLAY=":$DISPLAY_NUM"
CONFIG="$WORK/congminal.toml"
cat >"$CONFIG" <<EOF
[font]
family = ["DejaVu Sans Mono"]
size = 13

[window]
columns = 100
rows = 30

[monitor]
enabled = true
interval-seconds = 1

[shell]
# 只有 $WORK/load 存在时才持续输出：静默期删掉它，界面就真的没有新输出可画。
program = "/bin/sh"
args = ["-c", "i=0; while :; do if [ -f '$WORK/load' ]; then i=\$((i+1)); printf 'soak line %s\\n' \"\$i\"; fi; sleep ${SOAK_OUTPUT_SECONDS:-1}; done"]
EOF
: >"$WORK/load"

Xvfb "$DISPLAY" -screen 0 1280x800x24 >"$WORK/xvfb.log" 2>&1 &
XVFB_PID=$!
APP_PID=
cleanup() {
    [ -n "${APP_PID:-}" ] && kill "$APP_PID" 2>/dev/null || true
    kill "$XVFB_PID" 2>/dev/null || true
    rm -rf "$WORK"
}
trap cleanup EXIT INT TERM
sleep 2

"$BIN" --config "$CONFIG" >"$WORK/app.log" 2>&1 &
APP_PID=$!
sleep 3
kill -0 "$APP_PID" 2>/dev/null || { echo "congminal 没能启动，见 $WORK/app.log" >&2; cat "$WORK/app.log" >&2; exit 1; }

# 窗口句柄：用 xdotool 找到 congminal 的窗口，后面打字和改大小都靠它。
WINDOW=$(xdotool search --pid "$APP_PID" --name . 2>/dev/null | tail -1 || true)
[ -n "$WINDOW" ] || WINDOW=$(xdotool search --class congminal 2>/dev/null | tail -1 || true)

proc_status() {  # $1 字段名
    awk -v key="$1:" '$1 == key { print $2; exit }' "/proc/$APP_PID/status" 2>/dev/null || echo 0
}
fd_count() { ls "/proc/$APP_PID/fd" 2>/dev/null | wc -l; }
cpu_ticks() {  # utime + stime
    awk '{ print $14 + $15 }' "/proc/$APP_PID/stat" 2>/dev/null || echo 0
}

: >"$REPORT"
{
    echo "congminal 无头长测"
    echo "开始：$(date -u '+%Y-%m-%dT%H:%M:%SZ')  时长：${DURATION}s  采样间隔：${SAMPLE}s"
    echo "可执行文件：$BIN  显示器：$DISPLAY  窗口：${WINDOW:-（未找到）}"
    echo "阈值（预热 ${SOAK_WARMUP:-20}s 之后计基线）：RSS 增量 ≤ ${MAX_RSS_GROWTH_KIB} KiB、线程 ≤ +${MAX_THREAD_GROWTH}、fd ≤ +${MAX_FD_GROWTH}、静默 CPU ≤ ${IDLE_CPU_LIMIT}%"
    echo
    printf '%-8s %10s %8s %5s %4s %7s\n' 时间 秒 RSS_KiB 线程 fd CPU%
} >>"$REPORT"

# 预热：监控页的进程表首轮会给 sysinfo 建句柄缓存（上限 16），git/监控面板也会先分配一次额度。
# 先跑 WARMUP 秒再取基线，否则会把“一次性初始化”记成“增长”。
WARMUP=${SOAK_WARMUP:-20}
[ -n "$WINDOW" ] && xdotool key --window "$WINDOW" ctrl+shift+m 2>/dev/null || true
sleep "$WARMUP"
[ -n "$WINDOW" ] && xdotool key --window "$WINDOW" ctrl+shift+m 2>/dev/null || true
echo "预热 ${WARMUP}s 后开始计基线" >>"$REPORT"

FIRST_RSS=
FIRST_THREADS=
FIRST_FDS=
MAX_RSS=0
MAX_THREADS=0
MAX_FDS=0
ELAPSED=0
IDLE_CPU=

while [ "$ELAPSED" -lt "$DURATION" ]; do
    RSS=$(proc_status VmRSS)
    THREADS=$(proc_status Threads)
    FDS=$(fd_count)
    TICKS_START=$(cpu_ticks)
    sleep "$SAMPLE"
    TICKS_END=$(cpu_ticks)
    ELAPSED=$((ELAPSED + SAMPLE))
    # CPU 百分比：ticks 差 / (HZ × 间隔) × 100，HZ 按 100 算（Linux 默认）。
    CPU=$(awk -v a="$TICKS_START" -v b="$TICKS_END" -v s="$SAMPLE" 'BEGIN { printf "%.1f", (b - a) / (100 * s) * 100 }')
    printf '%-8s %10s %8s %5s %4s %7s\n' "$(date -u '+%H:%M:%S')" "$ELAPSED" "$RSS" "$THREADS" "$FDS" "$CPU" >>"$REPORT"

    [ -n "$FIRST_RSS" ] || FIRST_RSS=$RSS
    [ -n "$FIRST_THREADS" ] || FIRST_THREADS=$THREADS
    [ -n "$FIRST_FDS" ] || FIRST_FDS=$FDS
    [ "$RSS" -gt "$MAX_RSS" ] && MAX_RSS=$RSS
    [ "$THREADS" -gt "$MAX_THREADS" ] && MAX_THREADS=$THREADS
    [ "$FDS" -gt "$MAX_FDS" ] && MAX_FDS=$FDS

    # 每轮都动一动界面：打字、开关右栏 Git/监控页、改窗口大小，覆盖重排与面板生命周期。
    if [ -n "$WINDOW" ]; then
        xdotool windowactivate --sync "$WINDOW" 2>/dev/null || true
        xdotool type --window "$WINDOW" --delay 20 "echo soak-$ELAPSED" 2>/dev/null || true
        xdotool key --window "$WINDOW" Return 2>/dev/null || true
        case $((ELAPSED / SAMPLE % 4)) in
            0) xdotool key --window "$WINDOW" ctrl+shift+g 2>/dev/null || true ;;
            1) xdotool key --window "$WINDOW" ctrl+shift+m 2>/dev/null || true ;;
            2) xdotool windowsize "$WINDOW" 1000 700 2>/dev/null || true ;;
            3) xdotool windowsize "$WINDOW" 1280 800 2>/dev/null || true ;;
        esac
    fi
done

# 静默期：不再输出、不再输入，观察 CPU 是否掉到接近 0（静止界面不该持续重绘）。
rm -f "$WORK/load"
IDLE_SECONDS=${SOAK_IDLE_SECONDS:-30}
{
    echo
    echo "静默期 ${IDLE_SECONDS}s（不输入、不产出，只看 CPU）"
} >>"$REPORT"
TICKS_START=$(cpu_ticks)
sleep "$IDLE_SECONDS"
TICKS_END=$(cpu_ticks)
IDLE_CPU=$(awk -v a="$TICKS_START" -v b="$TICKS_END" -v s="$IDLE_SECONDS" 'BEGIN { printf "%.2f", (b - a) / (100 * s) * 100 }')
echo "静默期 CPU：${IDLE_CPU}%" >>"$REPORT"

RSS_GROWTH=$((MAX_RSS - FIRST_RSS))
# 稳态漂移：总量增长里有一部分是“按设计填满有界缓存”（终端回滚 10000 行、字形缓存预热），
# 真正要盯的是曲线有没有一直往上走。取前 1/3 与后 1/3 的平均值相减，得到稳态斜率。
RSS_SERIES=$(awk '$2 ~ /^[0-9]+$/ && $3 ~ /^[0-9]+$/ { print $3 }' "$REPORT")
LINES=$(printf '%s\n' "$RSS_SERIES" | grep -c . || true)
THIRD=$((LINES / 3))
RSS_HEAD=$(printf '%s\n' "$RSS_SERIES" | head -n "$THIRD" | awk '{ sum += $1 } END { if (NR) printf "%d", sum / NR; else print 0 }')
RSS_TAIL=$(printf '%s\n' "$RSS_SERIES" | tail -n "$THIRD" | awk '{ sum += $1 } END { if (NR) printf "%d", sum / NR; else print 0 }')
DRIFT=$((RSS_TAIL - RSS_HEAD))
DRIFT_LIMIT=${SOAK_MAX_RSS_DRIFT_KIB:-8192}   # 后 1/3 比前 1/3 平均多 8 MiB 以上才算“还在涨”
# 每秒增长量（KiB/min），方便对照
SPAN=$((DURATION > 60 ? DURATION / 60 : 1))
THREAD_GROWTH=$((MAX_THREADS - FIRST_THREADS))
FD_GROWTH=$((MAX_FDS - FIRST_FDS))
STATUS=0
{
    echo
    echo "小结"
    echo "  RSS：首轮 ${FIRST_RSS} KiB，峰值 ${MAX_RSS} KiB，增量 ${RSS_GROWTH} KiB（阈值 ${MAX_RSS_GROWTH_KIB}）"
    echo "  RSS 稳态漂移：前 1/3 平均 ${RSS_HEAD} KiB → 后 1/3 平均 ${RSS_TAIL} KiB，漂移 ${DRIFT} KiB（阈值 ${DRIFT_LIMIT}），全程 ${SPAN} 分钟"
    echo "  线程：首轮 ${FIRST_THREADS}，峰值 ${MAX_THREADS}，增量 ${THREAD_GROWTH}（阈值 ${MAX_THREAD_GROWTH}）"
    echo "  fd：首轮 ${FIRST_FDS}，峰值 ${MAX_FDS}，增量 ${FD_GROWTH}（阈值 ${MAX_FD_GROWTH}）"
    echo "  静默期 CPU：${IDLE_CPU}%（阈值 ${IDLE_CPU_LIMIT}%）"
} >>"$REPORT"

# 总量超阈值只作为提示：终端回滚按设计会填到 10000 行上限。
if [ "$RSS_GROWTH" -gt "$MAX_RSS_GROWTH_KIB" ]; then
    echo "  ! RSS 总增量超过 ${MAX_RSS_GROWTH_KIB} KiB（若稳态漂移未超阈值，通常是回滚/字形缓存填到上限）" >>"$REPORT"
fi
if [ "$DRIFT" -gt "$DRIFT_LIMIT" ]; then
    echo "  ✗ RSS 稳态漂移超阈值：后 1/3 比前 1/3 平均高 ${DRIFT} KiB，还在持续增长" >>"$REPORT"; STATUS=1
fi
if [ "$THREAD_GROWTH" -gt "$MAX_THREAD_GROWTH" ]; then
    echo "  ✗ 线程增长超阈值" >>"$REPORT"; STATUS=1
fi
if [ "$FD_GROWTH" -gt "$MAX_FD_GROWTH" ]; then
    echo "  ✗ fd 增长超阈值" >>"$REPORT"; STATUS=1
fi
if awk -v value="$IDLE_CPU" -v limit="$IDLE_CPU_LIMIT" 'BEGIN { exit !(value > limit) }'; then
    echo "  ✗ 静默期 CPU 超阈值（静止界面在持续重绘？）" >>"$REPORT"; STATUS=1
fi
[ "$STATUS" -eq 0 ] && echo "  结论：全部阈值通过" >>"$REPORT"

cat "$REPORT"
exit "$STATUS"
