# T5：窗格直接绘制到工作区总画布

基于 T4 HEAD `cfcb59b9d61eea5f70af0540e7a23cd96198281c`。不新增 crate。

## 实现与资源记账

Framebuffer 增加 stride，像素为 Owned(Vec) 或 Borrowed(&mut [u32])。根画布保留原 Canvas Lease；借用视图 memory=None、capacity=0，只借用矩形起点到最后一行末尾的跨度。拥有 Vec 的预算仍按真实容量×4，分配/复制完成前保留旧数据和票据；释放先销毁像素再归还额度。借用视图拒绝 resize，不分配、销毁或归还父画布的额度。

Frozen 删除 framebuffer，保留逻辑 viewport；Congminal 在工作区中使用 draw_into，鼠标/输入法/字体重载等通过 viewport 定位。非工作区 App::draw 保留单一拥有画布。工作区 relayout 全部网格计划准入后，仅调整根画布，然后释放旧单窗格画布、提交每个窗格尺寸；新增窗格的预留请求去掉 rect.w*rect.h*4，只保留 Queue/Grid。布局恢复、隐藏标签和重连都不创建窗格 Canvas；没有新增 Kind。viewport、stride、像素枚举/可选票据均为内联元数据，不计为额外堆容量。

平稳工作区的 Canvas 从 `root.capacity()*4 + Σpane.capacity()*4` 变为 `root.capacity()*4`，原生快照仍按其独立票据计费。根画布增长的新旧重叠预留保持；缩小时保留容量仍持续计费，没有将逻辑像素长度冒充保留容量。没有把异步平台快照/GPU/系统合成算成零。

## stride 与失效核对

直接修改行偏移的函数：Framebuffer::fill_rect、blend_rect、scroll_up，blend_glyph、blend_auth_glyph，以及 render_graphics 的 graphics_row、blit_cell。连续根画布沿用整块 copy_within；带间隙的矩形逐行移动 width 像素，stride 间隙不能被复制或清空。Glyph/图片源的行宽仍使用其自身宽度。

以下路径经以上函数使用 stride：draw_backgrounds、draw_glyphs、draw_command_mark、draw_ui_decorations、draw_bar、draw_cursor、draw_scrollbar、draw_preedit、draw_char、draw_decorations；render_chrome 的 panel/panel_row/panel_spark；graphics_cell 及认证文字/光标。宽高仍用于窗格内裁剪，不误改为 stride。原生 App::draw 返回最终拥有的根画布，局部借用视图不会进入平台提交路径。

工作区不再逐行复制 source.pixels。Damage 在窗格内产生，再按 rect.y 平移并裁剪到根画布；满帧先清背景再强制绘制全部可见窗格，不能依赖已删除的旧窗格像素。ViewState 和顶行身份仍交换；字体 generation 改变继续 invalidate。缩放计划期间旧网格可画在更小视图，glyph、图片及滚动均被视图宽高限制。

认证、传输准备/路径确认和退出覆盖层仍在窗格及 chrome 之后绘制；关闭时沿用 full/invalidate。重连交换 viewport，保留原像素位置的逻辑尺寸，不交换拥有像素。

## 测试

- direct_panes_match_owned_canvas_composition_across_layouts_and_partial_updates：横/纵分屏、2–4 个窗格，CJK/组合符/背景/搜索、单行修改、滚动，及新画布布局后的缩放；与“单独拥有窗格画布→按旧规则裁剪并复制→chrome”的独立测试参照逐像素一致。
- borrowed_stride_clips_scrolling_overflow_graphics_and_chrome_to_its_rectangle：带行间隙和四周像素哨兵，完整/更小画布、首次溢出字形、滚动 1 与 rows−1 行、选区/光标、Kitty 图片、chrome、认证文字；矩形内与连续拥有画布一致，矩形外不被写入。借用 resize 拒绝，Canvas 额度不增加。
- canvas_at_4k：3840×2160、四窗格，实际 Canvas 账本恰好等于一份根像素容量，driver 无像素；常规回归执行。
- 原 partial/full redraw、工作区字体/焦点/选区/搜索、旧网格分步缩放、认证及传输覆盖层、布局恢复准入失败、重连、资源总量等测试均保留。原失败状态尺寸断言改看 viewport。

