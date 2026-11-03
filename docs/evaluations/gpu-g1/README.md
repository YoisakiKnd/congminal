# G1：绘制记录与软件回放

基于 G0 `2985855`（[设计](../../design/gpu-rendering.md)）。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/render_list.rs | `DrawList`：填充、混合、区域平移、字形（灰度/彩色图集上传与清空）、图片纹理与最近邻采样，按顺序记录；未回放时继续累积，超过上限或回放端丢失内容时要求整屏重画。`SoftwareTarget`：同一语义的 CPU 回放器。 |
| src/render.rs | `Framebuffer::recording`、视图带原点和裁剪；`fill_rect`/`blend_rect`/`scroll_*`/字形混合在记录模式下改为追加操作。 |
| src/render_graphics.rs | 图片放置与占位符图块在记录模式下记成 `Image`；混合公式抽成 `blend_rgba` 供两边共用。 |
| src/font/mod.rs、src/image/mod.rs | 字形 `id`、图片 `serial`：进程内唯一、不复用，作为图集与纹理的键。 |
| src/workspace.rs | 整屏清空改用 `fill_rect`，记录模式下也生效。 |

默认路径仍是直接绘制，用户可见行为不变。

## 验证

- `render::list` 测试：直接绘制与“记录 → 软件回放”并排推进，每步逐像素相同。覆盖 1 倍和 2 倍缩放下的彩色文字、CJK、粗斜体、下划线/删除线、反色、组合附加符号、彩色 emoji、命令标记、选区、四种光标、整屏上滚（平移）、回看（下移 + 半透明滚动条）、直接放置与占位符图片、预编辑；断言以上每种操作都实际出现。另测视图原点与越界裁剪、面板、图集写满后清空、超大字形跳过、未回放累积和丢失后重画。
- 全部测试除 3 个已有的 SFTP 本地传输测试外通过；这 3 个在 G0 代码上同样失败（本机临时目录 `PermissionDenied`），与本阶段无关。
- clippy（Linux、Windows GNU、两种 macOS 目标）、rustfmt、1.88 检查通过。

## 性能（CPU 默认路径，不应退化）

与 G0 同机、同命令，三轮中位数；`frame_*` 单位 µs/帧（越小越好），`parse_ascii_scroll` 单位 MiB/s（越大越好）：

| 指标 | G0 | G1 | 变化 | 三轮极差 G0 / G1 |
| --- | ---: | ---: | ---: | --- |
| `frame_full` | 8855.7 | 8521.8 | -3.8% | 2.2% / 1.5% |
| `frame_one_row` | 361.8 | 347.2 | -4.0% | 2.9% / 0.6% |
| `frame_scroll_1line_growing` | 1609.5 | 1594.5 | -0.9% | 6.1% / 3.4% |
| `frame_scroll_1line_full` | 1589.7 | 1638.3 | +3.1% | 5.3% / 6.7% |
| `frame_scrollback_view` | 1762.1 | 1709.5 | -3.0% | 4.6% / 8.9% |
| `frame_unchanged` | 0.7 | 0.7 | -0.2% | 0.4% / 1.1% |
| `parse_ascii_scroll` | 187.7 | 192.5 | +2.6% | 4.4% / 2.0% |

`ci/perf-compare.py` 对滚动类指标报“三轮极差超过 5%”：这是本机噪声门槛（G0 自身滚动指标的极差也有 5–6%），中位数没有退化。
