# 资源边界与实现

窗口直接使用 AppKit、Win32、X11；终端、标签和侧栏由软件渲染器绘制。输入经有界队列写入 PTY，输出分批交给解析器和字符网格，再更新有变化的像素。

## 内存和队列

| 路径 | 限制及行为 |
| --- | --- |
| PTY 输出 | 1 MiB、1024 块；满时停止读取，向子进程背压 |
| PTY 输入 | 8 MiB、1024 笔，包含正在发送的一笔；不能准入时整笔拒绝 |
| 输出处理 | 每轮至多 128 KiB，约 2 ms 合作式时间目标，每 1 KiB 检查时间 |
| 工作区历史 | 行数和字节双重限制，共享历史额度；旧行按需淘汰 |
| 主/备用网格 | 每网格至多 1,048,576 格，最多 4096 列、2048 行；字符格 20 B |
| 帧缓冲 | 最多 64M 像素，单边最多 16384；拒绝超限或分配失败 |
| Kitty 图片 | 最多 16 张 / 16 MiB RGBA 容量，单图 4 Mi 像素，最多 32 项放置 |
| Kitty 输入 | 单条 APC 5120 B、Base64 内容 4096 B、累计分块 16 MiB；异常输入拒绝 |
| SFTP | 2 工作槽、64 等待槽、16 MiB 子池；目录和递归操作另有限额 |
| 配置 | 文件 1 MiB、嵌套 32 层、键路径 32 段、4096 个语法条目 |

额度覆盖受管资源及已接入的临时工作集，**不代表整个进程 RSS 的硬上限**。系统库、窗口表面、线程栈、字体映射和外部 SSH/编辑器还需单独测量。字体映射有独立额度；具体配置见 [配置参考](configuration.md)。

2 ms 是合作式调度目标，不是复杂指令或渲染工作的实时保证。输入入队整笔准入；子进程中途退出仍可能只收到部分数据。

## 图片与 Windows 适配

支持 Kitty 静态直接传输、分块、RGB/RGBA/PNG、raw zlib、普通及 Unicode 占位符放置。文件/共享内存传输、动画、相对放置及其他未实现扩展返回不支持。放置色表至多 256 项，在终端重置前不重用，避免历史内容被新图片重定向。

解码、输入与原缓存会短暂重叠，16 MiB 缓存不是流水线峰值。未完成分块在 30 秒后遇到下一批输入时回收，输入结束立即清理；不为超时增设线程。

Windows 附带固定版本的微软 ConPTY。APC 应答中的 ESC 通过 Win32 Unicode 输入记录保留，避免真实图片库在非 VT 输入模式下错误回退。该适配输入最多 4096 B，临时编码栈缓冲固定 16 KiB；普通键盘及粘贴保持原路径。

三平台本地会话默认使用主目录，显式目录优先；`.` 表示继承启动目录。主目录候选限制为绝对路径和最多 4096 个路径编码字节，Windows 查询另有 4096 个 UTF-16 单元上限。普通窗口标题固定 `congminal`，程序标题保留在标签中。

## 验证与数据

常规检查为 `cargo fmt --check`、`cargo clippy --all-targets -- -D warnings` 和单线程完整回归。CI 另检查最低 Rust 版本、三平台真实 PTY、真实图片库、原生认证传输和窗口启动/退出。

最新范围见 [当前版本验收](evaluations/v0.2.3/README.md)。历史基准、原始数据与源码快照见 [归档](archive/README.md)；每份结果只代表其记录的提交、机器和负载，不能代替当前版本或所有平台的测量。

## 侧栏绘制检查（2026-10-31）

侧栏样式、引导和监控趋势条共用软件绘制器，不增加字体、纹理缓存、后台线程或动画计时器。引导文字每段最多处理 256 个字符、8 个段落，按可见行绘制；工具探测最多保存 4 项，配置未变时复用探测结果。趋势图直接遍历现有定长采样历史，最多画 48 个桶，不分配图表点集。

本机 macOS arm64 的 release 模式测量：30×24 格侧栏，预热 10 次后重复绘制 1000 次（含页签、按钮、说明和 120 个样本的图表），总计 39.10 ms，平均 39.1 μs/次；跟踪额度从 546607 字节到 546607 字节，工作线程从 0 到 0。命令：`cargo test --release --bin congminal render::chrome::tests::warmed_sidebar_paint_cost_and_resource_stability -- --exact --ignored --nocapture --test-threads=1`。

