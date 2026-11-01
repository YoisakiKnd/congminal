#!/usr/bin/env bash
# Linux/macOS 原生窗口手动采样：ci/rss-sample.sh [release 可执行文件] [输出目录]
set -euo pipefail
# tabs 场景由同目录控制器采样；默认单窗口负载与清理规则不变。
if [[ "${3:-}" == tabs ]]; then exec python3 "$(dirname "$0")/rss-sample-tabs.py" "$1" "$2"; fi
case "$(uname -s)" in Linux|Darwin) ;; *) echo '仅支持 Linux/macOS' >&2; exit 2 ;; esac
bin=${1:-target/release/congminal}
bin=$(cd "$(dirname "$bin")" && pwd)/$(basename "$bin")
[[ -x "$bin" ]] || { echo "不可执行：$bin" >&2; exit 2; }
out=${2:-target/evaluations/rss-sample-$(date +%Y%m%d-%H%M%S)}
mkdir -p "$out"
out=$(cd "$out" && pwd)
work=$(mktemp -d)
pid=
# 先清理后代，再等候主进程；失败/信号路径同样执行。
kill_tree() {
    local child
    kill -STOP "$1" 2>/dev/null || true
    for child in $(pgrep -P "$1" || true); do kill_tree "$child"; done
    kill -KILL "$1" 2>/dev/null || true
}
cleanup() {
    if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
        touch "$work/stop"
        kill_tree "$pid"
        wait "$pid" 2>/dev/null || true
    fi
    rm -rf "$work"
}
trap cleanup EXIT
trap 'exit 130' INT TERM
# TOML 字面量不支持单引号；不把临时路径拼入 shell 命令。
[[ "$work" != *"'"* ]] || { echo '临时路径含单引号，无法生成配置' >&2; exit 2; }
cat > "$work/load.sh" <<'SH'
set -eu
touch "$1/ready"
while [ ! -f "$1/start" ]; do sleep 0.02; done
seq 1 2000000
cat "$1/payload.txt"
touch "$1/done"
# 采样端完成空闲采样后释放，不把退出后的零值当作结束值。
while [ ! -f "$1/stop" ]; do sleep 0.2; done
SH
# 固定 16 MiB、每行 100 ASCII 字符加 LF，文件生成不算终端负载。
awk 'BEGIN { s=""; for(i=0;i<100;i++) s=s "x"; for(i=0;i<166111;i++) print s; printf "xxxxx" }' > "$work/payload.txt"
[[ $(wc -c < "$work/payload.txt") -eq 16777216 ]]
cat > "$work/config.toml" <<EOF
[shell]
program = '/bin/sh'
args = ['$work/load.sh', '$work']
working-directory = '$work'
[window]
columns = 200
rows = 60
[scrollback]
lines = 10000
EOF
"$bin" --config "$work/config.toml" > "$out/stdout.log" 2> "$out/stderr.log" &
pid=$!
printf 'sample rss_bytes\n' > "$out/rss.txt"
read_rss() {
    local rss
    rss=$(ps -p "$pid" -o rss= | tr -d ' ')
    [[ "$rss" =~ ^[0-9]+$ ]] || return 1
    last=$((rss * 1024))
    (( last <= peak )) || peak=$last
    printf '%s %s\n' "$sample" "$last" >> "$out/rss.txt"
    sample=$((sample + 1))
}
peak=0
last=0
sample=0
for ((attempt=0; attempt<900; attempt++)); do
    kill -0 "$pid" 2>/dev/null || { echo '窗口提前退出' >&2; exit 1; }
    [[ ! -f "$work/ready" ]] || break
    sleep 0.2
done
[[ -f "$work/ready" ]] || { echo '负载启动超时' >&2; exit 1; }
read_rss
start=$last
touch "$work/start"
while [[ ! -f "$work/done" ]]; do
    read_rss
    if (( sample >= 900 )); then echo '负载超时（180 秒）' >&2; exit 1; fi
    sleep 0.2
done
read_rss
end=$last
sleep 3
idle=()
for ((i=0; i<5; i++)); do
    read_rss
    idle+=("$last")
    if (( i < 4 )); then sleep 0.2; fi
done
median=$(printf '%s\n' "${idle[@]}" | sort -n | sed -n '3p')
printf 'rss_start %s bytes\nrss_end %s bytes\nrss_idle %s bytes\nrss_peak %s bytes\n' "$start" "$end" "$median" "$peak" | tee "$out/summary.txt"
touch "$work/stop"
# 正常退出也有期限，卡住时 EXIT trap 清理进程树。
for ((attempt=0; attempt<50; attempt++)); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 0.2
done
if kill -0 "$pid" 2>/dev/null; then echo '终端未按期退出' >&2; exit 1; fi
wait "$pid"
pid=
