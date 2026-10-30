# v0.1.1 许可与安装仓库验收

用户选择 GNU GPL v3；项目明确采用 **GPL-3.0-only**，不额外添加“或更高版本”。根目录 LICENSE 与 GNU 官方 <https://www.gnu.org/licenses/gpl-3.0.txt> 下载文本一致；NOTICE 记录原创代码版权、v0.1.0 的许可声明与第三方例外。Cargo 元数据声明同一 SPDX 标识；字体保留 SIL OFL 1.1，Unix libc 按 MIT 随包附全文。

## 发布与附件

[正式 Release v0.1.1](https://github.com/YoisakiKnd/congminal/releases/tag/v0.1.1) 对应实现/打包提交 `4ea4119ef3db88256de735b6128da6d30437467e`，源码与构建脚本见同名标签及 GitHub 源码归档。v0.1.0 标签、安装包和原始 SHA256 未改写。

[main CI 41021710051](https://github.com/YoisakiKnd/congminal/actions/runs/41021710051) 和 [稳定标签 CI 41022263296](https://github.com/YoisakiKnd/congminal/actions/runs/41022263296) 全部通过：三平台原生单元/Clippy、四目标 MSRV、实际 Windows/Linux SSH/SFTP、原生应用启动退出及默认包。

三个发布包均已下载，核对 SHA256、来源提交、主程序/helper 架构、版本和必要文件。根目录 LICENSE、NOTICE、LICENSE-Terminus.txt、LICENSE-libc.txt 与源码逐字节一致；macOS 应用包的 Resources 内也保留同一组全文，移动 .app 后仍存在。原始结果见 [published-assets.json](published-assets.json)。本次仅许可、版本元数据、发布说明选择和打包变化，未新增运行时依赖、线程、池或网络任务，不重复宣称全部平台性能测量。

## 安装仓库

- [Homebrew cask](https://github.com/YoisakiKnd/homebrew-tap/blob/main/Casks/congminal.rb)：v0.1.1 通用包，使用实际发布哈希；保留版本根目录中的 .app，命令入口直接指向 app 内原生二进制，helper 与许可保留在应用内。
- [Homebrew 原生验收 41022919165](https://github.com/YoisakiKnd/homebrew-tap/actions/runs/41022919165)：Apple Silicon / Intel 均通过实际安装、bundle 版本、CLI 符号链接、两程序架构、签名、许可和卸载。此 CI 不绕过 Gatekeeper、不宣称未经系统首次允许的 CLI 执行。此前本机临时 tap 已确认正确路径能够安装，但首次 CLI 受系统安全验证阻塞，终止后卸载清理了本次测试应用和链接。
- [Scoop manifest](https://github.com/YoisakiKnd/scoop-bucket/blob/main/bucket/congminal.json)：v0.1.1 Windows x86_64，GPL-3.0-only，版本目录解压、主程序 shim、开始菜单快捷方式及稳定版 checkver/autoupdate，校验和从该包的 .sha256 文件取得。
- [Scoop 原生验收 41023322215](https://github.com/YoisakiKnd/scoop-bucket/actions/runs/41023322215)：完整 schema、真实安装、主程序/helper、所有许可、直接执行与 shim 的实际版本、发布校验和、更新目录展开及卸载均通过。初次 [41022951307](https://github.com/YoisakiKnd/scoop-bucket/actions/runs/41022951307) 安装已成功，但 PowerShell 的表达式捕获提前关闭 GUI 子系统程序 stdout 管道；改为 Start-Process -Wait 并重定向文件后复测通过，没有放宽版本断言。

两仓库只添加安装元数据、文档和验证 CI。livecheck/checkver/autoupdate 提供维护者更新配置，不等同于已新增自动定时同步。安装方式已加入项目 README。