该数字只覆盖软件绘制，不代表整个窗口、系统合成、完整堆分配或 RSS。主程序 908 项、辅助程序 21 项本机回归通过；Windows GNU、Linux GNU 完成全目标交叉编译检查，不能替代两平台的原生窗口验收。

## macOS 原生像素提交

原生快照按实际像素容量和拥有者元数据申请共享 Canvas 额度，由 CGDataProvider 回调在系统释放图像时归还，来源帧不会与异步使用的像素混用。差分范围最多八段，超过时合并；macOS 最多保留 16 个局部图层、局部像素合计最多 4 MiB，超限或尺寸变化回到完整底图。系统图层与 GPU 缓存尚非完整账本，不能把准入额度当作 RSS 上限。

macOS 的完整快照在 provider 释放后可归还最多一块的同尺寸复用池（T6 原为三块，4K 下空闲常驻约 100 MB，P1 降为一块），像素及原 Lease 一起保留，局部条带仍直接释放。每块票据为 `pixels.capacity()*4 + size_of::<Snapshot>()`；池自身先申请 `size_of::<Mutex<PoolData>>() + 2*size_of::<usize>() + align_of::<Mutex<PoolData>>()` 的 Canvas 额度，覆盖 Arc 头部及对齐的上界。池内固定数组不另分配；注册表为固定八个内联句柄，注册槽满则沿用不复用的快照路径。借用屏幕像素不能进入池，所有快照仍独立复制。

尺寸变化、关闭窗格、清历史或不可见时清空池并推进代次，旧回调不能重新填入已清空的池。回调先释放 Box 外壳，再把像素与票据移入池；最后一个句柄用 Arc::into_inner 先释放共享分配，再销毁池内数据并归还额度。池中 Snapshot 元数据仍保守计费，不把复用容量当作零。不可见窗口停止绘制和光标计时，继续处理 PTY，恢复时 invalidate 整屏；焦点、监控与 Git 的原门控独立保留。

关闭窗格、会话解析到清历史/RIS、不可见事件后提示系统分配器归还空闲页；全进程以五秒为最小间隔，清池不受此间隔阻挡。glibc 使用 malloc_trim(0)，musl 排除此调用；macOS 使用 malloc_zone_pressure_relief(NULL,0)，Windows 使用 HeapCompact(GetProcessHeap(),0)。提示不保证 RSS 降低，受管额度、系统分配器空闲页与 RSS 分开报告。

M4 本机相同万行 Neovim 4K 缓冲负载、前后各三轮诊断测量中，活动 CPU 中位数由单核 49.47% 到 21.80%，终端 RSS 采样峰值中位数由 270.86 MiB 到 194.69 MiB。修正标准 sRGB 与原生 BGRA 提交，并减少局部变化的整屏上传。同步绘制/提交 P95 从 2.89 ms 到 8.24 ms，未宣称延迟或 FPS 提升：此前数字不含后续 Core Animation 颜色转换。完整本机记录在 `target/evaluations/macos-presentation-optimization-20261031/`，不纳入历史版本证据，也不代表跨机器或外接 4K 实屏验收。

## PTY 输出缓冲复用

输出队列保留最多四块空闲缓冲，按 2/4/8/16 KiB 大小类复用；小于 2 KiB 的读取仍按长度拷贝，避免交互式小块被按 16 KiB 计费。较大块容量最多放大到有效数据的两倍，1 MiB 与 1024 块背压上限不变。解析完才归还；关闭先销毁空闲数据，读线程结束及最后拥有者释放之后才归还 Queue 额度。

令 C=16384、N=1024、V=sizeof(Vec<u8>)，输出 Queue 预留为 `OUTPUT_LIMIT + 7*C + 2*N*V`，覆盖三块在途缓冲和四块空闲缓冲。会话两队列预留为 `INPUT_LIMIT + OUTPUT_LIMIT + 7*C + 4*N*V`，相对旧公式增加 64 KiB；池中实际容量继续计费。空闲槽为内联固定数组，不创建额外堆索引。资源诊断的输出容量包含空闲池，待处理输出/EOF 则只看真实队列和解析中的块。

## ASCII 写入与字素簇状态

VT Ground 且 UTF-8 解码器空闲时合并可打印 ASCII；连续高位字节用标准库校验最长合法 UTF-8 前缀，直接分发 char，非法或截断后缀沿用原逐字节解码器。终端在 IRM、非 ASCII 字符集、前一簇能与当前 ASCII 合并时沿用逐字符入口；批次 revision、行尾 pending_wrap、滚动和宽字碎片清理保持原语义。

