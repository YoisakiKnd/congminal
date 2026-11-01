# P0：恢复压缩历史后的解析吞吐

基于 T7 `c9fcf99ddcf5022cf94688010a70b28bb24e8edf`。审计复测发现 T4 逐行编码和 T7 逐次修改计数让滚动解析大幅退化，CJK 低于 T0。本次只处理解析吞吐，不改变历史可逆性、行身份、额度口径或渲染。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/term/grid.rs | `Color` 改为紧凑整数并增加 `ColorKind`；行状态记录已写范围，打印专用入口 `row_mut_upto`，`Row::clear_wide_fragment`、`reset_upto`；网格校验检查范围不变量；颜色往返测试。 |
| src/term/grid/compact.rs | 计数遍只扫描已写范围；整数属性键；一遍按段写入；WIDE/WIDE_SPACER 位图。 |
| src/term/grid/history.rs | 滚出行带范围编码，只清空写过的部分。 |
| src/term/mod.rs | `print_char` 只取一次行，ASCII 批量写入保留范围；`ColorKind` 导出。 |
| src/render.rs、src/graphics/*、src/term/reflow.rs | 适配 `Color` 构造与 `kind()`。 |
| src/perf_bench.rs、ci/perf-compare.py | 新增 `history_bytes_10k_cjk`。 |

## 结果

Linux x86_64（AMD EPYC 7B13 虚拟机）release，三轮中位数；该机噪声较大，帧指标另用新旧二进制交替运行核对。

| 指标 | T0 | T1 | T7 `c9fcf99` | P0 |
| --- | ---: | ---: | ---: | ---: |
| parse_ascii_scroll MiB/s | 66.2 | 197.5 | 112.4 | 194.1 |
| parse_ascii_noscroll MiB/s | 75.9 | 537.1 | 559.0 | 598.6 |
| parse_ascii_2027 MiB/s | 19.1 | 165.6 | 110.9 | 184.2 |
| parse_sgr MiB/s | 67.7 | 140.8 | 70.5 | 135.8 |
| parse_cjk MiB/s | 73.5 | 68.8 | 52.4 | 89.9 |
| history_bytes_10k B | 40,082,112 | 40,082,112 | 1,533,274 | 1,533,274 |
| history_bytes_10k_cjk B | — | — | 10,539,820 | 2,398,141 |

交替运行的帧指标（frame_full、one_row、scroll、scrollback_view、unchanged）新旧在噪声范围内一致。macOS、Windows 未实测。

## 验证

fmt、四目标 clippy、`cargo +1.88 check`、单线程完整回归通过；本机沙盒的 3 个 SFTP 本地文件权限用例在基线同样失败。模糊、语料、快慢路径及新旧历史差分在每块之后检查网格已写范围不变量。
