# P2：去掉字形地址表的 unsafe、回看历史平移

基于 P1 `4aed5c6`。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/font/mod.rs | ASCII 单格字形改为由固定数组直接持有，删除指向 HashMap 条目的裸地址和 unsafe；额度按主缓存模型转入 `ascii_glyphs`，拒绝时退回主缓存。测试改为验证主缓存淘汰不影响 ASCII、额度不足时退回且不泄漏。 |
| src/render.rs | 回看历史时也按顶行差值上下平移；滚动条滑块单独跟踪；去掉为刷新滚动条而把历史长度放进行指纹的做法；`Framebuffer::scroll_down`；首次出现越格字形时写一条 `CONGMINAL_TRACE` 诊断。新增回看平移与整屏重画逐像素一致的测试，并断言确实走了平移。 |
| src/term/grid/history.rs | 字节上限这条策略检查改为 `debug_assert!`；保护记账和编码完整性的断言保留。 |

## 结果

Linux x86_64 release，新旧二进制交替三轮：

| 指标 | P1 | P2 |
| --- | ---: | ---: |
| frame_scrollback_view µs | 4933 / 5070 / 5171 | 1832 / 1886 / 1934 |
| frame_full、one_row、scroll_* | — | 噪声范围内不变 |

去掉地址表后的 ASCII 数组与原实现五轮交替中位数：frame_full 9050 vs 9179 µs，one_row 370 vs 363 µs，在噪声内一致。

## 暂缓

“字形越格”仍是整个字体共用的开关。按行记录每帧最多省 0.1–0.2 ms，改错会留下旧像素，且本机没有 Maple Mono 可验证。先通过 trace 确认用户字体是否会触发，再决定是否实施。