2027 的边界判断保存 Segmenter，先校验实际 Cell 再增量 push；Cell 不匹配时从格子和 marks 迭代重建，不构造临时 String。附加成功后同步推进状态，拒绝附加时保留旧状态；只存在 flags 中的变体选择符不补入分段文本，以保持原有行为。RIS 随终端初始化清空状态。

每终端的内联缓存 `Option<(Cell, Segmenter)>` 不产生堆分配，与 cursor、modes 等内联字段一样不计入额度。T7 行状态数组的堆容量另纳入下面的 Grid 公式；创建/缩放仍先申请额度，失败保持原网格，不新增 Kind 或 Lease。

## 行修改计数与网格额度

T7 的 Grid 另有与行同步移动的 RowState 数组，release 元素为 8 B 修改计数，debug_assertions 时加入旧内容哈希及有效标志，元素 24 B。所有 IndexMut 及网格内部修改入口均推进计数，整体/局部滚动同步搬移状态。Grid/History 的唯一存储身份、clock 是内联字段；Row 仍为 32 B，压缩历史行索引口径不变。

令 R=rows、C=retained_cols(cols)、I=max(2R,32)、S=sizeof(RowState)，两屏 Grid 预留为 `2*((R+1)*C*sizeof(Cell)+I*(sizeof(Row)+2*S))`；2*S 覆盖状态数组重分配时新旧重叠，allocated_bytes 按 states.capacity()*S 计实际保留容量。原有新旧目标网格重叠 Lease 与 Reflow 工作集另外保持，准入失败不取走原网格。80×24 相比旧公式增加 release 1536 B / debug 4608 B，状态容量不会随逻辑长度清零而停止计费。

渲染行键由四元组 `(text_epoch,line_id,storage_identity,revision)` 和装饰哈希构成，release 40 B、debug 48 B（含防漏画旧内容哈希），旧帧/工作数组及每窗格 ViewState 仍由 Tracked<Vec> 按真实容量计入 Kind::Render。release 跳过逐格哈希；调试/测试构建在计数未变而内容哈希变化时 panic，历史行也由上一帧键检查，语料及随机差分逐块执行两屏校验。

## 性能基准

基准仅通过 `#[cfg(test)]` 挂载，不进入产品 release 构建。默认测试不注册全局分配器；非默认 `alloc-count` feature 仅在测试时注册转发 System 的计数分配器，旧源码注入探针无需修改。

```sh
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --features alloc-count --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
python3 ci/perf-compare.py run-1.log run-2.log run-3.log
```

同一台接通电源的机器、同一命令顺序运行三次取中位数。每个解析/绘制/历史指标使用独立的 1 GiB 根 Budget 和 256 MiB 字体映射额度；指标前后断言拒绝数为零、没有历史回收请求、销毁后 used 回到初值。输入沿用 Queue 额度，Tracked 先申请容量再分配、先销毁 Vec 再释放 Lease；不修改产品资源公式或默认额度。

固定输出为 `metric value unit`；未启用分配统计时输出 `allocs_* skipped (alloc-count disabled)`（累计字节指标同样 skipped），不能将未测量值视为零。计数仅覆盖当前测量线程成功的 alloc/alloc_zeroed/realloc 调用；不含系统分配、RSS、打印、输入生成及预热。

200×60 网格、1 KiB 输入块；ASCII 滚动/不滚动、SGR、CJK 输入各 64 MiB，2027 为 16 MiB，每个解析场景先运行完整输入预热。ASCII 每行 100 字符加 CRLF，不滚动场景每 59 行归位并断言历史为空；滚动场景断言历史填满 10000 行。

绘制使用默认字体、2.0 缩放和满屏彩色 ASCII，各场景独立创建并预热 64 帧。full/one_row/unchanged/scroll_growing/scroll_full 分别测量 1800/25000/60000/3000/3000 帧的平均 draw 微秒数；准备输入不计时。one_row 交替修改中间一格，无垂直溢出字形时只画一行，有溢出时保守扩展到三行；unchanged 断言没有 damage；两个 scroll 场景分别断言历史未满和已满。不同字体/机器不能直接比较。

