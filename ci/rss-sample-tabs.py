#!/usr/bin/env python3
"""原生四标签 RSS：新建三个标签，负载结束后关闭三个；控制目录供手动/原生 UI 自动化读取。"""
import json
import os
from pathlib import Path
import signal
import statistics
import subprocess
import sys
import tempfile
import time

binary = Path(sys.argv[1]).resolve()
output = Path(sys.argv[2]).resolve()
output.mkdir(parents=True, exist_ok=True)
process = None

def children(pid):
    result = subprocess.run(['pgrep', '-P', str(pid)], stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True)
    return [int(value) for value in result.stdout.split()]

def kill_tree(pid):
    try:
        os.kill(pid, signal.SIGSTOP)
    except ProcessLookupError:
        return
    for child in children(pid):
        kill_tree(child)
    try:
        os.kill(pid, signal.SIGKILL)
    except ProcessLookupError:
        pass

def interrupted(_signal, _frame):
    raise SystemExit(130)

signal.signal(signal.SIGINT, interrupted)
signal.signal(signal.SIGTERM, interrupted)
with tempfile.TemporaryDirectory(prefix='congminal-rss-tabs-') as directory:
    work = Path(directory)
    script = work / 'load.sh'
    script.write_text('''set -eu
for n in 1 2 3 4; do
    if mkdir "$1/slot-$n" 2>/dev/null; then break; fi
done
printf '%s' "$$" > "$1/pid-$n"
touch "$1/ready-$n"
while [ ! -f "$1/start" ]; do sleep 0.02; done
seq 1 2000000
cat "$1/payload.txt"
touch "$1/done-$n"
while [ ! -f "$1/stop" ]; do sleep 0.2; done
''')
    with (work / 'payload.txt').open('wb') as payload:
        payload.write((b'x' * 100 + b'\n') * 166111 + b'xxxxx')
    config = work / 'config.toml'
    config.write_text(f'''[shell]
program = '/bin/sh'
args = ['{script}', '{work}']
working-directory = '{work}'
[window]
columns = 200
rows = 60
[scrollback]
lines = 10000
[workspace]
restore-layout = false
[memory]
managed-mib = 512
mapped-font-mib = 1024
''')
    (output / 'config.toml').write_bytes(config.read_bytes())
    try:
        with (output / 'stdout.log').open('w') as stdout, (output / 'stderr.log').open('w') as stderr:
            process = subprocess.Popen([str(binary), '--config', str(config)], stdout=stdout, stderr=stderr,
                                       env=dict(os.environ, CONGMINAL_TRACE='1'))
        (output / 'control.json').write_text(json.dumps({'pid': process.pid, 'work': str(work)}, indent=2))
        print(f'PID {process.pid}; control {output / "control.json"}', flush=True)
        started = time.monotonic()
        peak = 0
        log = (output / 'rss.txt').open('w', buffering=1)
        log.write('elapsed_ms phase rss_bytes\n')
        def sample(phase):
            global peak
            if process.poll() is not None:
                raise RuntimeError(f'窗口提前退出：{process.returncode}')
            value = subprocess.check_output(['ps', '-p', str(process.pid), '-o', 'rss='], text=True).strip()
            rss = int(value) * 1024
            peak = max(peak, rss)
            log.write(f'{(time.monotonic()-started)*1000:.0f} {phase} {rss}\n')
            return rss
        def wait_until(predicate, phase, seconds=180):
            deadline = time.monotonic() + seconds
            while not predicate():
                sample(phase)
                if time.monotonic() >= deadline:
                    raise RuntimeError(f'{phase} 超时')
                time.sleep(0.2)
        print('请在该窗口新建三个标签（Primary+T）；四个 ready 后自动开始负载。', flush=True)
        wait_until(lambda: all((work/f'ready-{n}').exists() for n in range(1,5)), 'open_tabs')
        rss_start = sample('load_start')
        (work/'start').touch()
        wait_until(lambda: all((work/f'done-{n}').exists() for n in range(1,5)), 'load')
        rss_end = sample('load_end')
        print('四标签负载已写完；请关闭三个标签，保留一个。', flush=True)
        workers = [int((work/f'pid-{n}').read_text()) for n in range(1,5)]
        def live_workers():
            alive = 0
            for pid in workers:
                try: os.kill(pid, 0)
                except ProcessLookupError: continue
                alive += 1
            return alive
        wait_until(lambda: live_workers() <= 1, 'close_tabs')
        sample('closed_three')
        # 与默认 rss-sample 一致：结束后等三秒，再取五次中位数。
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            sample('idle_wait')
            time.sleep(min(0.2, max(0,deadline-time.monotonic())))
        idle = []
        for n in range(5):
            idle.append(sample('idle_end'))
            if n < 4: time.sleep(0.2)
        summary = f'rss_start {rss_start} bytes\nrss_end {rss_end} bytes\nrss_idle {statistics.median(idle):.0f} bytes\nrss_peak {peak} bytes\n'
        (output/'summary.txt').write_text(summary)
        print(summary, end='', flush=True)
        (work/'idle_done').touch()
        if os.environ.get('CONGMINAL_RSS_HOLD') == '1':
            print('保留窗口供遮挡/最小化 CPU 检查；完成后在 control.work 中创建 finish 文件。',flush=True)
            wait_until(lambda:(work/'finish').exists(),'hold',300)
        (work/'stop').touch()
        try: process.wait(timeout=10)
        except subprocess.TimeoutExpired: kill_tree(process.pid);process.wait()
        log.close()
    finally:
        (work/'stop').touch()
        if process is not None and process.poll() is None:
            kill_tree(process.pid)
            process.wait()
