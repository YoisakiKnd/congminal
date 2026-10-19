#!/usr/bin/env bash
# 冒烟测试（Linux / macOS）：ci/smoke.sh <congminal 可执行文件> <截图路径>
#
# 让 congminal 运行 ci/demo.sh，检查：
#   1. 启动后没有崩溃
#   2. 截一张图，方便人工确认渲染效果
#   3. demo.sh 退出后，congminal 自己也退出
set -uo pipefail

bin=$1
shot=$2
here=$(cd "$(dirname "$0")" && pwd)
mkdir -p "$(dirname "$shot")"
log="${shot%.png}-trace.log"

CONGMINAL_SHELL="$here/demo.sh" CONGMINAL_TRACE=1 "$bin" 2>"$log" &
pid=$!

sleep 4
if ! kill -0 "$pid" 2>/dev/null; then
    wait "$pid"
    status=$?
    # 把日志放进注解（换行要编码成 %0A），这样不登录 GitHub 也能在 API 里看到
    echo "::error::congminal 启动后退出了，退出码 $status%0A$(sed 's/%/%25/g' "$log" | tail -20 | awk '{printf "%s%%0A", $0}')"
    exit 1
fi

case "$(uname -s)" in
    Linux) import -window root "$shot" ;;
    # 运行机可能没有录屏权限，截不到图不算失败
    Darwin) screencapture -x "$shot" || echo "::warning::截图失败" ;;
esac

for _ in $(seq 1 20); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
done
if kill -0 "$pid" 2>/dev/null; then
    kill "$pid"
    echo "::error::shell 退出后 congminal 没有退出"
    exit 1
fi
wait "$pid"
status=$?

echo "--- trace log ---"
cat "$log"
if [ "$status" -ne 0 ]; then
    echo "::error::congminal 退出码 $status"
    exit 1
fi
echo "冒烟测试通过"