`history_bytes_10k` 为写入 10000 行后的 `history().used_bytes()`，其中 9941 行留在历史、59 行仍在屏幕。T4 起包含实际压缩缓冲容量及完整行索引（含闲置槽），不含 64 KiB 票据余量或 RSS；T0–T3 的历史报告保留当时 Row 元数据与 Cell 容量的旧口径。`history_bytes_10k_cjk` 用同样方式写入每行 40 个汉字的中文行，衡量宽字符行的压缩效果。结构大小用 `size_of::<Cell>()` 和 `size_of::<Row>()` 输出。

分配器拒绝/unwind 和 System 调用自检进入常规回归，仅性能 benchmarks 忽略。比较工具检查基准自身的通过标记及测试总体通过状态，不依赖通过测试的数量。

比较工具要求三轮 `(最大值−最小值)/中位数 < 5%`，配置不同、字段缺失、重复、非法数值或单位变化均失败；零值及 skipped 单独处理。可加 `--baseline old-1.log old-2.log old-3.log` 检查旧门槛（吞吐至少 90%、帧耗时最多 110%、内存和分配不增加）；各任务的更严格验收另行核对，不能用该通用门槛代替。

```sh
cargo build --release --bin congminal
bash ci/rss-sample.sh target/release/congminal target/evaluations/rss-run
# Windows（需现有发布流程的 ConPTY 文件）
./ci/rss-sample.ps1 -Binary target/release/congminal.exe -OutputDirectory target/evaluations/rss-run
```

RSS 脚本需要原生图形会话。独立配置输出顺序数字及 16 MiB 文本，开始门控前采样 rss_start，生成器完成时采样 rss_end，之后等待三秒再以 200 ms 间隔采样五次取 rss_idle 中位数，同时报告 rss_peak。结束标记仅表示生产者写完，不证明终端已排空队列。采样仅含终端主进程；退出后零值不参与。正常和异常路径清理自己启动的进程树及临时目录；原始输出留在 target/evaluations，不纳入版本控制。

## 渲染滚动与字形索引

在没有图片放置、画布尺寸不变，且（底部视图时）为完整滚动区的情况下，按顶行 line_id 的差值向上或向下平移整块像素及行指纹；新露出的行和首/末行（含边距）重新绘制，Damage 覆盖整个平移画布。回看历史同样平移（P2 起）：全帧键只含“是否在底部”而非具体偏移；滚动条滑块单独记录上一帧的像素范围，滑块移动或其下方的行重画时，重画新旧位置覆盖的行并重新叠加滑块。离开或回到底部、图片 revision、字体缓存 epoch 变化仍强制整屏重画。

字形光栅缓存记录垂直溢出，Fonts 保守记录清缓存以来是否见过溢出；首次在局部重画中发现溢出会立即完整重画，避免旧像素。清缓存同步清除此状态，并推进共享缓存 epoch，其他窗格的旧视图下次绘制也会失效。

ASCII 单格字形（128×粗体×斜体）由字体对象持有的固定数组直接保存（P2 起，原为指向主缓存条目的地址表），不进哈希表、不参与主缓存淘汰。槽位数组按真实 Vec 容量记 Kind::CacheIndex，字形按主缓存同样的模型（128 B + 像素容量）记 Kind::Glyph；任一申请被拒绝时退回主缓存，本轮不在命中路径重试，清缓存后重新申请。CachedGlyph/Cluster 新增的溢出字段由现有 CacheIndex 桶容量公式 sizeof(V) 自动覆盖；像素的 FontRaster 等票据不变。画布平移、颜色并行计算与背景合并不创建像素副本或额外 Lease。

## 紧凑历史与重排容量

活动网格仍使用 Row/Vec<Cell>。历史按物理行保存 UTF-8 文本、每 16 格一个字节偏移、11 B 属性游程（列/fg/bg/样式）、有宽字符时每格 2 位的 WIDE/WIDE_SPACER 位图，以及 10 B 稀疏旁表（列/其余 flags/完整 marks）。Color 是紧凑整数（默认、调色板、真彩色三种互不相同），按种类处理时用 `kind()`；旁表保留超链接、EMOJI、mark id 及 Kitty 色位。网格每行记录“此列之后全是默认格”的范围：通用可变访问把它设为未知，打印路径只推进到实际写入的列，滚入历史时编码与清空只处理这一段；调试构建和网格校验会检查范围之外确实全是默认格。满历史复用最旧行的压缩缓冲时，容量不得超过需要的 2 倍加 32 B，过大的旧缓冲直接释放、按需重新分配，避免短行长期占着长行的容量（`history_bytes_10k_after_long` 衡量这一点）。仅裁去逐字段完全默认的尾空白，原始列宽和 wrapped 保存在内联头部；复杂行可回退 Raw，读取返回按值 Cell 的借用视图，不分配整份解码缓存。

