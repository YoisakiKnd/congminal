# T4：物理行紧凑历史设计

基于 T3 `45b90fac4fff9bc6c4040cea917bd62738c53266`。本稿先单独提交并推送，再开始实现；不新增 crate。目标是可逆地保留原物理行和全部 Cell 信息，压缩历史而不压缩活动网格。

## 存储选择与容量

采用“每行连续字节缓冲 + VecDeque 行索引”，暂不采用共享 64 KiB arena。这是 prompt 允许的其他方案：独立 Vec 可以复用被淘汰行的缓冲，头部淘汰和缩放取消直接销毁对应拥有者，不需要 arena 内部空洞压缩、跨块记录或共享存活计数；代价是每行仍有一个分配和 Vec 元数据，短行压缩率受索引影响。64 KiB 仍是 History 票据的聚合申请粒度，不是每行 Vec 的最低容量。

行记录使用 Packed 与 Raw 两种表示；编码比原 Cell 容量更大时保留 Raw，避免随机颜色/旁表密集数据膨胀。原 Row 仍是活动屏幕表示，测试参照保留旧 VecDeque<Row> 实现。Packed 的所有区段放在一个 Vec<u8>，不创建每行 String、属性 Vec 或旁表 Vec。

每行字节成本为 `sizeof(StoredRow) + payload.capacity()`，Raw 为 `sizeof(StoredRow) + cells.capacity()*sizeof(Cell)`，是实际拥有容量的上界，不按编码长度少记容量。整个容器另计空闲索引槽 `(index.capacity()-index.len())*sizeof(StoredRow)`；扩容准入覆盖旧索引、新索引和新 payload 的同时存活峰值。先申请额度再分配；准入失败不把尚未准备的行放进历史。历史已有的行数/字节淘汰政策继续显式累计 dropped，失败不得留下半条记录。

票据按需向上取整 64 KiB，拒绝后可尝试精确峰值；批次结束在数据销毁后才归还多余额度。满历史滚动复用淘汰的同容量 Packed Vec，活动屏幕 Row 原地 reset，避免每滚一行再分配完整 Cell 行。不同容量需要新分配时仍先覆盖旧/新缓冲重叠。clear 销毁所有缓冲和索引，再归还票据；pop 到网格/工作集时票据保留到目标接管或解码完成。

## 可逆编码

固定小端头部记录版本、wrapped、原始列宽、保留列数、UTF-8 字节数、属性游程数和旁表项数。各计数字段使用明确宽度，所有算术 checked；最大列宽沿用 4096，字符最多四字节。内部格式不写磁盘，不是外部输入协议。

1. 去掉尾部与 `Cell::default()` **逐字段完全相等**的格子。背景色、前景色、flags、超链接、marks 或 Kitty 色位非默认的空白都保留。原始列宽保留，读出时补回默认格子；wrapped 行也不丢失原宽。
2. UTF-8 区保存每个保留 Cell 的 c，包括 WIDE_SPACER 中的字符；不根据 Unicode 宽度推测列数。文本按格而非字节编号。每隔固定数量的格子保存 UTF-8 字节偏移检查点，随机取格只需在邻近检查点后解码一小段；连续迭代保持解码器游标。
3. 属性游程以起始列、fg、bg、样式 flags 编码；默认属性可隐式表示。Color 明确区分 Default、Indexed 与 Rgb，不能将相同显示颜色当成同一值。游程结束由下一起始列/保留列数确定。
4. 稀疏旁表按列存完整 marks 位图和超链接 id，保留 mark id 的低 17 位及 Kitty 占位色的高位。EMOJI、WIDE、WIDE_SPACER 明确保存，不能从 c 重建；未知保留 flag 位也必须往返。旁表与属性区组合恢复完整 flags。

编码只省掉可由原始列宽恢复的全默认后缀。每个保留格的 c、两色、flags、marks 都有唯一来源，wrapped 独立保存，因此解码后逐格相等。UTF-8 校验/偏移边界由编码器产生，解码不得依赖不受检的外部字节；往返测试覆盖所有 Color/flag/marks 位、宽字碎片和非规范但合法的 Cell 组合。

## 保留物理行

