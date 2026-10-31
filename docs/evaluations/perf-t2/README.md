# T2：PTY 输出缓冲复用

基于 T1 返工 HEAD `681f4295101c4680a2151a8f5c008174587029c4`。不新增 crate，保持原始 PTY 系统调用、输入队列、退出通知和 EOF 条件。

## 实现与背压评估

输出 ByteQueue 旁增加最多四块空闲 Vec，固定内联槽不分配堆索引。每块容量为 2/4/8/16 KiB 中的一档，最多 READ_CHUNK。读线程优先取同档缓冲；未命中时仅在四槽已满的情况下淘汰一块，先销毁旧缓冲再分配，避免旧/新容量超出预留。

ReadBuffer 始终保有一个完整初始化的 16 KiB 读缓冲。读取超过 8 KiB 时，把当前读缓冲直接移动给输出队列，并换入池中的下一块；2..=8 KiB 读取从相邻大小类复制，省去反复分配；小于 2 KiB 则沿用原长度 to_vec。主线程完整解析一块后 clear 并归还；超过池上限、非大小类或关闭后的迟到缓冲直接销毁。

固定复用 16 KiB 的原设想会让 4095 B 读取放大约四倍、32 B 读取放大 512 倍，1 MiB 上限最多容纳 64 块。选用大小类后，所有复用块容量不超过有效字节数的两倍，4095 B 只按 4096 B 计费；32 B 仍按 32 B。最坏仍可能比旧路径更早背压（如 8193 B 按 16384 B），但不超过两倍放大；没有改成按长度少记保留容量，也没有提高 1 MiB / 1024 块上限。FIFO、阻塞 push、close 唤醒生产者保持原语义。

原单独 Pty::spawn 仅供现有测试，保留旧无池路径；Session 使用 spawn_with_output 传入共享输出队列。ReadBuffer.packet 的无池分支就是原 buf[..n].to_vec，保持原生图像等独立 PTY 测试入口。生产读线程通过输出队列票据统一保护全部缓冲。

## 资源记账

没有新增 Kind 或每块 Lease；沿用 Queue 的整体容量票据。令 C=READ_CHUNK=16384，N=MAX_CHUNKS=1024，V=sizeof(Vec<u8>)：

```text
输入 = INPUT_LIMIT + 2*N*V
输出 = OUTPUT_LIMIT + (3+4)*C + 2*N*V
session_reservation = INPUT_LIMIT + OUTPUT_LIMIT + 7*C + 4*N*V
```

三块在途预留覆盖当前读缓冲、等待 push 的块和正在解析的块；四块覆盖空闲池。取新缓冲与转交旧缓冲的重叠也包含在三块中。大小类按最大 16 KiB 保守预留，实际池内容量可能更小。

64 位目标 V=24：输出预留从 1,146,880 B 到 1,212,416 B；会话两队列从 9,584,640 B 到 9,650,176 B，均增加 **65,536 B**。workspace 等现有准入调用 session_reservation 自动采用新公式；没有在运行时不记账地扩容。内联元数据遵循 T1 审计澄清，不另计堆容量。

- ByteQueue 的 chunks/free 在 memory 票据之前销毁；close 先销毁队列和空闲池数据，票据继续由活着的拥有者持有。
- ReadBuffer 的 data 字段先于 pool Arc 销毁，读线程未结束时不能归还额度；Windows UI 关闭后继续排空时同样成立。
- Session.pending_output 仍先于 _output_memory 销毁，归还操作发生在解析完成、借用结束之后。
- resource_bytes 的输出容量现在包含空闲池真实保留容量；queued bytes 仍仅是队列块容量，不把空闲池误当待解析输出。EOF 判断仍只检查原队列/解析状态，空闲缓冲不阻塞退出。

## 关闭与平台时序

Unix 读循环只替换缓冲拥有者与 Data 构造，poll、进程退出后排空内核、EIO/EOF 处理都不改。Windows forward=false 后仍调用 output.read，跳过 packet/通知，直到读端结束，保留“UI 关闭后排空 ConPTY”的原逻辑。close 与归还共用队列 Mutex；锁内不执行 I/O，分配/初始化位于锁外。

## 测试与失败场景

