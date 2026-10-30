# 历史文档归档

阶段执行计划、DeepSeek 交接、旧评估 JSON、日志、截图与源码快照已移出当前工作树。完整内容固定保存在 **`v0.2.2`**，提交 `c2aafdc6464b8d42dd2174d23cddcdf473d6a46b`；未重写标签或旧发布包。

- [浏览完整历史目录](https://github.com/YoisakiKnd/congminal/tree/v0.2.2/docs)
- [旧路线图](https://github.com/YoisakiKnd/congminal/blob/v0.2.2/docs/roadmap.md)
- [旧交接文档](https://github.com/YoisakiKnd/congminal/blob/v0.2.2/docs/DEEPSEEK_HANDOFF.md)
- [历史性能与发布证据](https://github.com/YoisakiKnd/congminal/tree/v0.2.2/docs/evaluations)
- [文件清单与 Git 内容哈希](manifest.json)

清理前，移出的 1,165 个文件逐一与该标签的 Git 内容哈希核对一致，约 226 MiB 历史资料均可恢复。完整 Git 历史仍保留；清理减少的是工作树文件，不宣称重新克隆时省去历史对象。

需要离线查看时，在仓库中执行以下命令，将历史文档恢复到独立目录：

```sh
mkdir -p /tmp/congminal-docs-v0.2.2
git archive v0.2.2 docs | tar -x -C /tmp/congminal-docs-v0.2.2
```

浅克隆若没有该标签，先执行 `git fetch origin tag v0.2.2`。复现旧报告应使用报告记录的源码提交、工具版本和环境；不要改旧哈希来验证新代码。当前说明见 [文档导航](../README.md)。
