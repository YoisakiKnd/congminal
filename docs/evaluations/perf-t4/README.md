# T4：紧凑物理行历史

前一任务产品 HEAD：`45b90fac4fff9bc6c4040cea917bd62738c53266`。设计先以 `[T4-design] Specify reversible compact physical history rows` 提交，推送 HEAD `293241f7cc9420f1883453d709a7b4109c908a07`，随后实现；详见 [设计稿](../../design/compact-history.md)。不新增 crate。

## 格式、读取与重排

每个历史物理行保存一个连续字节 Vec：UTF-8、每 16 格一个 u16 字节偏移、11 B 属性游程与 10 B 稀疏旁表。原列宽、保留列数、各段长度和 wrapped 在内联 Packed 头部。属性区保存起始列、精确 Color 值与低八位样式；旁表保存其余 flags 和完整 marks，因此链接、WIDE、WIDE_SPACER、EMOJI、mark id 与 Kitty 色位均可往返。只省略 Cell::default() 逐字段相等的尾空白，背景/前景/标记非默认的空白都保留。不按字符宽度推测 spacer 内容。

编码大于原 Cell 保留容量时使用 Raw；活动 Grid 仍为 Row/Vec<Cell>，不改变 Cell 或 Row 大小。逐行 Vec 是设计中明确选择的 arena 替代方案，保留 64 KiB 聚合记账，避免共享块孔洞和跨块所有权。代价是初次写入仍有每行分配，短行仍有拥有者元数据；不承诺任意属性负载都有相同压缩率。

RowView/Cells 借用原始切片或压缩字节。顺序迭代推进 UTF-8、属性和旁表游标；随机读使用检查点及二分属性/旁表。反向迭代使用随机读，正反混合仍保持一致。UTF-8 在建立压缩视图时校验，编码缓冲不对外暴露可变接口。读取不分配，渲染仅访问可见行；Raw 屏幕哈希与窄格复制继续使用原切片路径。

同步重排和 UI 分步重排共用流式逻辑，输出行及时编码；屏幕行解码为 Row，当前/备用 Raw 行保持有限数量。同步移交压缩输入及原票据，分步只读旧源直到交换，取消/退休分片销毁。原 Raw 重排/History 保留在 cfg(test) 独立参照中；旧后端测试通过单独入口运行原重排，未把两个新内核的相同比较当成独立证明。软换行仍在重排层处理，不改变历史的物理行身份和坐标。

## 资源公式与失败路径

令 S=sizeof(StoredRow)，`allocated_bytes = index.capacity()*S + Σ(payload.capacity())`；Raw payload 为 Cell 容量乘 sizeof(Cell)。used_bytes 采用同一真实保留容量口径，包含闲置索引；字节上限也按此裁剪，工作区总配额不能因只改指标而越界。编码长度没有替代容量计费。

沿用 Kind::History 整体 Lease，以 64 KiB 向上取整准入，聚合申请拒绝后允许尝试精确峰值。编码前覆盖新 payload、尚存活的旧 spare、旧/新索引；小字节上限会限制初始索引槽数。满历史的同容量 Packed 替换只重写原缓冲、移动拥有者并增加 dropped，不分配，不改额度。clear 先销毁 payload/索引，再归还票据。pop 解码由事先准入的目标 Grid/工作集覆盖，原历史票据保持到批次结束；take_rows 把数据与原票据一起移交。

Kind::Grid 两屏预留公式不变；Kind::Reflow 改用 decoded_bytes 与 StoredRow 索引大小估算，不再用压缩 bytes/sizeof(Cell) 推算源格数。令 D=历史 decoded_bytes+主网格 allocated_bytes，n=历史行数+原屏幕行数，C=新列数，H=历史字节上限，L=行数上限，r=原屏幕行数：

```text
possible = (D/sizeof(Cell))/max(C-1,1) + n + 1
K = min(possible, L+r, H/S+r)
R = row_bytes(retained_cols(C)) + S
scratch = min(H,K*R) + r*R + 6*(K+1)*S + 8*R + 65536
```

六份索引项覆盖结果及交接过程的新旧缓冲，Raw 行项覆盖当前/备用/屏幕和编码重叠；源由旧票据覆盖。新 Grid 仍先留目标全容量，拒绝发生在取走源之前。高度变化沿用顶部行迁移模型，索引项改为 S。同步/分步阈值改看 Σrow_bytes(原列宽)，避免压缩后误选同步；输入不会整体展开为 decoded 大小的 Vec。

## 测试与失败场景

