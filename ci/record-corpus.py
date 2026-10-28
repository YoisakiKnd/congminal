#!/usr/bin/env python3
"""录制真实程序的终端输出，生成回放语料（手工运行，不进 CI）。

用法：python3 ci/record-corpus.py <输出文件> <程序> [参数...]

做法：分配一个 80×24 的 PTY，设 TERM=xterm-256color，把程序写在 PTY 上的
字节原样收下来；按键用固定脚本在延迟后写入，保证退出干净。产物是纯字节流，
不含 typescript 头尾，回放测试（`term::corpus_tests`）直接喂给解析器。

注意：语料一旦提交就固定下来，回放不依赖本机的程序版本；重录会改变文件内容，
需要同时核对回放断言。
"""
import fcntl
import os
import pty
import select
import struct
import sys
import termios
import time

COLS, ROWS = 80, 24
# 每种程序按固定节奏按键：先等它画完第一屏，再滚到底、退出。
KEYS = {
    "less": [(0.35, b"G"), (0.45, b"q")],
    "vim": [(0.45, b":q!\r")],
    "htop": [(0.60, b"q")],
    "man": [(0.45, b"G"), (0.55, b"q")],
}


def record(program, args, limit=262144):
    name = os.path.basename(program)
    keys = list(KEYS.get(name, [(0.4, b"q")]))
    pid, fd = pty.fork()
    if pid == 0:  # 子进程
        os.environ["TERM"] = "xterm-256color"
        os.environ["LINES"] = str(ROWS)
        os.environ["COLUMNS"] = str(COLS)
        os.environ["LC_ALL"] = "C.UTF-8"
        try:
            os.execvp(program, [program, *args])
        finally:
            os._exit(127)
    fcntl.ioctl(fd, termios.TIOCSWINSZ, struct.pack("HHHH", ROWS, COLS, 0, 0))
    output = bytearray()
    deadline = time.time() + 20
    while time.time() < deadline:
        while keys and time.time() >= keys[0][0] + (deadline - 20):
            os.write(fd, keys.pop(0)[1])
        ready, _, _ = select.select([fd], [], [], 0.05)
        if ready:
            try:
                chunk = os.read(fd, 65536)
            except OSError:
                break
            if not chunk:
                break
            output.extend(chunk)
            if len(output) > limit:
                break
        else:
            done, _ = os.waitpid(pid, os.WNOHANG)
            if done:
                break
    try:
        os.close(fd)
    except OSError:
        pass
    try:
        os.waitpid(pid, 0)
    except ChildProcessError:
        pass
    return bytes(output)


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    target, program, *args = sys.argv[1:]
    data = record(program, args)
    if not data:
        print("没有录到任何字节", file=sys.stderr)
        return 1
    with open(target, "wb") as handle:
        handle.write(data)
    print(f"{target}: {len(data)} 字节")
    return 0


if __name__ == "__main__":
    sys.exit(main())