不在存储层拼接软换行。逻辑行存储虽可减少元数据，但会改变头部淘汰单位、line_id 与 display_offset 的对应关系，并要求选区/搜索坐标、shell 命令标记、超链接边界共同引入子行映射；这超出压缩的必要范围。物理行保持 wrapped，三条重排路径继续按原规则组合逻辑行；未重排时 line_id、文本 revision 和坐标不因编码而变化。压缩后的字节上限可容纳更多行，这是预期容量变化，行数上限仍不变。

## 读取 API 与迁移清单

引入借用的 RowView/Cells：Raw 借用 Cell 切片，Packed 借用字节区。`cell/get` 返回 Copy 的 Cell，`iter` 连续按值解码，提供 len、wrapped 与原始列宽；不会在只读查询里分配。History.get/iter、Terminal.visible_row/row_by_id 返回视图。活动网格写入 API 继续使用 Row，不暴露可变 Packed 借用。

以下为当前源码检索得到的迁移范围，包含测试而非仅生产调用点；实现时再以编译器和 rg 复核：

| 文件/路径 | 迁移内容 |
| --- | --- |
| src/term/grid.rs | History 所有拥有/读写/淘汰/移交入口、容量诊断及旧参照测试。 |
| src/term/mod.rs | visible_row、row_by_id、同步 reflow_rows、resize_primary、高度拉回、终端测试。 |
| src/term/memory.rs | decoded 容量上界、同步/分步选择、工作集预留、额度测试。 |
| src/term/memory/stepped.rs | Source、分片重排结果、屏幕解码、历史接管、取消与退休、差分/容量测试。 |
| src/term/reflow.rs | 硬行复用谓词、输出拥有类型、旧算法比较。 |
| src/term/reflow/stream.rs | Source.row、逐格/窄段读取、输出压缩、spare 与取消销毁。 |
| src/term/reflow_reference.rs | 仅测试旧算法参照，保留 Raw 输入/结果。 |
| src/text.rs | 文本游标、跨软换行的 Cell 值读取。 |
| src/selection.rs | 单词边界、行选择、宽字符边界和复制。 |
| src/links.rs | 显式链接、自动 URL、前后字形扫描及测试。 |
| src/search.rs、src/shell.rs | 通过文本/坐标 API 间接读取，复核坐标和失效，不增加整份解码。 |
| src/render.rs | row_key、draw_backgrounds、draw_glyphs 只解码当前可见行；不会展开不可见历史。 |
| src/main.rs | 行末选择长度及退出/测试的文本迭代。 |
| src/session.rs、src/workspace.rs | 文本快照与状态测试的 iterator 生命周期。 |
| src/workspace/{authentication,remote_files}.rs | 测试中逐行文本收集。 |
| src/graphics/{tests,native_tests}.rs | Kitty 占位逐格检查与真实图片测试。 |
| src/term/{ascii_tests,corpus_tests,fuzz_tests,regression}.rs | 快慢差分、语料摘要、随机不变量、历史内容比较。 |
| src/perf_bench.rs、ci/perf-compare.py | 新增 frame_scrollback_view，并同步 history_bytes_10k 为真实保留容量上界。 |

连续解码用于渲染每行的扫描；随机装饰访问使用检查点。活动屏幕视图直接借用原 Cell，T3 的底部滚动不会解码历史。历史视图不建全历史缓存；允许同一可见行的独立扫描，新增 frame_scrollback_view 公开这一成本。

## 三条重排与准入

所有路径共用 RowView 读取和 StoredRow 拥有格式，屏幕仍解码为 Row。不能将压缩历史一次性展开成未记账 Vec<Row>。

- 同步路径只用于 decoded 上界小于 SYNC_REFLOW_LIMIT 的情况；先申请其完整输入/输出峰值，再允许解码和既有硬行捷径。take_rows 移交压缩数据及原票据，旧/新拥有者销毁顺序不变。
- UI 分步路径继续只读旧终端直到提交。Source 返回视图，Stream 逐行/逐 cell 读取；输出历史及时编码为 StoredRow，只保留当前行、有限 spare 及最终屏幕所需的 Raw 行。结果索引也按 StoredRow 计费。取消分批销毁结果，旧源完全保留；Ready 才交换网格/历史，退休阶段释放旧数据后释放票据。
- 流式内核保留原逻辑行/光标/宽字符算法，不能再用 `byte_limit/row_bytes(cols)` 当作压缩输出的正确保留行数。输出根据真实 StoredRow 成本裁剪，同时为屏幕与光标下方必要行留余量；不能因压缩输入的列数庞大而构造逻辑长行。
- prepare_resize_inner 根据历史原始列宽总和/可解码 Cell 上界决定同步还是分步，而非用压缩字节数误判。预留包括旧压缩输入（原票据继续覆盖）、新压缩输出及其新旧索引峰值、两屏目标 Grid、当前/备用 Raw 行和编码缓冲的重叠；不能从压缩 bytes/sizeof(Cell) 推算输入格数。历史输出上界按 byte_limit 与行数上限共同约束。高度变化只解码被拉回屏幕的几行，解码前由 Grid/工作集覆盖。