早期差分夹具未裁剪旧合成器的复制边界，以及在 40 ms 布局合并前把目标窗口尺寸当成新画布尺寸，导致夹具失败；已恢复旧边界规则并先提交布局后比较。相关日志保留，不将中间结果当通过样本。

## 文件清单

| 文件 | 用途 |
| --- | --- |
| src/render.rs | 拥有/借用画布与 stride、裁剪平移、认证/字形偏移、哨兵差分。 |
| src/render_graphics.rs | Kitty 光栅行偏移使用 stride。 |
| src/render_chrome.rs | 测试像素快照改用切片 to_vec，绘制通过共用光栅路径。 |
| src/main.rs | 逻辑 viewport、共享画布 resize、公共绘制前状态与 draw_into。 |
| src/workspace.rs | 删除 Frozen 画布及逐行复制，调整预检、Damage、像素参照与 4K 测量。 |
| src/workspace/remote.rs | 重连保留 viewport。 |
| src/workspace/authentication.rs | 明确返回拥有画布的生命周期。 |
| src/platform/mod.rs | App 返回最终拥有画布的生命周期。 |
| src/platform/{protected_input,macos/protected_window}.rs | 原生受保护窗口测试实现适配同一类型。 |
| src/perf_bench.rs | Scene 拥有画布的生命周期，负载不变。 |
| docs/foundations.md | 同步 Canvas 与借用/预检口径。 |
| docs/evaluations/perf-t5/README.md | 审计材料、指标及风险。 |

## 运行与相关指标

MacBook Air Mac16,12 / Apple M4 / 16 GiB / macOS 27.0.1 (26A434) / rustc 1.99.0，AC Power、电池 100%。全部性能为 release，同机同命令顺序三次中位数。Mac arm64 实际运行软件绘制与原生 PTY；**Linux/X11、Windows、macOS Intel 未在真机验证**。4K 是实际分配的软件像素尺寸，未在外接 4K 显示器验证。

```sh
cargo test --release --bin congminal workspace::tests::bench_workspace_input_and_composition -- --exact --ignored --nocapture --test-threads=1
cargo test --release --bin congminal workspace::tests::canvas_at_4k -- --exact --nocapture --test-threads=1
# alloc-count 配置在 --bin congminal 后增加 --features alloc-count；两种配置各三轮。
cargo test --release --bin congminal perf_bench -- --ignored --nocapture --test-threads=1
cargo test --release --bin congminal --features alloc-count perf_bench -- --ignored --nocapture --test-threads=1
```

旧工作区基准在 cfcb59b 产品代码上、T5 修改前运行；旧 4K 基准在同 HEAD 的 git archive 中仅加入同名容量夹具（目标尺寸/分屏步骤相同），不改产品逻辑。原始源码及输出位于 target/evaluations/t5-4k-before/、t5-before-*；新输出 t5-{4k,workspace,default,count}-*。没有并行编译/回归污染正式计时。

| 工作区指标 | 单位 | T4 默认 | T5 默认 | T4 alloc-count | T5 alloc-count |
| --- | --- | ---: | ---: | ---: | ---: |
| 输入 P50 | ns | 84 | 84 | 83 | 84 |
| 输入 P95 | ns | 250 | 250 | 208 | 250 |
| 输入 P99 | ns | 1750 | 1709 | 1291 | 1959 |
| 1600×900 四窗格 full | us/frame | 541 | 512 | 545 | 509 |
| 1600×900 四窗格 partial | us/frame | 20 | 21 | 21 | 21 |
| 1600×900 Canvas | B | 11224704 | 5760000 | 11224704 | 5760000 |
| 3840×2160 Canvas | B | 65645504 | 33177600 | 65645504 | 33177600 |

4K 减少 **32,467,904 B（30.96 MiB，49.46%）**。1600×900 减少 5,464,704 B；完整组合平均耗时降低 **5.36% / 6.61%**，局部组合默认 20→21 us、计数组持平。原基准输出整数 us，保留此量化口径。输入尾延迟计数组 P99 增加 668 ns（51.74%），不宣称输入延迟整体改善。

## 全部固定指标（三轮中位数）

