#!/usr/bin/env python3
"""生成 src/term/width_table.rs：字符在终端里占几格。

规则和 glibc 的 wcwidth 保持一致（shell 用它计算光标位置，两边不一致就会错位）：
- 0 格：组合字符（Mn、Me）、格式字符（Cf，软连字符 U+00AD 除外）、韩文中声和终声字母
- 2 格：East Asian Width 为 W（宽）或 F（全角）的字符，包括汉字和大部分 emoji
- 其余 1 格

用法：python3 tools/gen_width_table.py > src/term/width_table.rs
加上 --check 参数时，改为和本机 glibc 的 wcwidth 逐个码点比较。
"""

import sys
import unicodedata


# Prepended_Concatenation_Mark：属于格式字符，但会显示出来，glibc 给 1 格
PREPENDED_CONCATENATION_MARKS = [(0x0600, 0x0605), (0x06DD, 0x06DD), (0x070F, 0x070F), (0x0890, 0x0891),
                                 (0x08E2, 0x08E2), (0x110BD, 0x110BD), (0x110CD, 0x110CD)]

# Unicode 15.1 / 16.0 新增或改为宽字符的区间。Python 自带的数据库可能还是旧版本，这里补上，和新版 glibc 一致
NEWER_WIDE = [(0x2FFC, 0x2FFF), (0x31EF, 0x31EF), (0x3248, 0x324F), (0x4DC0, 0x4DFF)]


def in_ranges(cp, table):
    return any(lo <= cp <= hi for lo, hi in table)


def width(cp):
    ch = chr(cp)
    category = unicodedata.category(ch)
    if cp == 0x00AD or in_ranges(cp, PREPENDED_CONCATENATION_MARKS):
        return 1
    if in_ranges(cp, NEWER_WIDE):
        return 2
    if category in ("Mn", "Me", "Cf") or 0x1160 <= cp <= 0x11FF or 0xD7B0 <= cp <= 0xD7FF:
        return 0
    if unicodedata.east_asian_width(ch) in ("W", "F"):
        return 2
    return 1


def ranges(predicate):
    result = []
    start = None
    for cp in range(0x110000):
        if predicate(cp):
            if start is None:
                start = cp
        elif start is not None:
            result.append((start, cp - 1))
            start = None
    if start is not None:
        result.append((start, 0x10FFFF))
    return result


def emit(name, items):
    print(f"pub static {name}: &[(u32, u32)] = &[")
    for lo, hi in items:
        print(f"    (0x{lo:04X}, 0x{hi:04X}),")
    print("];")


def check():
    import ctypes
    import ctypes.util

    libc = ctypes.CDLL(ctypes.util.find_library("c"))
    libc.setlocale(6, b"C.UTF-8")
    diffs = []
    for cp in range(0x20, 0x110000):
        if 0xD800 <= cp <= 0xDFFF or 0x7F <= cp <= 0x9F:
            continue
        theirs = libc.wcwidth(cp)
        if theirs < 0:
            continue  # glibc 认为未分配或不可打印
        if width(cp) != theirs:
            diffs.append((cp, width(cp), theirs))
    print(f"unicode {unicodedata.unidata_version}: {len(diffs)} differences")
    for cp, ours, theirs in diffs[:40]:
        print(f"  U+{cp:04X} {unicodedata.name(chr(cp), '?')}: ours={ours} glibc={theirs}")


if __name__ == "__main__":
    if "--check" in sys.argv:
        check()
        sys.exit()
    print(f"// 由 tools/gen_width_table.py 生成（Unicode {unicodedata.unidata_version}），不要手改。")
    print()
    emit("ZERO_WIDTH", ranges(lambda cp: width(cp) == 0))
    print()
    emit("DOUBLE_WIDTH", ranges(lambda cp: width(cp) == 2))
