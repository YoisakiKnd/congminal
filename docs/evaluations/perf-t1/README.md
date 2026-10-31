# T1：ASCII 批量写入与 2027 增量分段

基于已审计 T0：`5e84e86a5de95e0980cabf41ff691cecb8e93f1e`。保留首轮数据作为审计记录；本次在 ed16ed40e125c5c1395bec092a1790895d1892e0 上追加返工，不新增 crate、不修改平台提交路径或 T0 负载，返工推送后继续 T2。下文“首轮”检查与问题均为历史结果，最新结果见文末返工记录。

## 实现与副作用核对

| 调用链/状态 | 核对结果 |
| --- | --- |
| Parser::advance → begin_batch | 每个非空输入块只调用一次，保留 graphics.expire 和 text_revision 的次数；空输入不调用。 |
| Ground / UTF-8 | 只有字节为 0x20..=0x7E、Ground、解码器空闲时才扫描连续 ASCII；连续高位字节的最长合法前缀用标准库验证并直接分发 char；非法/截断后缀、多字节中途和转义序列继续原 byte 状态机。 |
| Perform::print_ascii | 默认逐字符调用 print；Terminal 覆盖为批量写入，不改变其他 performer。 |
| 字符集与 IRM | 当前激活的 G0/G1 非 ASCII，或 insert=true 时整段回退 print_char；不跳过字符集转换、insert_blanks 及命令输出失效。 |
| 字形边界 | 2027 前一簇能与首个 ASCII 合并时，先逐字符处理首字。ASCII 的 GCB 为 Other，之后相邻 ASCII 均为断点，可继续批量写入。 |
| 行尾与 autowrap | 行内按剩余列宽分块，恰好写到末列只设置 pending_wrap；下次字符通过 wrap_line → index → scroll_region_up 执行原滚动语义。DECAWM 关闭时继续覆盖末列。 |
| 宽字符碎片 | 按原逐字符顺序检查 WIDE/WIDE_SPACER；另一半通过 Cell::blank 保留其原背景。包含批量写入范围外的相邻格。 |
| last_char / last_glyph | 每段写入结束更新为实际最后写入的字符/格子；仅追加字素簇时保持旧 last_char，REP 仍使用原入口。 |
| cursor.template | 整个 Cell 模板照抄，保留 fg/bg/flags、超链接 ID 和 marks 中的 Kitty 占位颜色位。 |
| epoch / revision | 普通 print_char 不单独增加 revision，快速路径也不增加；begin_batch、滚动、重置和重排沿用原入口。差分逐块核对 text_stamp、text_bounds、graphics.revision。 |
| shell 标记/命令输出 | 普通覆盖不额外调用失效函数；IRM、字符集回退及局部滚动仍走原链；主屏滚动保持历史/line_id，回看 display_offset 随历史增长调整。测试比较命令 ID、prompt、output、outcome 及目录/phase。 |
| 链接水位 | finish_link_run 位于 OSC/重置等原有控制入口，不在普通字符上调用；批量路径保留同一 flags.link_id，差分比较链接目标。 |

2027 的每次边界查询复制缓存的 Segmenter 后 push(c)，不分配字符串。Cell 一致时使用增量状态；不一致时从实际 cell.c 和 marks 的迭代器重建。emoji 宽度判定也用 base+marks 迭代器，不构造 String。附加成功后推进缓存；附加被上限拒绝时不推进。样式变体选择符若仅存于 EMOJI flag，仍不进入分段文本，保持旧实现的细节。

旧实现中 last_glyph 只在初始化/RIS 时置 None；RIS 通过 with_storage 同时清空缓存。光标移动、擦除、插入/删除、备用屏切换、宽化、重排可能使坐标内容变化：cursor_follows 校验位置和范围，缓存再校验实际 Cell（包含 marks 编号、flags），不匹配即重建。已有 marks 表只追加且编号不复用；无需每个编辑入口维护另一份失效清单，也不改变原本零宽字符仍可附加到 last_glyph 的行为。

## 资源记账