| 指标 | 单位 | T4 默认 | T5 默认 | Δ | T4 alloc-count | T5 alloc-count | Δ |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `size_of::<Cell>()` | bytes | 20.000000 | 20.000000 | +0.00% | 20.000000 | 20.000000 | +0.00% |
| `size_of::<Row>()` | bytes | 32.000000 | 32.000000 | +0.00% | 32.000000 | 32.000000 | +0.00% |
| `history_bytes_10k` | bytes | 1533274.000000 | 1533274.000000 | +0.00% | 1533274.000000 | 1533274.000000 | +0.00% |
| `allocs_per_mib_parsed` | allocs/MiB | skipped | skipped | — | 156.437500 | 156.437500 | +0.00% |
| `alloc_bytes_per_mib_parsed` | bytes/MiB | skipped | skipped | — | 34297.500000 | 34297.500000 | +0.00% |
| `allocs_per_frame` | allocs/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `alloc_bytes_per_frame` | bytes/frame | skipped | skipped | — | 0.000000 | 0.000000 | 0.00% |
| `parse_ascii_scroll` | MiB/s | 224.763559 | 230.940771 | +2.75% | 222.746023 | 231.248230 | +3.82% |
| `parse_ascii_noscroll` | MiB/s | 1185.312299 | 1196.342743 | +0.93% | 1204.973324 | 1180.620295 | -2.02% |
| `parse_ascii_2027` | MiB/s | 215.076650 | 217.826736 | +1.28% | 219.027565 | 220.323969 | +0.59% |
| `parse_sgr` | MiB/s | 184.301319 | 185.325390 | +0.56% | 186.069320 | 183.917581 | -1.16% |
| `parse_cjk` | MiB/s | 122.304250 | 123.201398 | +0.73% | 123.258621 | 122.687851 | -0.46% |
| `frame_full` | us/frame | 3563.950516 | 3634.346188 | +1.98% | 3556.427564 | 3567.564280 | +0.31% |
| `frame_one_row` | us/frame | 125.825905 | 125.498419 | -0.26% | 128.900094 | 124.159399 | -3.68% |
| `frame_scroll_1line_growing` | us/frame | 727.466744 | 777.300661 | +6.85% | 730.664143 | 786.296319 | +7.61% |
| `frame_scroll_1line_full` | us/frame | 726.833222 | 791.456833 | +8.89% | 730.424171 | 784.297043 | +7.38% |
| `frame_unchanged` | us/frame | 65.237719 | 64.863276 | -0.57% | 68.034664 | 64.467228 | -5.24% |
| `frame_scrollback_view` | us/frame | 2214.635143 | 2216.561528 | +0.09% | 2234.891878 | 2233.739652 | -0.05% |

default 波动 >=5%：parse_ascii_2027 5.25%。

count 波动 >=5%：无。

## 检查与风险

- fmt、四目标 Clippy 和额外本机 alloc-count Clippy：通过。
- Rust 1.88.0 check：通过，沿用临时 RUSTUP_HOME 的 +1.88 映射。
- 默认完整回归：938 passed / 56 ignored；alloc-count：940 passed / 56 ignored；辅助程序均 21 passed。

状态：T5 的共享 Canvas、stride 裁剪及 2–4 窗格一致性目标完成。没有借口忽略相关性能差距：固定基准的滚动帧默认增加 6.85% / 8.89%，计数组增加 7.61% / 7.38%；原因尚未独立采样定位，不能解释为已经证实的纯布局噪声。工作区完整组合改善约 5–7%，局部组合没有相同收益。

工作区输入/组合测量的三轮波动 >=5% 项（前后均保留）：
- default before-workspace：p95 16.80%；p99 30.91%；partial_us 5.00%。
- default workspace：p95 16.40%；p99 9.83%。
- count before-workspace：p95 19.71%；p99 16.11%。
- count workspace：p95 16.40%；p99 29.76%。

4K 两种配置前后各三次的容量相同，波动零。固定基准默认 parse_ascii_2027 波动 5.25%，比较器因此失败；计数组比较器通过。普通输入纳秒级尾部样本和整数微秒组合样本存在调度/计时量化影响，未通过增加/挑选运行凑出稳定结果。

风险按严重程度：滚动帧增加约 7–9%；输入计数组尾延迟及上述波动；根画布仍可能保留缩放前的较大容量并持续计费；T4 的滚动解析吞吐缺口仍在，本任务没有恢复到 T3 水平；Linux/X11、Windows、macOS Intel 未在真机验证。