## 估算、验证与风险

200 列、100 个 ASCII 字符、其余全默认尾空白：旧行约 4032 B；新行约 100 B UTF-8 + 十余字节检查点 + 小头部 + 约 40–48 B 拥有索引，约 170–200 B/行，预期超过 15 倍。全宽 200 字符约 270–300 B，压缩较小；CJK 每字符三字节且保留 spacer，约 450–500 B；随机颜色和链接密集行可能回退 Raw，不承诺所有负载 15 倍。报告使用实际容量，不使用这些估算冒充测量。

验证包括 codec 往返、旧 History 后端差分（所有读取 API、选区/搜索/复制、缩放）、现有三条 reflow 差分、额度恰好/拒绝/取消/清空/淘汰和峰值检查，以及完整语料/随机回归。新增历史视图帧基准；固定指标以前一 HEAD 为基线，同命令 release 三次中位数。

主要风险：编码 CPU 使 parse_ascii_scroll 超过 10% 退化；每行独立分配及索引闲置容量使实际压缩率不足；稀疏旁表与检查点的边界错误；压缩后 decoded 规模远大于 64 MiB，旧重排模型少记额度；字节策略变化导致保留行数变化。对性能缺口如实报告，不能改基准负载。任何无法修复的正确性问题均撤回对应优化，并在审计材料说明，不提交会丢数据或漏记账的实现。

## 实现期间的设计修订

同步小输入路径也复用压缩流式内核，而不先整份解码：原计划额外维护一套解码峰值及硬行捷径，实际可与只读分步路径共用编码输出，减少差分面及输入峰值。同步路径移交 StoredRow 拥有者，UI 路径继续只读旧拥有者；旧 Raw 算法保留在 cfg(test) 参照中。修改不改变字符、光标和软换行规则。

历史字节上限与 used_bytes 都包含闲置索引槽，避免报表口径改变后工作区各窗格之和超过历史配额。小字节上限会限制初始索引槽数；满索引的增长若放不下，先按既有头部淘汰政策复用槽位。票据 64 KiB 余量仍不计为实际数据容量，持续由 History Kind 计费。

内部头部以 Packed 的内联计数字段表示，不把重复头部再写入字节区；布局不是持久化格式，不需要独立版本字节。所有 Cell 旁表位仍明确保存。

## 接手后的性能修订（P0）

审计复测发现逐行编码让滚动解析大幅退化（Linux：ASCII 198 → 112 MiB/s，CJK 低于 T0）。修订保持可逆性和记账口径不变：

- 网格行状态增加“已写范围”。打印路径通过专用入口只推进到写入末列，其余可变访问标为未知；编码和滚出后的清空只处理该范围，行尾空白不再逐格比较。
- `Color` 改为紧凑整数，属性游程比较变成整数比较；编码按计数遍得出的各段起点一遍写入，纯 ASCII 文本走逐字节快速路径。
- WIDE/WIDE_SPACER 从旁表移到每格 2 位的位图，中文行不再每格占一个 10 B 旁表项；EMOJI、超链接、marks 等其余位仍在旁表。
- 不写历史时的滚动保持原有整片填充，避免编译器放弃向量化。

## 复用上限（P1）

满历史时按原设计复用最旧行的缓冲，但原条件只要求“容量不小于需要”，缓冲只增不减：先出现一批长彩色行，后续短行会一直占着长行容量（Linux 实测 1 万行短行 13.66 MB，直接写入只需 1.53 MB）。现在复用要求容量不超过需要的 2 倍加 32 B；不满足时释放旧缓冲，按实际大小重新分配，准入峰值公式不变。