没有新堆缓冲或 Kind。Terminal 的内联 `Option<(Cell, Segmenter)>` 与 cursor/modes 一样不计入堆额度。审计明确该口径后，撤回首轮错误加入的固定状态额度，Grid 恢复原公式：

```text
2 × ((rows + 1) × max(2 × cols, 32) × sizeof(Cell)
     + max(2 × rows, 32) × sizeof(Row))
```

Terminal::try_new / prepare_resize_inner 的 Kind::Grid 先申请、失败保持原网格的行为不变。memory.rs 的原网格容量断言和 docs/foundations.md 的口径同步恢复。不增加 Lease，不修改任何堆分配容量。

## 差分与零分配测试

- Parser::advance_slow 仅在 cfg(test) 下逐字节调用旧状态机；Terminal 的 slow_print 开关让 2027 查询保留旧 String + grapheme::extends 算法，RIS 后保留测试开关。release 不含这些测试钩子。
- ascii_boundaries_and_graphemes_match_the_old_path：1/2/7/80 列、1/7/1024 字节输入块；行尾、pending_wrap、DECAWM、IRM、G0/G1 DEC 字符集、宽字符两半覆盖、ASCII 与组合符/ZWJ/RI/Indic/Prepend 混合、RIS、超链接、REP、shell 标记，以及 UTF-8/ESC 恰好跨 1 KiB 块边界。
- corpus_ascii_fast_and_slow_paths_match_after_every_chunk：四份 term/corpus/*.bin，80×24、17 和 1024 字节分块，逐块比较主/备用网格、历史及全部终端/图片 revision。
- 现有 fuzz_tests 随机输入用相同八个固定种子同步驱动快慢终端，每块之后及每次重排之后比较，不删改原来的健壮性断言。
- grapheme_boundary_checks_do_not_allocate（alloc-count）：在预热且不滚动的 2027 终端中分别使用批量和单字符入口，断言边界查询及普通 ASCII 写入为零分配。组合字符的持久存储/驻留表仍沿用原有路径，没有宣称持久文本存储零分配。

差分还比较 wrapped、完整 Cell、历史 dropped、光标/template/pending_wrap、saved cursor、modes、字符集、tabs、marks 表、应答、标题、键盘栈、last_char/last_glyph 和 shell 输出位置。命令 elapsed 为实际墙钟时长，不要求两次顺序执行恰好相等；其他命令字段逐项一致。

## 首轮复现和环境（历史命令）

```sh
for run in 1 2 3; do
  cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t1-default-$run.log 2>&1
done
for run in 1 2 3; do
  cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t1-count-$run.log 2>&1
done
python3 ci/perf-compare.py target/evaluations/t1-default-{1,2,3}.log --baseline target/evaluations/t0-default-{1,2,3}.log
python3 ci/perf-compare.py target/evaluations/t1-count-{1,2,3}.log --baseline target/evaluations/t0-count-{1,2,3}.log
```

MacBook Air Mac16,12 / Apple M4（4P+6E）/ 16 GiB / macOS 27.0.1 (26A434)，rustc 1.99.0 (b940084d7 2026-09-28)。测量前 pmset 确认 AC Power、电池 100%；未并行编译或跑其他回归。相同 T0 workload、字体、缩放、预热及迭代数，各运行三轮取中位数，不调整基准补齐目标。原始输出全部保留 target/evaluations。

首版试测发现 CJK 退化和吞吐目标缺口；保留 trial-a 日志，暂停后将非 ASCII 字节从 ASCII 扫描候选检查中提前排除，并合并宽字符属性的快速排除条件。trial-b 为修正后的单次试测。正式六轮在最终源码上完整顺序运行，不以试测挑选替换正式样本。

## 改动文件

| 文件 | 用途 |
| --- | --- |
| src/vt.rs | ASCII run 回调、Ground 扫描及仅测试的逐字节入口。 |
| src/term/mod.rs | 行内批量写入、增量 Segmenter 缓存、内容校验和测试慢路径。 |
| src/term/grapheme.rs | ASCII 分段初态，旧 extends 仅保留为测试参照。 |
| src/term/emoji.rs | 共享字符迭代器算法，允许无 String 的 base+marks 输入。 |
| src/term/grid.rs | 返工撤回内联字段额度，恢复原网格公式。 |
| src/term/memory.rs | 返工恢复既有网格准入容量断言。 |
| src/term/ascii_tests.rs | 统一状态差分与专门边界输入。 |
| src/term/corpus_tests.rs | 四份语料逐块快慢差分。 |
| src/term/fuzz_tests.rs | 既有随机流和重排同步驱动慢路径参照。 |
| src/perf_bench.rs | 新增不改变基准负载的 2027 零分配回归测试。 |
| docs/foundations.md | 同步固定状态记账和 ASCII/2027 行为。 |
| docs/evaluations/perf-t1/README.md | 本审计材料、运行方法和最终指标。 |

Linux/X11、Windows、macOS Intel **未在真机验证**；仅 macOS arm64 执行软件解析/绘制/完整测试，交叉编译不视作运行验证。本任务不涉及原生窗口改动。

## 首轮三轮中位数（历史完整指标）

Δ 为数值变化；MiB/s 越高越好，us/frame 越低越好。默认和 alloc-count 是独立构建，分别与相同 feature 的 T0 对比，不能交叉比较。

| 指标 | 单位 | T0 默认 | T1 默认 | Δ | T0 alloc-count | T1 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | 0.00% | 20.000000 | 20.000000 | 0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | 0.00% | 32.000000 | 32.000000 | 0.00% |
| `history_bytes_10k` | bytes | 40082112.000000 | 40082112.000000 | 0.00% | 40082112.000000 | 40082112.000000 | 0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | 0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 638188.000000 | 638188.000000 | 0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 137.220325 | 403.752162 | +194.24% | 135.469411 | 338.900629 | +150.17% |
| `parse_ascii_noscroll` | MiB/s | 171.373984 | 974.430102 | +468.60% | 171.110947 | 976.789184 | +470.85% |
| `parse_ascii_2027` | MiB/s | 29.620451 | 395.629119 | +1235.66% | 29.904947 | 328.535065 | +998.60% |
| `parse_sgr` | MiB/s | 148.073526 | 297.880569 | +101.17% | 148.481085 | 265.475484 | +78.79% |
| `parse_cjk` | MiB/s | 195.328967 | 172.710562 | -11.58% | 194.130608 | 166.589500 | -14.19% |
| `frame_full` | us/frame | 4149.073757 | 4188.316136 | +0.95% | 3576.467802 | 3749.851989 | +4.85% |
| `frame_one_row` | us/frame | 271.439801 | 273.639386 | +0.81% | 246.532466 | 254.930795 | +3.41% |
| `frame_scroll_1line_growing` | us/frame | 2345.404453 | 2366.779297 | +0.91% | 2043.786204 | 2174.516282 | +6.40% |
| `frame_scroll_1line_full` | us/frame | 2360.756121 | 2362.668539 | +0.08% | 2042.541453 | 2160.457437 | +5.77% |
| `frame_unchanged` | us/frame | 63.472915 | 63.551741 | +0.12% | 63.930633 | 64.701083 | +1.21% |

## 首轮性能结论与修正标准复核

实现和差分验证完成，状态按性能目标记为**部分完成**，依据用户允许“正确但没有达到验收指标的优化，可以提交”的规则提交。不调整基准条件，也不把下列缺口标为通过：

- parse_ascii_scroll：默认约 2.94 倍，alloc-count 约 2.50 倍，低于 3 倍。
- parse_ascii_noscroll：默认约 5.69 倍，alloc-count 约 5.71 倍；parse_ascii_2027 分别约 13.36 倍、10.99 倍，二者自身的三倍提升目标通过。
- 审计修正标准为 2027 / scroll ≥70%。首轮默认 395.629119 / 403.752162 = 97.99%，alloc-count 328.535065 / 338.900629 = 96.94%，两组按新标准均通过。原错误标准的 2027 / noscroll 数值保留为 40.60% / 33.63%，仅对照、不再作为失败项。
- parse_cjk：默认退化 11.58%，alloc-count 退化 14.19%。新增 ASCII 候选检查已提前排除高位字节，但退化尚未解决，具体归因未独立定位，不将代码布局或机器噪声假定为已证实原因。
- 帧耗时也未达到“其他指标不退化”：alloc-count 下 full / one_row / scroll_growing / scroll_full / unchanged 分别约增加 4.85% / 3.41% / 6.40% / 5.77% / 1.21%。默认配置的相应变化见完整表。本任务未改 render.rs，但不据此排除实测退化。
- 分配次数、分配字节、Cell/Row 大小、history_bytes_10k 均未增加；2027 普通 ASCII 边界查询的零分配由专门测试验证。

默认配置 parse_ascii_scroll 三轮波动为 6.75%，超过 5%；其他默认指标低于 5%。alloc-count 全部指标低于 5%，最大 parse_ascii_scroll 为 3.34%。比较工具默认组因波动及 CJK 回归失败，alloc-count 组因 CJK 回归失败；失败结果均保留 target/evaluations/t1-*-comparison.txt，没有丢弃样本或重跑挑选稳定轮组。

## 首轮必需检查结果（历史）

| 检查 | 结果 |
| --- | --- |
| cargo fmt --check | 通过 |
| cargo clippy --all-targets --target x86_64-unknown-linux-gnu -- -D warnings | 通过；未在真机验证 |
| cargo clippy --all-targets --target x86_64-pc-windows-gnu -- -D warnings | 通过；未在真机验证 |
| cargo clippy --all-targets --target aarch64-apple-darwin -- -D warnings | 通过 |
| cargo clippy --all-targets --target x86_64-apple-darwin -- -D warnings | 通过；未在真机验证 |
| RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check | 通过，实际 Rust 1.88.0；沿用 T0 的临时工具链映射 |
| cargo test -- --test-threads=1 | 主程序 915 passed / 57 ignored，helper 21 passed |
| cargo test --features alloc-count -- --test-threads=1 | 主程序 916 passed / 58 ignored，helper 21 passed；含新增 2027 零分配测试 |
| cargo clippy --all-targets --features alloc-count -- -D warnings | 额外检查通过 |

完整回归在允许本地 socket 的环境执行，避免 T0 已确认的沙箱 IPC 拒绝，不跳过任何既有用例。六轮 release 基准本身全部通过其正确性/额度断言；性能比较工具失败已在上一节明确记录，不混同为性能验收通过。

## 首轮已知问题（历史，按严重程度）

1. CJK 与部分帧指标实测退化，违反“其他指标不退化”；未定位到足以证明和修复该退化的原因。
2. ASCII 滚动的三倍目标未达到；2027/scroll 的修正目标已通过，不再将原 noscroll 比值列为问题。
3. 默认组 ASCII 滚动波动超过 5%，对应数据可信区间有限；没有更换负载、选取单轮或删去样本。
4. Linux/X11、Windows、macOS Intel 仅交叉检查，未在真机验证。

所有任务内代码和必需回归均完成；后续性能工作需在此提交上另行验证，不能把本次缺口自动算入 T2 的收益。提交以 [T1] 开头，推送的完整 HEAD SHA 记录在聊天进度日志，避免文档自引用提交哈希。

## T1 返工实现与诊断

- Parser::advance 在 Ground 且 UTF-8 空闲时，取连续高位字节 run，用 std::str::from_utf8 得到最长合法前缀，逐 char 调用 performer.print。非法或截断后缀整段逐 byte 喂原解码器，包含后续合法字节，保留 U+FFFD 位置、pending 状态和 reprocess，且不会在恶意非法输入上反复扫描形成平方复杂度。
- 高位扫描、校验和分发放在独立不可内联入口，避免扩大 ASCII/SGR 主循环。ASCII 连续段用安全 u64 整数运算一次验证八字节，遇到异常块再逐字节找准确断点；新增 0..32 长度、每个位置和全部 256 种字节值的标量对比测试。连续合法字节不会跨字节进借位；非法字节必使至少一个高位置一，字节序不影响“整块是否合法”的结论。
- CSI 参数区间直接调用原 param_byte 后进入 CsiParam，控制字节、私有标记和结束字节仍走完整状态表；快慢差分覆盖该入口。此补充是为避免 UTF-8 返工后 SGR 下降而实施，未改变参数累积规则。
- 原 Terminal 批量写入及 2027 增量状态完全保留（src/term/mod.rs 与 ed16ed4 相同）。试测的强制内联、宽字行段分流、额外 ASCII Segmenter 规则等未取得稳定改善，全部撤回，不带入最终提交。
- 初次增加 UTF-8 路径后 CJK 仍低于 T0，因此按要求运行 macOS sample。3303 个基准线程样本中，两个解析阶段各有约 879/865 个样本位于 UTF-8 分发 → print_char，约 417/427 个进一步位于 char_width；滚动维护也是显著成本。采样仅定位热点，不用样本比例冒充加速因果证明。
- 基于实际热点，在 char_width 中对基础汉字 U+4E00..U+9FFF 直接返回双宽；新增测试逐个码点验证现有 ZERO_WIDTH/DOUBLE_WIDTH 表及返回值，宽度语义不变。其余字符仍按原表查询。
- sample 用临时、未提交的 CJK 重复运行入口采样五秒；采样后终止该诊断进程并撤回临时入口。原始 sample、试测、暂停的 round-a 和正式日志均保留 target/evaluations，不提交，不拼选样本。
- 新差分用例涵盖孤立续字节、过长编码、ED A0 80 代理区、F4 90 80 80 超范围、二/三/四字节序列所有块切断位置、与 ESC/C0 交错及 OSC 内字节。原慢路径参照不变。
- 两个分配器自检取消 ignore，进入常规回归；perf_bench 仅 benchmarks 继续 ignore。比较器同时要求明确的 benchmarks 通过标记与 test result: ok，允许任意通过测试数量。nocapture 会拆开框架输出，因此基准全部断言通过后补打印明确标记；此标记位于所有计时之外。
- 历史日志缺少新的连续通过标记，不能用新比较器直接读取；本页历史中位数由原始成功日志保留，返工新日志及后续任务都使用新标记。未修改任何历史原始数值。
- 原始试测轮组 round-a..round-f 全部保留；每次改源码后整组重新测量，最终表仅来自最终源码的三轮，不拼选不同版本。
- 额外改动文件：src/term/width.rs（已验证的基础汉字捷径），ci/perf-compare.py（通过标记判定），src/perf_bench.rs（常规自检/计时外标记）；其他返工文件及记账变化见上文。

## 返工复现

```sh
for run in 1 2 3; do
  cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t1-rework-default-$run.log 2>&1
done
for run in 1 2 3; do
  cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1 > target/evaluations/t1-rework-count-$run.log 2>&1
done
python3 ci/perf-compare.py target/evaluations/t1-rework-default-{1,2,3}.log
python3 ci/perf-compare.py target/evaluations/t1-rework-count-{1,2,3}.log
```

两组都保持 T0 的固定负载、字体、尺寸、迭代数及计时区间，机器接通电源；同一命令运行三次取中位数，测量期间不并行编译或回归。帧耗时保留原始实测；按本次审计在 Linux 的无退化复测结论，alloc-count 首轮 +3–6% 不阻塞，不据此声称本机噪声已被独立证明。

## 返工后三轮中位数（全部指标）

返工前列为 ed16ed4 的原始成功日志，使用相同负载/命令；历史标记拆行由专门的历史读取脚本核对，不篡改日志以绕过新比较器。Δ 为数值变化，MiB/s 增加是改善，us/frame 增加是退化。

| 指标 | 单位 | 返工前默认 | 返工后默认 | Δ | 返工前 alloc-count | 返工后 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | 0.00% | 20.000000 | 20.000000 | 0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | 0.00% | 32.000000 | 32.000000 | 0.00% |
| `history_bytes_10k` | bytes | 40082112.000000 | 40082112.000000 | 0.00% | 40082112.000000 | 40082112.000000 | 0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | 0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 638188.000000 | 638188.000000 | 0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 403.752162 | 445.900716 | +10.44% | 338.900629 | 381.048682 | +12.44% |
| `parse_ascii_noscroll` | MiB/s | 974.430102 | 1203.975927 | +23.56% | 976.789184 | 1192.681787 | +22.10% |
| `parse_ascii_2027` | MiB/s | 395.629119 | 415.733799 | +5.08% | 328.535065 | 357.351908 | +8.77% |
| `parse_sgr` | MiB/s | 297.880569 | 334.188822 | +12.19% | 265.475484 | 298.083243 | +12.28% |
| `parse_cjk` | MiB/s | 172.710562 | 255.761033 | +48.09% | 166.589500 | 209.182114 | +25.57% |
| `frame_full` | us/frame | 4188.316136 | 3581.495841 | -14.49% | 3749.851989 | 3555.360693 | -5.19% |
| `frame_one_row` | us/frame | 273.639386 | 246.295743 | -9.99% | 254.930795 | 246.607559 | -3.26% |
| `frame_scroll_1line_growing` | us/frame | 2366.779297 | 2067.448640 | -12.65% | 2174.516282 | 2044.061386 | -6.00% |
| `frame_scroll_1line_full` | us/frame | 2362.668539 | 2048.476998 | -13.30% | 2160.457437 | 2029.447796 | -6.06% |
| `frame_unchanged` | us/frame | 63.551741 | 64.597664 | +1.65% | 64.701083 | 64.875556 | +0.27% |

返工验收的解析中位数全部满足要求：

- default：CJK 255.761033 ≥ T0 195.328967 MiB/s；2027/scroll 93.23% ≥70%。旧的 2027/noscroll 标准对应 34.53%，保留对照，不作为验收。
- count：CJK 209.182114 ≥ T0 194.130608 MiB/s；2027/scroll 93.78% ≥70%。旧的 2027/noscroll 标准对应 29.96%，保留对照，不作为验收。

稳定性超标项（不删去或替换轮次）：

- default：parse_ascii_2027 6.91%；parse_sgr 12.51%。
- count：parse_ascii_2027 5.78%；parse_sgr 10.65%；parse_cjk 27.16%。

波动的具体外部归因没有独立证明；不将所有变化笼统归为布局噪声。解析验收采用按要求记录的三轮中位数，稳定性比较器的拒绝与超标项保持公开。原始帧耗时及其前后差值保留，上述变化不归因于 render.rs（本次无改动）。

## 返工必需检查与剩余差距

- cargo fmt --check：通过。
- cargo clippy --all-targets -- -D warnings：分别显式指定 x86_64-unknown-linux-gnu、x86_64-pc-windows-gnu、aarch64-apple-darwin、x86_64-apple-darwin，全部通过；另加 alloc-count 本机 Clippy 通过。
- RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check：通过，使用已安装的 1.88.0 与 T0 的同一临时映射。
- cargo test -- --test-threads=1：主程序 919 passed / 56 ignored，helper 21 passed。
- cargo test --features alloc-count -- --test-threads=1：主程序 921 passed / 56 ignored，helper 21 passed；两个取消 ignore 的分配器自检和 2027 零分配测试都通过。
- 追加的 term 子集 113 项、VT 子集 11 项通过；正式完整回归覆盖同样测试，未以子集替代完整回归。

本次返工要求的解析中位数、资源口径恢复、常规自检、新通过标记及修正 2027 标准均完成。原 T1 的滚动 ASCII 三倍目标在默认构建已达到（约 3.25 倍 T0），alloc-count 仍约 2.81 倍，保留该历史目标缺口，不把它悄悄改成通过。原始稳定性超标也保留。按最新返工指令推送后直接继续 T2；本页不宣称 T2–T8 已完成。

返工仍只在 macOS arm64 运行验证；Linux/X11、Windows、macOS Intel **未在真机验证**。Linux 复测结论来自本次用户提供的审计记录，并非本机运行。