- output_pool_is_bounded_cleared_on_close_and_keeps_its_lease：五块归还只保留四块，满池换大小类时淘汰，close 清空，迟到归还不重建池，最后读拥有者仍持有票据，数据销毁后 used 归零。
- different_size_classes_survive_until_the_pool_is_full：未满池时不同档位共存，原地址可再次取回，避免交错读取大小持续分配。
- read_packets_preserve_bytes_and_small_reads_keep_length_billing：2047/2048/4095/4096/8192/8193/16384 B 的数据与容量；128 个 32 B 块仍只记 4096 B，防止固定 16 KiB 池在 64 块提前背压。
- session_preflight_covers_both_queues_and_refusal_returns_no_credit_early：预留少一字节时输出队列准入失败且输入额度不变；恰好总预留能创建两队列，销毁后全部归还。
- real_pty_preserves_output_across_backpressure_and_exit 改为实际池路径：真实 macOS shell 输出 2 MiB 零字节及 tail，确认背压后完整输出、尾部、退出码 7 和 EOF。
- pty_read_path_allocations：同一读缓冲交接组件预热后反复 4096 次，检查数据长度、分配次数和额度回到初值；alloc-count 下断言零分配。该测试进入常规回归，不增加 ignored 测试。
- 原 flow 输入拒绝/部分写入/背压/关闭，以及 Session 暂停解析、关闭阻塞读线程、EOF 先排完输出、网格准入先于子进程等测试全部保留。

## 分配测量口径与复现

旧基线是在 681f429 的产品代码上，仅追加测试夹具后测量的原 to_vec → push → pop → drop 读缓冲交接路径；尚未实现池。之后同名夹具改为生产 ReadBuffer → push → pop → recycle。输入为完整初始化的 16 KiB 缓冲，分别模拟 16384 与 4095 B 的读取；先预热 64 次，再统计 4096 次。队列索引增长和初次分配在预热中，不混入稳态分配数。

计数器直接运行读缓冲交接组件，因此覆盖生产相同的 Rust 分配入口，不含操作系统 I/O、通知 API、其他线程、系统分配和端到端吞吐。不能把“组件预热后零分配”说成整个 PTY 线程/系统完全零分配。小于 2 KiB 的交互式读取仍会按原长度分配；尺寸持续变化或池饱和也可能未命中。

```sh
# before 为实现池之前的产品代码+同名夹具；after 为本任务代码。
for run in 1 2 3; do
  cargo test --release --bin congminal perf_bench::pty_read_path_allocations -- --exact --nocapture --test-threads=1
  cargo test --release --bin congminal --features alloc-count perf_bench::pty_read_path_allocations -- --exact --nocapture --test-threads=1
done
# 其余固定指标沿用 T0 的同一条命令，默认与 alloc-count 各三轮。
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1
```

实际采样按每种配置完整顺序三轮，不在测量时并行编译或跑回归。原始输出在 target/evaluations/t2-before-read-*、t2-read-*、t2-{default,count}-*。早期池未满时过早淘汰不同大小类的试测实现已修复，日志留在 t2-trial-a-*；最终结果不拼选早期样本。

## 改动文件

| 文件 | 用途 |
| --- | --- |
| src/pty/flow.rs | 有上限的大小类空闲池、预留公式、读缓冲交接、容量诊断和针对性测试。 |
| src/pty/unix.rs | 读线程连接共享池，真实 PTY 背压测试使用新路径。 |
| src/pty/windows.rs | 同一缓冲交接接口，保留关闭后继续排空分支。 |
| src/pty/mod.rs | 同步生产启动接口的模块文档。 |
| src/session.rs | 传入共享输出队列、解析后归还缓冲、诊断包含闲置容量。 |
| src/perf_bench.rs | 原路径/新路径同名分配测量与零分配常规回归。 |
| docs/foundations.md | 同步 Queue 额度、大小类及背压口径。 |
| docs/evaluations/perf-t2/README.md | 审计材料、全部基准与限制。 |

机器为 MacBook Air Mac16,12、Apple M4、16 GiB、macOS 27.0.1 (26A434)、rustc 1.99.0；AC Power、电池 100%。macOS arm64 实际运行了真实 PTY 测试和软件基准；**Linux/X11、Windows、macOS Intel 未在真机验证**，仅做交叉 Clippy 检查。

## 三轮中位数：全部固定指标

Δ 是数值变化，MiB/s 增加为改善，us/frame 增加为退化。这些软件解析/绘制基准不连接 PTY，观察到的波动不归因于本任务读缓冲复用。

