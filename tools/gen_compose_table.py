#!/usr/bin/env python3
"""生成 src/term/compose_table.rs：两个字符合成一个字符的规则（Unicode 规范合成，NFC 用的那张表）。

比如 'e' + U+0301（组合用的尖音符）→ 'é'。macOS 的文件名是分解形式（NFD）存的，
不合成的话，带重音的文件名在 ls 里会显示成字母加一个孤零零的符号。

只收录"两个字符 → 一个字符"、并且没被排除在合成之外的规则（用 NFC 结果验证）。
韩文音节的合成有公式，不在表里。

用法：python3 tools/gen_compose_table.py > src/term/compose_table.rs
"""

import unicodedata

pairs = []
for cp in range(0x110000):
    decomposition = unicodedata.decomposition(chr(cp))
    # 有尖括号的是兼容分解（比如全角字母），不参与规范合成
    if not decomposition or decomposition.startswith("<"):
        continue
    parts = [int(p, 16) for p in decomposition.split()]
    if len(parts) != 2:
        continue
    # 合成排除表里的字符，NFC 不会把它们合回来
    if unicodedata.normalize("NFC", chr(parts[0]) + chr(parts[1])) != chr(cp):
        continue
    pairs.append((parts[0], parts[1], cp))

pairs.sort()
print(f"// 由 tools/gen_compose_table.py 生成（Unicode {unicodedata.unidata_version}），不要手改。")
print("// (基础字符, 组合字符, 合成结果)，按前两项排序")
print()
print("pub static COMPOSITIONS: &[(u32, u32, u32)] = &[")
for a, b, c in pairs:
    print(f"    (0x{a:04X}, 0x{b:04X}, 0x{c:04X}),")
print("];")
