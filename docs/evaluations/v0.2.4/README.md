# v0.2.4 验收

正式标签对应 `d1b4efa5cb59e029f7b98f395298e98da0e9521f`。[发布 CI](https://github.com/YoisakiKnd/congminal/actions/runs/41182962411) 的 Windows、macOS 通用包、Linux、最低 Rust 版本与正式发布任务全部成功。三个正式包实际下载后重新核对 SHA256、来源提交、架构、图标和许可证，见 [安装包核验](published-assets.json)；Windows 两个 ConPTY 文件逐一核对固定版本的大小和哈希。macOS 下载包额外通过本机签名完整性、双架构及 `--version` 检查。

个人包仓库均同步到 v0.2.4：[Homebrew Apple 芯片与 Intel 安装检查](https://github.com/YoisakiKnd/homebrew-tap/actions/runs/41183491641) 与 [Scoop 安装检查](https://github.com/YoisakiKnd/scoop-bucket/actions/runs/41183497092) 全部成功。检查包含实际安装、版本/许可/架构或 shim 核验、卸载；Scoop 额外从安装目录启动，核对默认主目录、显式目录、继承目录和固定应用标题。未绕过 Gatekeeper。

原生单元测试结果：macOS 主程序 908 项、Linux 897 项、Windows 663 项，helper 各 21 项通过。三端真实 PTY 图片传输和真实 ratatui-image 自动探测回归通过；Linux/Windows 的原生 OpenSSH/SFTP 验证、三端冒烟窗口检查通过。Rust 1.88 四个目标的全目标编译检查通过。测试数量因平台条件编译而不同，忽略的手动性能入口不计入通过数。

侧栏、Git 三列工具栏、通知点击、空连接示例、窄窗口保留终端空间以及趋势图越界/缺失样本行为均有回归覆盖。macOS 本机另外检查了深浅主题、真实 Git 仓库、SSH 空页及监控画面。箭头指针三端使用原生系统资源；本次 CI 没有自动识别鼠标指针形状，不把编译与冒烟检查表述为三端指针视觉验收。

本机 30×24 格侧栏绘制测量见 [资源边界](../../foundations.md)。现有 Neovim 0.12.2 配置（69 个插件，采样时加载 61 个）在实际 Session/PTY 和 120×34、2 倍缩放软件绘制链路下打开万行 Lua 文件，4 次就绪后画面更新为 0.83～0.89 秒，绘制 P95 为 1.39 ms。每轮 120 次滚动与 120 个定时输入字符均完成，退出后跟踪额度归零；只在临时副本中附加本机测量夹具，未添加生产依赖或修改用户配置。该结果不含 AppKit 图层提交和系统合成，不代表端到端按键延迟、冷启动、其他平台性能或完整 RSS 上限。

本次发行只增加有效版本说明与这份验收，历史证据仍保留原版本，不把旧数据重新标为新版本测量。