| 指标 | 单位 | T1 返工默认 | T2 默认 | Δ | T1 返工 alloc-count | T2 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | 0.00% | 20.000000 | 20.000000 | 0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | 0.00% | 32.000000 | 32.000000 | 0.00% |
| `history_bytes_10k` | bytes | 40082112.000000 | 40082112.000000 | 0.00% | 40082112.000000 | 40082112.000000 | 0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | 0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 638188.000000 | 638188.000000 | 0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 445.900716 | 413.492343 | -7.27% | 381.048682 | 381.297176 | +0.07% |
| `parse_ascii_noscroll` | MiB/s | 1203.975927 | 1191.676889 | -1.02% | 1192.681787 | 1204.267631 | +0.97% |
| `parse_ascii_2027` | MiB/s | 415.733799 | 347.753951 | -16.35% | 357.351908 | 378.709429 | +5.98% |
| `parse_sgr` | MiB/s | 334.188822 | 269.494582 | -19.36% | 298.083243 | 301.550808 | +1.16% |
| `parse_cjk` | MiB/s | 255.761033 | 224.253318 | -12.32% | 209.182114 | 214.607116 | +2.59% |
| `frame_full` | us/frame | 3581.495841 | 3579.918594 | -0.04% | 3555.360693 | 3476.717918 | -2.21% |
| `frame_one_row` | us/frame | 246.295743 | 247.366558 | +0.43% | 246.607559 | 244.251994 | -0.96% |
| `frame_scroll_1line_growing` | us/frame | 2067.448640 | 2063.750281 | -0.18% | 2044.061386 | 2026.525286 | -0.86% |
| `frame_scroll_1line_full` | us/frame | 2048.476998 | 2055.401372 | +0.34% | 2029.447796 | 2027.152533 | -0.11% |
| `frame_unchanged` | us/frame | 64.597664 | 64.384104 | -0.33% | 64.875556 | 63.405139 | -2.27% |

## 读缓冲交接分配指标

| 输入块 | 复用前 allocs/MiB | 复用后 allocs/MiB | 变化 |
| --- | ---: | ---: | ---: |
| 16384 B | 64.000000 | 0.000000 | -100% |
| 4095 B | 256.062515 | 0.000000 | -100% |

两种大小的三轮分配数完全一致，波动 0%。默认构建对应指标全部 skipped，未把关闭计数当成零分配。目标“持续输出的读缓冲分配显著下降”在此稳态组件测量中达成；池首次填充、大小变化及小于 2 KiB 的读取不在零分配保证内，整个 PTY 线程/操作系统分配仍未做完整计数。

固定基准的三轮波动超 5% 项：

- default：parse_ascii_scroll 13.65%；parse_sgr 15.71%。
- count：parse_sgr 14.14%；parse_cjk 7.48%。

## 必需检查与状态

- cargo fmt --check：通过。
- 四目标 cargo clippy --all-targets --target <target> -- -D warnings：x86_64-unknown-linux-gnu、x86_64-pc-windows-gnu、aarch64-apple-darwin、x86_64-apple-darwin 全部通过；本机 alloc-count Clippy 也通过。
- RUSTUP_HOME=/tmp/congminal-perf-rustup cargo +1.88 check：通过，实际 Rust 1.88.0，沿用 T0 的临时映射。
- cargo test -- --test-threads=1：主程序 924 passed / 56 ignored，helper 21 passed。
- cargo test --features alloc-count -- --test-threads=1：主程序 926 passed / 56 ignored，helper 21 passed。
- flow 子集 10 项、真实池路径 PTY 用例、read 分配常规测试均通过；以上用例仍在完整回归内，没有用子集代替全量。

状态：T2 读缓冲复用、Queue 记账和背压/退出正确性目标完成。全部固定指标如实公开，不能把此状态解读为所有软件基准都无回归。比较工具因三轮波动及默认构建部分吞吐下降返回失败；原始记录在 t2-*-comparison.txt，未删除或替换样本。

## 已知问题（按严重程度）

1. 默认软件基准的 parse_ascii_2027 / parse_sgr / parse_cjk 相对上个 HEAD 的观测分别下降 16.35% / 19.36% / 12.32%。本任务未改 Parser/Terminal/Renderer，计数组也没有相应下降；原因未独立定位，不将其直接解释为已证实的布局噪声。后续需审计复测，不能将此组下降悄悄从总表删除。
2. 默认组与计数组存在表中列明的 >5% 波动，不宣称固定基准整体稳定性通过。
3. 小于 2 KiB 的读取、首次填池和未命中仍可能分配；只对所测稳态组件保证零分配，尚未测整个原生读线程的所有分配。
4. 中等大小块按容量舍入，可能使有效载荷背压阈值降低到旧路径约一半；预算不漏记，数据顺序和完整性保持。
5. Linux/X11、Windows、macOS Intel 未在真机验证；特别是 ConPTY 排空仅保持原控制分支并交叉检查，不能代替 Windows 运行验证。