令 S=sizeof(StoredRow)，实际历史占用为 `索引.capacity()*S + Σ(payload.capacity())`；Raw payload 为 Cell 容量乘 sizeof(Cell)。used_bytes 与字节上限都采用此口径。索引增长若超过上限则按原头部淘汰政策复用槽位；小配额也限制初始槽数。Kind::History 仍按 64 KiB 聚合申请，覆盖旧/新索引与编码缓冲重叠，精确申请可作为聚合申请拒绝后的回退；释放数据后才收缩票据。满历史的等容量替换只重写原缓冲，不分配或修改额度。

同步与 UI 分步重排均使用压缩流式结果，保留旧光标、宽字和软换行规则。同步移交压缩输入及票据，分步只读旧输入直到交换；输出先编码，屏幕行才解码，取消/退休分批销毁。同步阈值使用 `decoded_bytes = Σ(row_bytes(原始列宽))`，不能拿压缩字节数当 Cell 数；旧 Raw 算法与旧 History 仅在 cfg(test) 的独立参照入口保留。

宽度变化的 Reflow 工作集模型：D 为历史 decoded_bytes 加原主网格 allocated_bytes，n 为历史行数加原屏幕行数，C 为目标列宽，H 为历史字节上限，L 为历史行数上限，r 为原屏幕行数。令 `possible=(D/sizeof(Cell))/max(C-1,1)+n+1`，`K=min(possible,L+r,H/S+r)`，`R=row_bytes(retained_cols(C))+S`，则额外工作集为 `min(H,K*R)+r*R+6*(K+1)*S+8*R+65536`。索引六份保守覆盖重排结果与历史交接的新旧容量，Raw 行项覆盖当前/备用/屏幕与编码重叠；旧源由原票据覆盖，不展开整个 decoded 上界。高度变化沿用顶部行迁移模型，索引元素改用 S。目标两屏 Grid 仍另留旧/新重叠预留，任何申请失败都在取走源行之前返回。

新增 frame_scrollback_view 使用同一 200×60/2×缩放场景，先填满 10000 行历史，再在偏移 1000 附近交替上下看一行；预热 64 帧，测量 600 帧。它只解码/绘制可见历史；完整输出仍为 metric/value/unit，不用此新场景替换已有滚动基准。比较器允许旧基线没有该新增指标，当前日志则必须包含。

## 工作区共享画布

启用工作区后只有一份拥有像素的总画布。Frozen 保存逻辑 viewport 和 ViewState，不再拥有窗格 Framebuffer/Canvas 票据；单窗格非工作区模式仍使用原拥有画布。新增窗格仅为 Queue 与 Grid/Reflow 预检，不再预留窗格像素；所有可见网格计划准备成功后，relayout 仅申请/调整一次总画布，再提交各窗格的逻辑尺寸。恢复布局同样使用这个总画布准入流程，隐藏标签不分配像素。

Framebuffer 增加 stride，并可借用父画布矩形内的像素跨度。借用视图没有 Lease/Vec 容量，Rust 借用保证父像素与 Canvas 票据存活；拥有画布仍先申请新旧缓冲重叠额度，再分配/交换。所有光栅行偏移使用 stride，宽高用于裁剪；跨 stride 滚动逐行仅移动视图宽度，不改写跨度中的相邻窗格。Damage 保持局部坐标，Workspace 裁剪并换算到窗口坐标。

只读/输入定位使用每窗格逻辑 viewport，避免用空的单窗格拥有画布推算鼠标/输入法边界。布局满帧清空总画布后强制重画全部可见窗格，切换 ViewState 仍保留各窗格的顶行身份；字体 generation/cache epoch 规则不变。认证/传输覆盖层继续在窗格与 chrome 之后绘制，关闭覆盖层沿用 full/invalidate 语义。

软件 4K（3840×2160）四窗格的 Canvas 实测由 65,645,504 B 到 33,177,600 B，减少 32,467,904 B（30.96 MiB）；只含受管像素，不是 RSS/系统合成/GPU 额度，未在外接 4K 显示器验证。总画布在缩小时仍可保留历史容量并持续计费，不把 stride 视图的跨度重复计入 Canvas。
