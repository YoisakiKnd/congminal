# v0.2.1 Windows 图片自动探测修复

用户在 v0.2.0 中运行 NakuruMusic 仍得到半块字符图案。这是实际兼容缺陷；v0.2.0 的低层协议测试不等于真实库自动探测验收。

## 复现与原因

NakuruMusic 的顺序是 `Picker::from_query_stdio()`，然后才初始化 crossterm TUI。原生手写测试提前设置 `ENABLE_VIRTUAL_TERMINAL_INPUT`，实际库只关闭行输入、回显与控制字符处理，保留原有 VT 输入位。

[修复前的真实原库测试](https://github.com/YoisakiKnd/congminal/actions/runs/41051737996) 在 Windows 返回 `Halfblocks`，字体尺寸却正确为 8×16。微软 [InputStateMachineEngine::ActionEscDispatch](https://github.com/microsoft/terminal/blob/main/src/terminal/parser/InputStateMachineEngine.cpp) 在非 VT 输入时会将 ESC 序列解释为 Alt 按键，因此 Kitty APC 应答没有完整到达应用。

修复将终端应答和普通输入分开：仅现代 ConPTY 的 APC 应答将 ESC 编码为原生 Win32 Unicode 按键记录；其余字符、CSI/DA 应答、普通键盘和粘贴保持原样。Unix 应答仍直接写 PTY。不依赖 Nakuru 的专用配置或环境变量。

## 回归与资源范围

[修复后原生回归](https://github.com/YoisakiKnd/congminal/actions/runs/41053101155) 使用真实 `ratatui-image 11.0.6`，调用自动探测、不强制协议或预设输入模式，再按应用顺序进入 crossterm，发送真实库编码的 128×128 图片。Windows 直接启动及 PowerShell 子进程均通过 Kitty 选择与所有 65536 个 RGBA 字节核对。macOS/Linux 使用相同库与实际 PTY 验证。夹具不访问播放服务或用户账号，因此不声称测试过 Nakuru 登录/播放全流程。

源码：`tools/fixtures/kitty-interop`；测试：`graphics::native_tests::real_picker_without_forced_console_modes`。图像库与 crossterm 只存在于独立测试工程，不进入 congminal 生产依赖。

应答输入上限 4096 B，编码临时存储固定 16 KiB 栈缓冲，仍由已有异步输入队列按预算整笔准入；不能准入则报告错误，不截断应答。不增加线程、循环刷新或持久缓存，图像绘制热路径未改变。本机常规回归主程序 901 项、helper 21 项通过。旧版渲染和 ConPTY 成本结果保留 [v0.2.0 测量的明确范围](../v0.2.0/README.md)，未把它当作此次应答编码的计时结果。

升级必须完全退出旧 congminal 进程并重开，随后重启 NakuruMusic；它不会在已经回退的运行实例中自动重新探测。
