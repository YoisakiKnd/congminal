# congminal 角色终端图标

![图标预览](congminal-character-v1.png)

- 主文件：`congminal-character-v1.png`，1254 × 1254 RGBA，外部背景透明。
- 用途：终端应用图标设计稿。主体为 `>_`，角色元素为薄荷绿猫耳与双马尾、粉色蝴蝶结/发带和金色星形发饰。
- 参考：用户在本次对话提供的角色立绘；原始立绘未复制进仓库。
- 生成方式：内置 `image_gen`，保留生成输出的透明度；不是 CLI/API 回退。
- 完整提示词：`congminal-character-v1.prompt.txt`。

集成格式：macOS `congminal.icns`（应用包 CFBundleIconFile）、Windows `congminal.ico`（SDK rc.exe 编译到主程序资源及窗口图标）、Linux 64px 窗口图标与 256px XDG 桌面图标。派生 PNG/ICO/ICNS 仅做原生尺寸/格式转换，保留透明度；运行时无新增图片库依赖。

初始设计稿为 v1；具体发布版本和三平台验证结果见对应发布记录。
