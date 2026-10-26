# SFTP 预览构建

这是 macOS Apple 芯片的独立预览包，包含 SFTP 和按需认证 helper。使用本机临时签名，未公证；普通 Alpha.6 包继续可用。完整原生 GUI、三平台和长测验收尚未完成，先用测试目录和测试文件体验。

本轮原生窗口的 SFTP 认证/传输尚未通过验收：窗口控制工具长时间阻塞，未证明前台焦点和有效连接点击。已有软件前端真实回环验证，但本包仍用于实验，不作为日常稳定版。

解压并打开 `congminal-sftp-preview.app`。普通终端和快捷键见 Alpha.6 说明。SFTP 默认不连接，需要在 `~/.config/congminal/congminal.toml`（或 `XDG_CONFIG_HOME`）中明确启用：

```toml
[sftp]
enabled = true
# 自动使用 app 包内 helper；CLI 构建则使用主程序同目录的 helper。
# helper = "/absolute/path/congminal-ssh-helper"
# 不写时沿用系统 SSH 配置；此选项只影响独立 SFTP。
# ssh-config = "/absolute/path/ssh_config"

[connections.server]
kind = "ssh"
name = "测试服务器"
host = "your-ssh-alias"
```

修改 `your-ssh-alias` 后，在左栏“连接”页打开该 SSH 会话，再切到“文件”页点击“连接 SFTP”。SFTP 使用独立连接，认证在独立面板完成；不是将文件数据或答案发送到终端。主机确认请查看完整身份/指纹；不要在配置中写密码或私钥正文。

文件页支持远程目录导航、刷新、上传/下载路径输入。传输前需要完整查看主机、源和目标；默认不覆盖，覆盖必须重新明确确认。任务列表有进度、取消、详情和手动重试；失败/取消可能保留部分文件，清除记录不会删除文件。提交不确定时先检查目标。退出会等待后台结束；回收异常或不确定结果需要核对警告并显式确认。

只有点击连接才创建原 16 MiB SFTP 子池、2 个工作器和 1 个回收线程；最多 64 个等待请求，超出内存额度时可能更早拒绝。不是整进程 RSS 上限。系统 ssh/helper 的临时进程内存另计；本预览未引入 Rust 第三方包。不要同时启动多个实例向同一目标反复覆盖。

helper 或 SSH 配置写错会停用入口/报错；不静默回落到另一份配置。已创建的 SFTP 池固定使用启动时捕获的 helper/SSH 配置，更改这两项请重新启动应用。Windows 安全 IPC/本地文件拥有者、Linux/Windows 真机和完整性能验收仍待完成。


源码进度与包快照：后续已实现本地上传源选择器，见[评估](../evaluations/m6-sftp-local-picker.md)。本说明对应此前生成的实验ZIP，尚不含该选择器；没有把软件回环作为原生窗口验收。
