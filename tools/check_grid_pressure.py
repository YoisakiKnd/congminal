#!/usr/bin/env python3
"""macOS 真实 PTY/字符尺寸重排压力检查；仅修改临时副本中的测试代码。"""
import argparse
import ctypes
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def instrument(source):
    old = "app.width = if app.width == 1200 { 1190 } else { 1200 };\n                app.relayout().unwrap();"
    settle = "wait(&mut app, |a| !resize_busy(a));" if "fn resize_busy(" in source else ""
    new = """let previous_cols = app.driver.session.term.cols();
                app.width = if app.width == 1200 { 1200 - 4 * app.driver.renderer.cell_size().0 } else { 1200 };
                let resize_started = Instant::now();
                app.relayout().unwrap();
                SETTLE_UI_RESIZE
                let resize_elapsed = resize_started.elapsed();
                max_relayout = max_relayout.max(resize_elapsed);
                resize_histogram[((resize_elapsed.as_micros() / 50) as usize).min(2000)] += 1;
                assert_ne!(previous_cols, app.driver.session.term.cols());
                actual_reflows += 1;"""
    if source.count(old) != 1:
        raise RuntimeError("压力测试已变化，请检查测试适配，未修改工作区")
    source = source.replace(old, new.replace("SETTLE_UI_RESIZE", settle), 1)
    begin = source.index("fn stress_workspace_sessions()")
    source = source[:begin] + source[begin:].replace(
        "let mut max_tick = Duration::ZERO;",
        "let mut max_tick = Duration::ZERO;\n        let mut max_relayout = Duration::ZERO;\n        let mut actual_reflows = 0;\n        let mut resize_histogram = [0u32; 2001];",
        1,
    )
    source = source.replace(
        '"M4STRESS done cycles={cycles} max_tick_us={} max_key_us={}",',
        '"M4STRESS done cycles={cycles} max_tick_us={} max_key_us={} actual_reflows={actual_reflows} max_relayout_us={}",',
        1,
    )
    pos = source.index('"M4STRESS done')
    source = source[:pos] + source[pos:].replace("max_key.as_micros()", "max_key.as_micros(), max_relayout.as_micros()", 1)
    source = source[:begin] + source[begin:].replace("        drop(app);", '        eprintln!("REFLOW_HISTOGRAM {:?}", resize_histogram);\n        drop(app);', 1)
    return source


