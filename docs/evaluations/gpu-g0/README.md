# G0：CPU 渲染基线

基于 `c363fe5`（[设计](../../design/gpu-rendering.md)）。G1 之后的阶段以此对照。

## 环境

Linux x86_64，AMD EPYC 7B13（4 vCPU），rustc 1.99.0，release 构建。
`cargo test --release --bin congminal perf_bench::benchmarks -- --ignored --nocapture --test-threads=1`，连续三轮。

场景：`perf_bench.rs` 的 200×60 字符、2 倍缩放、默认字体，画布 3616×2116 像素（接近 4K），单位 µs/帧。

| 指标 | 中位数 | 三轮 |
| --- | ---: | --- |
| `frame_full` | 8855.7 | 8669.7 / 8868.8 / 8855.7 |
| `frame_one_row` | 361.8 | 367.9 / 361.8 / 357.3 |
| `frame_scroll_1line_growing` | 1609.5 | 1636.0 / 1537.6 / 1609.5 |
| `frame_scroll_1line_full` | 1589.7 | 1563.4 / 1648.3 / 1589.7 |
| `frame_scrollback_view` | 1762.1 | 1811.4 / 1730.6 / 1762.1 |
| `frame_unchanged` | 0.7 | 0.8 / 0.7 / 0.7 |

## 结论

- 整屏重画约 8.9 ms/帧，高于 120 Hz 的 8.3 ms 帧预算；耗时与像素数成正比，主要是填充和逐像素混合。
- 滚动一行约 1.6 ms，主要是在 CPU 上平移整个画布的像素。
- 没变化的帧不到 1 µs：行指纹判断本身很便宜，GPU 后端继续沿用。

GPU 阶段要在同一场景下报告这些指标的“只记录”开销（CPU 端生成实例的时间）和 GPU 回放时间。
