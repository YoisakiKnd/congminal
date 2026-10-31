# T3：滚动平移与软件渲染热路径

前一任务 HEAD：`626e070084f2eb98cc99c067e97954a51983f7b4`。不新增 crate。

## 实现与失效核对

- 底部视图以顶行 line_id 的正向差值 k 判断整屏上滚，仅 0<k<rows 时平移完整画布（含 padding）和 last_rows；第一行及新尾行强制重画。第一行修复上边 padding，尾行修复下边 padding。Damage 覆盖所有移动像素，未仅返回新尾行。
- 非底部视图、局部滚动区、Kitty 放置、graphics revision、尺寸变化、k>=rows 或倒退都不平移。旧网格大于画布时回到完整绘制，copy_within 范围不越界。
- frame_key 移除 history.len。滚动条只在 display_offset!=0 出现，此时历史长度保留在行装饰键；装饰变化回到整屏重画，避免半透明滚动条重复叠加。底部视图无滚动条。
- 光标、选区、搜索匹配/当前匹配、命令标记仍进入各行装饰指纹，平移后仍逐行比较；移动的光标及选区边界会重画。UI 状态栏只在最后一行哈希 Some 值，旧状态栏平移到内部行时指纹不同而被擦除。图片 revision 仍在全帧键，底色、缩放/字体 generation、text_epoch 仍在原失效键。
- ViewState 同时保存/交换顶行身份。共享字体缓存清理推进独立 cache_epoch，让暂未绘制的其他窗格也完整重画；不推进会触发工作区布局变化的 generation。
- 每个光栅字形记录是否跨越格子上下边界。Fonts 保守聚合“清缓存以来曾出现溢出”，为真时沿用相邻行重画；首次局部绘制发现溢出立即补一次完整重画。清缓存、重载字体清除记录。该聚合是保守实现：溢出字形离开屏幕后仍可能多画邻行，直到缓存清理，不会少画。
- Fonts::glyph 热命中只查一次主哈希表；ASCII 单格无 emoji 使用字符×bold×italic 直接索引。索引是非拥有条目地址，主缓存的每个写入口先清空索引（包括缓存失败/淘汰/重分配），释放先销毁索引。返回字形借用绑定 &mut Fonts，期间无法修改缓存；表不会单独拥有字形。未命中仍需插入后取回条目。
- mix/over 将 RB 两个 16 位通道并行计算，G 单独计算；保留 alpha=0/255 与非预乘任意源输入的旧饱和语义。同一行相邻相同背景合并 fill_rect，选区/搜索等覆盖层顺序不变。

## 资源记账

新增 ASCII 地址 Vec 使用 Tracked、Kind::CacheIndex；512×sizeof(usize)，64 位目标 4096 B，先 reserve_capacity 申请再分配。被拒绝不清除已准入字形，退回哈希命中，直到清缓存前不反复申请失败。clear_caches 先 drop Vec 后 resize(0)。

CachedGlyph 与 Cluster 的溢出字段增加桶中 value 大小，原 Cache 的 map_model 使用 sizeof((K,(V,usize))) 自动覆盖；字形像素的 FontRaster/FontOutline 转移及 Glyph 模型不变。Renderer/ViewState 新状态均为内联字段，不新增堆占用。平移使用原 Canvas 的 copy_within，无像素副本；行指纹容量、Canvas 与 History 公式不变。

## 测试与失败场景

- translated_scroll_matches_full_for_rows_overlays_and_glyph_overflow：滚动 1 / rows−1 行、历史增长/已满、宽字符、选区、搜索、光标、shell 命令标记、状态栏及合成溢出字形；逐像素比较 invalidate 后完整重画，并验证 Damage 覆盖每个变化像素行。
- unsafe_scroll_cases_and_smaller_canvases_match_full_redraw：局部滚动区、历史视图滚动条、实际 Kitty 放置、graphics revision、缩小画布及 k>=rows 回退，逐像素比较。
- first_overflow_and_cache_reclaim_leave_no_stale_pixels：首次出现垂直溢出及缓存回收后不留下旧像素。
- scroll_identity_moves_with_each_view_and_reclaim_invalidates_all_views：两个窗格的顶行身份交换，共享缓存回收后两个视图均正确重画。
- parallel_color_channels_match_scalar_on_a_million_inputs：div255 全部 0..65025 输入及一百万组随机源/目标/alpha，与原 mix/over 精确相等。
- 字体地址索引测试覆盖单条缓存反复淘汰、重建、clear、样式槽与额度全归还；额度恰好仅足字形时地址表被拒绝，字形仍可用且命中不重复拒绝。
- 原 partial_redraw_matches_full_redraw、图片/字体/宽字符等测试保留。基准 frame_one_row 的固定三行 Damage 断言改为一至三行，匹配新溢出条件；输入、计时区间、预热及迭代数未变。

## 改动文件

| 文件 | 用途 |
| --- | --- |
| src/render.rs | 平移、视图失效、条件邻行重画、颜色通道计算、背景合并及像素差分测试。 |
| src/font/mod.rs | 字形溢出元数据、单哈希/ASCII 地址表、记账与失效测试。 |
| src/font/ttf.rs | 两处字体测试夹具适配缓存元数据。 |
| src/term/mod.rs | 暴露只读的完整滚动区判断供渲染回退使用。 |
| src/perf_bench.rs | 单行基准断言允许正确的一行 Damage，负载不变。 |
| docs/foundations.md | 同步地址表记账、失效与单行绘制口径。 |
| docs/evaluations/perf-t3/README.md | 本次审计材料与全部测量。 |

