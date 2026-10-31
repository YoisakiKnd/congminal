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

M4 本机相同万行 Neovim 4K 缓冲负载、前后各三轮诊断测量中，活动 CPU 中位数由单核 49.47% 到 21.80%，终端 RSS 采样峰值中位数由 270.86 MiB 到 194.69 MiB。修正标准 sRGB 与原生 BGRA 提交，并减少局部变化的整屏上传。同步绘制/提交 P95 从 2.89 ms 到 8.24 ms，未宣称延迟或 FPS 提升：此前数字不含后续 Core Animation 颜色转换。完整本机记录在 `target/evaluations/macos-presentation-optimization-20261031/`，不纳入历史版本证据，也不代表跨机器或外接 4K 实屏验收。

## ASCII 写入与字素簇状态

VT Ground 且 UTF-8 解码器空闲时合并可打印 ASCII。终端在 IRM、非 ASCII 字符集、前一簇能与当前 ASCII 合并时沿用逐字符入口；批次 revision、行尾 pending_wrap、滚动和宽字碎片清理保持原语义。

2027 的边界判断保存 Segmenter，先校验实际 Cell 再增量 push；Cell 不匹配时从格子和 marks 迭代重建，不构造临时 String。附加成功后同步推进状态，拒绝附加时保留旧状态；只存在 flags 中的变体选择符不补入分段文本，以保持原有行为。RIS 随终端初始化清空状态。

每终端新增内联缓存 `Option<(Cell, Segmenter)>`，无堆分配。Kind::Grid 的预留在原两屏公式上增加 `S = size_of::<Option<(Cell, Segmenter)>>()` 一次；创建/缩放仍先申请额度，失败保持原网格。缩放的新旧网格预留重叠阶段会保守覆盖两份 S，提交后只保留一份。缓存先于 grid_memory 票据销毁，不新增 Kind 或独立 Lease。

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

绘制使用默认字体、2.0 缩放和满屏彩色 ASCII，各场景独立创建并预热 64 帧。full/one_row/unchanged/scroll_growing/scroll_full 分别测量 1800/25000/60000/3000/3000 帧的平均 draw 微秒数；准备输入不计时。one_row 交替修改中间一格，当前实现 damage 为三行；unchanged 断言没有 damage；两个 scroll 场景分别断言历史未满和已满。不同字体/机器不能直接比较。

`history_bytes_10k` 为写入 10000 行后的 `history().used_bytes()`，其中 9941 行留在历史、59 行仍在屏幕，包含 Row 元数据和 cell 容量，不含索引闲置容量、块余量或 RSS。结构大小用 `size_of::<Cell>()` 和 `size_of::<Row>()` 输出。

比较工具要求三轮 `(最大值−最小值)/中位数 < 5%`，配置不同、字段缺失、重复、非法数值或单位变化均失败；零值及 skipped 单独处理。可加 `--baseline old-1.log old-2.log old-3.log` 检查旧门槛（吞吐至少 90%、帧耗时最多 110%、内存和分配不增加）；各任务的更严格验收另行核对，不能用该通用门槛代替。

```sh
cargo build --release --bin congminal
bash ci/rss-sample.sh target/release/congminal target/evaluations/rss-run
# Windows（需现有发布流程的 ConPTY 文件）
./ci/rss-sample.ps1 -Binary target/release/congminal.exe -OutputDirectory target/evaluations/rss-run
```

RSS 脚本需要原生图形会话。独立配置输出顺序数字及 16 MiB 文本，开始门控前采样 rss_start，生成器完成时采样 rss_end，之后等待三秒再以 200 ms 间隔采样五次取 rss_idle 中位数，同时报告 rss_peak。结束标记仅表示生产者写完，不证明终端已排空队列。采样仅含终端主进程；退出后零值不参与。正常和异常路径清理自己启动的进程树及临时目录；原始输出留在 target/evaluations，不纳入版本控制。