- 1200 组随机编码往返：包括 0/1/16/4096 列、默认尾空白、任意合法 Unicode、全部颜色、任意 flags/marks 位、wrapped；逐格、随机取格、正向、反向及正反混合迭代、解码均与原行相等。宽字与 spacer 不要求规范配对，验证保留原 Cell 而非推测内容。
- 仅全默认尾空白可省略：有背景、WIDE_SPACER、marks/Kitty 色位或全 flags 的空白必须保留。
- 独立旧 History/重排差分：四份应用语料、1/7/80 列混合 CJK/组合符/颜色/链接、分块输入、回看、四次宽高变换、RIS；所有行 API、光标/epoch/revision、选区复制和搜索一致。另将新 UI 分步只读重排与旧同步重排比较，完成后释放全部额度。
- 字节上限 0..4096 的边界组、400 次混合行插入：每步核对真实 payload 容量+全部索引槽、上限和 Lease 上界，缩到一字节后数据与票据归零。
- 原拒绝/恢复增长、完整池下 funded 转移、take/pop 数据销毁顺序、索引保留/清空、解码容量计数、全局额度拒绝后的稳定复用继续通过。
- 三条原重排参照/随机测试、同步与分步结果、取消各阶段保留源、退休到最后一行后才归还票据、备用屏高度变化、缩放保住光标、Kitty 占位与所有渲染像素测试保留。
- 工作区固定 6000 行负载的旧“必须占历史池一半”断言改为检查完整保留的行数及总配额上界；压缩后内存降低不应造成正确性测试失败。没有更改性能基准输入来满足目标。

## 改动文件

| 文件 | 用途 |
| --- | --- |
| src/term/grid/compact.rs | 连续编码、Raw 回退、借用视图/迭代器及往返测试。 |
| src/term/grid/history.rs | 拥有与容量准入、复用、淘汰、移交和测试后端分派。 |
| src/term/grid/legacy.rs | 仅 cfg(test) 的旧 History 独立参照。 |
| src/term/grid/compact_tests.rs | 新旧历史/重排、所有读取与复制/搜索差分。 |
| src/term/grid.rs | 引入紧凑历史模块，同步容量/拒绝/销毁测试。 |
| src/term/mod.rs | 视图 API、同步压缩重排、独立旧重排入口及 RIS 测试状态。 |
| src/term/memory.rs | decoded 阈值与工作集公式、额度用例更新。 |
| src/term/memory/stepped.rs | 压缩只读源、编码输出交接、屏幕解码、取消和退休。 |
| src/term/reflow.rs | 通用拥有结果类型，旧 Raw 捷径仅作测试参照。 |
| src/term/reflow/stream.rs | 通用输出、压缩输入与输出、按字节/行数保留，原宽格/光标逻辑。 |
| src/text.rs | 文本游标按值读 Cell，保留组合符与软换行语义。 |
| src/selection.rs | 选择边界及测试复制使用视图。 |
| src/links.rs | 自动/显式链接逐格扫描迁移。 |
| src/term/emoji.rs | emoji 组合读取兼容 Row 与借用视图。 |
| src/render.rs | 可见行哈希/字形/背景/连字迁移，Raw 哈希保持切片路径。 |
| src/graphics/tests.rs | Kitty 历史占位与图块测试采用新视图。 |
| src/term/ascii_tests.rs | 快慢差分比较按值历史视图。 |
| src/term/corpus_tests.rs | 语料可见文本迭代迁移。 |
| src/term/fuzz_tests.rs | 隐藏主屏的大历史条件改用 decoded 上界。 |
| src/term/regression.rs | 行快照迁移，宽历史容量断言改用真实保留/票据上界。 |
| src/workspace.rs | 压缩负载检查保留行数和共享配额。 |
| src/perf_bench.rs | 新增回看历史帧场景，原固定负载不变。 |
| ci/perf-compare.py | 当前日志要求新指标，旧基线缺少该指标时明确跳过。 |
| docs/design/compact-history.md | 记录同步复用内核和真实索引口径的设计修订。 |
| docs/foundations.md | 同步 History/Reflow 公式与历史指标口径。 |
| docs/evaluations/perf-t4/README.md | 本次审计材料、指标及差距。 |

原清单中的 main/session/workspace 子模块、graphics/native_tests 等无需改源码：返回的 Cell 按值迭代仍兼容其现有闭包；四目标编译和完整回归用于复核。search/shell 通过 Text/坐标接口读取，未另建历史解码副本。

## 复现与环境

MacBook Air Mac16,12，Apple M4，16 GiB，macOS 27.0.1 (26A434)，rustc 1.99.0，AC Power/电池 100%。软件基准及原生 Unix PTY/工作区测试在 macOS arm64 运行；**Linux/X11、Windows、macOS Intel 未在真机验证**。

```sh
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1
python3 ci/perf-compare.py run-1.log run-2.log run-3.log --baseline old-1.log old-2.log old-3.log
```

每种配置顺序运行三次，测量期间没有并行编译/回归。原始日志 t4-{default,count}-{1,2,3}.log。新增 frame_scrollback_view 的旧值从 T3 git archive 隔离源码中仅加入同名基准夹具后取得，同命令三次；源码、构建与输出在 target/evaluations/t4-scrollback-before/ 和 t4-before-view-*。原固定指标仍使用 T3 当时的三轮数值，不拼选这组补测的其他指标。