class TaskInfo(ctypes.Structure):
    _fields_ = [(n, ctypes.c_uint64) for n in (
        "virtual", "rss", "user", "system", "threads_user", "threads_system"
    )] + [(n, ctypes.c_int32) for n in (
        "policy", "faults", "pageins", "cow", "messages_sent", "messages_received",
        "mach", "unix", "csw", "threads", "running", "priority"
    )]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--seconds", type=int, default=60)
    parser.add_argument("--ui-rounds", type=int, help="测原始 UI 分片用例，不注入同步等待；1–1000 轮")
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "darwin" or not 1 <= args.seconds <= 1800:
        parser.error("仅支持 macOS，时长范围 1–1800 秒")
    if args.ui_rounds is not None and not 1 <= args.ui_rounds <= 1000:
        parser.error("UI 轮数范围 1–1000")
    lib = ctypes.CDLL("/usr/lib/libproc.dylib", use_errno=True)
    lib.proc_pidinfo.argtypes = [ctypes.c_int, ctypes.c_int, ctypes.c_uint64, ctypes.c_void_p, ctypes.c_int]
    lib.proc_listchildpids.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_int]
    lib.proc_name.argtypes = [ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32]

    def snapshot(pid):
        info = TaskInfo()
        if lib.proc_pidinfo(pid, 4, 0, ctypes.byref(info), ctypes.sizeof(info)) != ctypes.sizeof(info):
            raise OSError(ctypes.get_errno(), "proc_pidinfo")
        return {"rss": info.rss, "cpu_ns": info.user + info.system, "threads": info.threads}

    def executable_child(pid):
        children = (ctypes.c_int * 32)()
        count = lib.proc_listchildpids(pid, children, ctypes.sizeof(children))
        for child in list(children)[:max(0, count)]:
            name = ctypes.create_string_buffer(256)
            lib.proc_name(child, name, 256)
            if name.value.startswith(b"congminal"):
                return child
        return None

    workspace_root = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="congminal-grid-pressure-") as tmp:
        copy_root = Path(tmp)
        for name in ("src", "assets"):
            shutil.copytree(workspace_root / name, copy_root / name)
        for name in ("Cargo.toml", "Cargo.lock"):
            shutil.copy2(workspace_root / name, copy_root / name)
        sources = {str(p.relative_to(copy_root)): hashlib.sha256(p.read_bytes()).hexdigest()
                   for p in (copy_root / "src").rglob("*.rs")}
        test = copy_root / "src/workspace.rs"
        if args.ui_rounds is None:
            test.write_text(instrument(test.read_text()))
        subprocess.run(["cargo", "test", "--offline", "--release", "--no-run"], cwd=copy_root, check=True)
        target = "bench_ui_resize" if args.ui_rounds is not None else "stress_workspace_sessions"
        command = ["cargo", "test", "--offline", "--release", f"workspace::tests::{target}",
                   "--", "--ignored", "--exact", "--nocapture", "--test-threads=1"]
        log = copy_root / "run.log"
        samples = []
        with log.open("w") as output:
            env = dict(os.environ, CONGMINAL_STRESS_SECS=str(args.seconds))
            if args.ui_rounds is not None:
                env["CONGMINAL_UI_ROUNDS"] = str(args.ui_rounds)
            process = subprocess.Popen(command, cwd=copy_root, env=env,
                                       stdout=output, stderr=output, start_new_session=True)
            try:
                started = time.monotonic()
                target = None
                while target is None and process.poll() is None and time.monotonic() - started < 15:
                    target = executable_child(process.pid)
                    time.sleep(.05)
                if target is None:
                    raise RuntimeError(log.read_text())
                while process.poll() is None:
                    if time.monotonic() - started > args.seconds + 30:
                        raise TimeoutError("压力测试超时")
                    try:
                        samples.append({"t": round(time.monotonic() - started, 3), **snapshot(target)})
                    except OSError:
                        break
                    time.sleep(.02 if args.ui_rounds is not None else 1)
                status = process.wait(timeout=10)
            finally:
                # 只终止本工具新建的进程组；测试进程关闭 PTY 后其 Shell 收到挂断。
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
                    process.wait(timeout=10)
        text = log.read_text()
        ledger = [{k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", line)}
                  for line in text.splitlines() if "BUDGET t=" in line]
        result = {
            "method": "macOS libproc ~50Hz; original UI slice workload in copied source; four real PTYs and echo; no test instrumentation"
                if args.ui_rounds is not None else "macOS libproc 1Hz; temporary copy with test-only resize delta=4 cell widths; assert columns change; PTY sessions and repeat spawn/close",
            "ui_rounds": args.ui_rounds,
            "seconds": args.seconds, "status": status, "source_sha256": sources,
            "layout_timing": "request through completion with test-harness UI ticks; not UI blocking time" if "fn resize_busy(" in test.read_text() else "synchronous relayout",
            "instrumented_workspace_sha256": hashlib.sha256(test.read_bytes()).hexdigest(),
            "samples": samples, "ledger_series": ledger, "log": text,
            "layout_histogram_50us": json.loads(re.search(r"REFLOW_HISTOGRAM (\[[^\n]+\])", text).group(1))
                if "REFLOW_HISTOGRAM [" in text else None,
            "ui_histograms_50us": {key: json.loads(re.search(key + r" (\[[^\n]+\])", text).group(1))
                for key in ("UI_TICK_HIST", "UI_KEY_HIST") if key + " [" in text},
            "ui_rounds_data": [{k: int(v) for k, v in re.findall(r"(\w+)=(\d+)", line)}
                for line in re.findall(r"UI_RESIZE [^\n]+", text)],
        }
        args.output.write_text(json.dumps(result, indent=2) + "\n")
        if status != 0 or "BUDGET done used=0 mappings=0" not in text:
            raise RuntimeError(text)
        print(f"压力检查通过，记录：{args.output}")


if __name__ == "__main__":
    main()