## 运行环境与复现

MacBook Air Mac16,12，Apple M4，16 GiB，macOS 27.0.1 (26A434)，rustc 1.99.0；接通 AC 电源。以下两条命令各顺序运行三次，release 模式，没有并行编译/回归；原始输出在 target/evaluations/t3-{default,count}-{1,2,3}.log，不入库。

```sh
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1
python3 ci/perf-compare.py run-1.log run-2.log run-3.log --baseline old-1.log old-2.log old-3.log
```

Mac arm64 实际运行软件绘制、分配与完整回归；**Linux/X11、Windows、macOS Intel 未在真机验证**，仅交叉 Clippy。软件绘制不代表原生窗口提交/合成耗时。

## 全部固定指标（三轮中位数）

Δ 为数值变化；MiB/s 越高越好，us/frame 越低越好。

| 指标 | 单位 | T2 默认 | T3 默认 | Δ | T2 alloc-count | T3 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | +0.00% | 20.000000 | 20.000000 | +0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | +0.00% | 32.000000 | 32.000000 | +0.00% |
| `history_bytes_10k` | bytes | 40082112.000000 | 40082112.000000 | +0.00% | 40082112.000000 | 40082112.000000 | +0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | +0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 638188.000000 | 638188.000000 | +0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 413.492343 | 379.999178 | -8.10% | 381.297176 | 440.592955 | +15.55% |
| `parse_ascii_noscroll` | MiB/s | 1191.676889 | 1196.474140 | +0.40% | 1204.267631 | 1200.679149 | -0.30% |
| `parse_ascii_2027` | MiB/s | 347.753951 | 374.425564 | +7.67% | 378.709429 | 367.107195 | -3.06% |
| `parse_sgr` | MiB/s | 269.494582 | 300.723440 | +11.59% | 301.550808 | 284.088598 | -5.79% |
| `parse_cjk` | MiB/s | 224.253318 | 232.073389 | +3.49% | 214.607116 | 204.759785 | -4.59% |
| `frame_full` | us/frame | 3579.918594 | 3523.941154 | -1.56% | 3476.717918 | 3507.962361 | +0.90% |
| `frame_one_row` | us/frame | 247.366558 | 121.837807 | -50.75% | 244.251994 | 120.390924 | -50.71% |
| `frame_scroll_1line_growing` | us/frame | 2063.750281 | 723.368707 | -64.95% | 2026.525286 | 721.799012 | -64.38% |
| `frame_scroll_1line_full` | us/frame | 2055.401372 | 725.772790 | -64.69% | 2027.152533 | 723.122443 | -64.33% |
| `frame_unchanged` | us/frame | 64.384104 | 63.075719 | -2.03% | 63.405139 | 63.048688 | -0.56% |

滚动增长/已满帧分别为整屏的 **20.53% / 20.60%**（默认），**20.58% / 20.61%**（alloc-count），低于 25%。单行耗时降低 **50.75% / 50.71%**，超过 40%。整屏默认降低 1.56%，alloc-count 增加 **0.90%**，严格数值上未达到“不退化”；不把微小差距改写为完全达标。帧稳态分配仍为零，History/结构尺寸/解析分配不变。

三轮波动 >=5%：默认 parse_ascii_scroll 7.79%、parse_sgr 18.07%、parse_cjk 9.28%；alloc-count parse_ascii_scroll 5.41%、parse_ascii_2027 11.32%、parse_sgr 16.63%、parse_cjk 15.52%。帧指标全部 <5%。比较工具由于解析波动及旧样本波动返回失败；没有删除样本或改变计时负载。Parser/Terminal 写入未改，不据此将解析变化归因于渲染优化；原因尚未独立定位。

T2 读缓冲分配辅助测试同命令各三轮复测，16 KiB/4095 B 两档计数组仍为 0 allocs/MiB，默认 skipped。原始输出 t3-read-*。

## 检查与限制

- fmt、四目标全目标 Clippy、额外本机 alloc-count Clippy：通过。
- Rust 1.88.0 check：通过，沿用 RUSTUP_HOME=/tmp/congminal-perf-rustup 的 +1.88 临时映射。
- 默认完整回归：931 passed / 56 ignored；辅助程序 21 passed。
- alloc-count 完整回归复跑：933 passed / 56 ignored；辅助程序 21 passed。首次运行两个 SSH 工作区用例等待外部工具超过两秒，日志完整保留 t3-tests-count.log，不将失败样本隐藏。

已知风险按严重程度：ASCII 地址表的安全性依赖主缓存写入前清表约束，未来新增写入口必须遵守；当前所有写入口和淘汰均已覆盖。跨平台仅编译，真实字体溢出与提交行为仍需各平台审计。解析组持续有稳定性缺口。alloc-count 整屏中位数微增 0.90%，如实保留。保守溢出聚合可能减少部分字体下的性能收益，不影响画面正确性。