最初试测在 t4-trial-perf*；补强独立旧重排参照前的中间样本移至 t4-attempt-a-*，未与最终样本混合。工具启动曾在 dyld 阶段停顿，连版本查询也无输出；诊断与中间失败日志保留，恢复后才正式采集。没有修改系统安全设置。全部原始输出在 target/evaluations，不入库。

## 三轮中位数

| 指标 | 单位 | T3 默认 | T4 默认 | Δ | T3 alloc-count | T4 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | +0.00% | 20.000000 | 20.000000 | +0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | +0.00% | 32.000000 | 32.000000 | +0.00% |
| `history_bytes_10k` | bytes | 40082112.000000 | 1533274.000000 | -96.17% | 40082112.000000 | 1533274.000000 | -96.17% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | +0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 638188.000000 | 34297.500000 | -94.63% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 379.999178 | 224.763559 | -40.85% | 440.592955 | 222.746023 | -49.44% |
| `parse_ascii_noscroll` | MiB/s | 1196.474140 | 1185.312299 | -0.93% | 1200.679149 | 1204.973324 | +0.36% |
| `parse_ascii_2027` | MiB/s | 374.425564 | 215.076650 | -42.56% | 367.107195 | 219.027565 | -40.34% |
| `parse_sgr` | MiB/s | 300.723440 | 184.301319 | -38.71% | 284.088598 | 186.069320 | -34.50% |
| `parse_cjk` | MiB/s | 232.073389 | 122.304250 | -47.30% | 204.759785 | 123.258621 | -39.80% |
| `frame_full` | us/frame | 3523.941154 | 3563.950516 | +1.14% | 3507.962361 | 3556.427564 | +1.38% |
| `frame_one_row` | us/frame | 121.837807 | 125.825905 | +3.27% | 120.390924 | 128.900094 | +7.07% |
| `frame_scroll_1line_growing` | us/frame | 723.368707 | 727.466744 | +0.57% | 721.799012 | 730.664143 | +1.23% |
| `frame_scroll_1line_full` | us/frame | 725.772790 | 726.833222 | +0.15% | 723.122443 | 730.424171 | +1.01% |
| `frame_unchanged` | us/frame | 63.075719 | 65.237719 | +3.43% | 63.048688 | 68.034664 | +7.91% |
| `frame_scrollback_view` | us/frame | 2083.187360 | 2214.635143 | +6.31% | 2092.850330 | 2234.891878 | +6.79% |

default 三轮波动 >=5%：无。

count 三轮波动 >=5%：无。



## 检查、状态与风险

全部必需检查通过：fmt；四目标 Clippy（Linux GNU、Windows GNU、macOS arm64/Intel）与额外 alloc-count Clippy；Rust 1.88.0 check（沿用临时 RUSTUP_HOME 的 +1.88 映射）；默认完整回归 935 passed / 56 ignored，alloc-count 完整回归 937 passed / 56 ignored；辅助程序均 21 passed。

状态：**部分完成**。历史指标 40,082,112 → 1,533,274 B，缩小 **26.14 倍**，超过 15 倍要求，且新口径额外包含闲置索引。冷启动解析分配次数仍为 156.4375 allocs/MiB，分配字节降低 94.63%；帧分配仍为零。

吞吐未达标：parse_ascii_scroll 默认/计数组分别下降 **40.85% / 49.44%**，超过 10% 限制；2027、SGR、CJK 滚动负载也下降，原不滚动负载基本持平。ASCII 批量编码、同容量复用等试改已有改善，但未消除编码开销，没有通过调基准负载来满足目标。比较工具因解析退化及旧样本波动返回失败，原日志保留。

frame_scroll_1line_growing/full 默认分别增加 0.57% / 0.15%，计数组增加 1.23% / 1.01%，严格数值上也未达到“不退化”。新增回看历史帧默认从 2083.19 → 2214.64 us（+6.31%），计数组从 2092.85 → 2234.89 us（+6.79%）。所有最终三轮指标的波动均 <5%；旧 T3 的波动没有被这次稳定样本覆盖或删除。

T2 读缓冲辅助指标继续同命令三次，16 KiB / 4095 B 两档计数组仍为零，默认 skipped，日志 t4-read-*。没有将组件的零分配扩展为整个 PTY/操作系统零分配的结论。

已知风险按严重程度：

1. 滚动解析出现约 35–49% 的吞吐损失，内存收益不能抵消此缺口。字符/属性扫描及编码写入引入额外工作；尚未完成完整采样比例归因。后续审计应优先复测。
2. 可见行借用视图改变热点读法，单行/不变帧在计数组分别增加 7.07% / 7.91%，回看帧增加约 6–7%；不将这些数值一律解释为已证实的布局噪声。
3. 重排准入按最坏输出/索引保守预留，极大压缩历史可能让调整窗口在有限共享预算下被拒绝；拒绝发生在取走源数据之前。多保留的历史本身也增加重排时间。
4. 逐行 Vec 仍有初次分配及元数据；复杂属性行可回退 Raw，不能把本次 ASCII 26 倍结果推广为全部负载。
5. Linux/X11、Windows、macOS Intel 未在真机验证，特别是字体与窗口提交仍需原生审计。
