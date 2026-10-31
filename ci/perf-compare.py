#!/usr/bin/env python3
"""手动门槛：三份同机 release 日志；可附三份旧日志，检查中位数退化。"""
import argparse
import math
from pathlib import Path
import statistics
import sys

UNITS = {
    "size_of::<Cell>()": "bytes", "size_of::<Row>()": "bytes",
    "history_bytes_10k": "bytes",
    "allocs_per_mib_parsed": "allocs/MiB",
    "alloc_bytes_per_mib_parsed": "bytes/MiB",
    "allocs_per_frame": "allocs/frame", "alloc_bytes_per_frame": "bytes/frame",
    **dict.fromkeys(("parse_ascii_scroll", "parse_ascii_noscroll", "parse_ascii_2027", "parse_sgr", "parse_cjk"), "MiB/s"),
    **dict.fromkeys(("frame_full", "frame_one_row", "frame_scroll_1line_growing", "frame_scroll_1line_full", "frame_unchanged"), "us/frame"),
}


def read(path):
    text = Path(path).read_text(encoding="utf-8")
    if not any(f"test result: ok. {n} passed" in text for n in (2, 3)):
        raise ValueError(f"{path}: 缺少基准测试通过的记录")
    result = {}
    for line in text.splitlines():
        fields = line.split()
        if not fields or fields[0] not in UNITS:
            continue
        name = fields[0]
        if line == f"{name} skipped (alloc-count disabled)" and name.startswith("alloc"):
            result[name] = None
            continue
        if len(fields) != 3 or fields[2] != UNITS[name] or name in result:
            raise ValueError(f"{path}: 指标重复或单位错误：{line}")
        value = float(fields[1])
        if not math.isfinite(value) or value < 0 or (UNITS[name] in ("MiB/s", "us/frame", "pixels") and value == 0):
            raise ValueError(f"{path}: 非法数值：{line}")
        result[name] = value
    if result.keys() != UNITS.keys():
        raise ValueError(f"{path}: 缺少指标 {UNITS.keys() - result.keys()}")
    return result


def summarize(paths):
    runs = [read(path) for path in paths]
    medians = {}
    failed = False
    for name, unit in UNITS.items():
        values = [run[name] for run in runs]
        if all(value is None for value in values):
            print(f"{name} skipped (alloc-count disabled)")
            medians[name] = None
            continue
        if any(value is None for value in values):
            raise ValueError(f"{name}: 三轮 alloc-count 配置不一致")
        median = statistics.median(values)
        spread = (max(values) - min(values)) / median if median else 0
        print(f"{name} {median:.6f} {unit} spread={spread:.2%}")
        if spread >= 0.05:
            print(f"FAIL: {name} 三轮极差达到中位数的 5%", file=sys.stderr)
            failed = True
        medians[name] = median
    return medians, failed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("logs", nargs=3)
    parser.add_argument("--baseline", nargs=3, help="同机、同字体、同编译器的旧三轮日志")
    args = parser.parse_args()
    current, failed = summarize(args.logs)
    if args.baseline:
        print("baseline:")
        old, unstable = summarize(args.baseline)
        failed |= unstable
        for name, unit in UNITS.items():
            if current[name] is None or old[name] is None:
                if current[name] != old[name]:
                    raise ValueError(f"{name}: 前后 alloc-count 配置不一致")
                continue
            if name in ("frame_width", "frame_height"):
                regression = current[name] != old[name]
            elif unit == "MiB/s":
                regression = current[name] < old[name] * 0.90
            elif unit == "us/frame":
                regression = current[name] > old[name] * 1.10
            else:
                # 内存和分配次数不容许增长；零分配不能被百分比容差掩盖。
                regression = current[name] > old[name]
            if regression:
                print(f"FAIL: {name} 超过回归门槛", file=sys.stderr)
                failed = True
    return int(failed)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError) as error:
        print(error, file=sys.stderr)
        sys.exit(2)
