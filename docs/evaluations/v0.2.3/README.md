# v0.2.3 验收

正式标签对应 `44d793db1eaeac8a5b5f70bad114a39b4c54393f`，[发布 CI](https://github.com/YoisakiKnd/congminal/actions/runs/41074466284) 的三平台、最低 Rust 版本和正式发布检查全部通过。实际下载包已重新核对来源提交、SHA256、架构、图标、许可及 ConPTY 配对，见 [安装包核验](published-assets.json)。

包管理清单均同步到 v0.2.3：[Scoop 安装验证](https://github.com/YoisakiKnd/scoop-bucket/actions/runs/41076383762) 通过，包括从实际安装目录启动的原生窗口/目录检查；[Homebrew Apple 芯片与 Intel 安装验证](https://github.com/YoisakiKnd/homebrew-tap/actions/runs/41076368482) 通过，检查架构、版本、图标、许可、签名和卸载，不绕过首次运行授权。

三平台本地会话默认采用主目录，显式目录优先；窗口统一保持 `congminal`，程序标题保留在标签中。主目录在配置中确定，不改变 congminal 自身工作目录，不影响相对配置路径或后台工具的原启动环境。

Windows 的原生 GUI 夹具从安装目录启动，检查主目录、含空格/中文的显式目录、`.` 继承目录及 shell 覆盖窗口标题后的稳定性。macOS/Linux 的真实会话回归核对相同三种工作目录，比较实际 shell 的 `pwd -P` 与目标目录。多窗格回归保留动态标签标题，同时固定原生窗口标题。

Windows 三种目录与固定标题的实际结果见 [原生窗口记录](windows-startup.json)。文档清理的计数、字节数和归档核对范围见 [清理记录](documentation-cleanup.json)。

本机完整回归为主程序 903 项、helper 21 项通过；当前 Markdown 本地文件链接检查通过。发布源码含 15 个文档文件、约 209 KiB，随后在此目录保留正式发布与安装证据；不宣称旧 Git 历史对象已从仓库移除。

文档清理前，1,165 个移出文件逐一通过 Git 内容哈希与 `v0.2.2` 标签核对。原始内容固定于 `c2aafdc6464b8d42dd2174d23cddcdf473d6a46b`，索引与恢复方法见 [归档](../../archive/README.md)。配置参考保留在原路径供配置一致性测试和开发工具使用，当前本地 Markdown 链接另行检查。

资源边界：Unix 主目录候选最多 4096 字节；Windows 环境查询最多 4096 个 UTF-16 单元，候选路径编码长度另限 4096 字节。路径仅在构建配置时处理，不新增线程或循环刷新；移除了窗口标题待发送副本。资源账本不是 RSS 硬上限。

历史测量仍保留其原始版本和机器范围。Windows 新 ConPTY 成本、Kitty 暖绘制和 v0.2.1 自动探测修复记录见 [历史评估](https://github.com/YoisakiKnd/congminal/tree/v0.2.2/docs/evaluations)，没有将旧数字重新标为 v0.2.3 测量。
