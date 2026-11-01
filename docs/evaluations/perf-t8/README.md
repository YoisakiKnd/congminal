# T8：X11 共享内存与分段提交

基于 P2 `6c6238a`。

## 改动

| 文件 | 内容 |
| --- | --- |
| src/platform/x11/shm.rs | 运行时加载 libXext 的 MIT-SHM 提交：共享段计入 Canvas，附加时临时捕获 X 错误，附加后立即 IPC_RMID，写入前同步上一帧，任一步失败退回 XPutImage。 |
| src/platform/x11.rs | 优先走共享内存；XPutImage 也改为按段提交；关闭连接前断开共享段。 |
| src/platform/win32.rs | `SetDIBitsToDevice` 按段提交。 |
| ci/check-x11-shm.sh | Xvfb 下两种方式截图逐字节比较，检查共享段残留。 |

不新增编译期依赖：libXext 通过 dlopen 加载，缺失时自动退回。

## 验证（Linux x86_64，Xvfb）

- `ci/check-x11-shm.sh`：启用 MIT-SHM；与 XPutImage 截图逐字节相同（截图非空白，采样 770 种颜色）；运行中共享段附加数为 2、状态为待删除，退出后为 0。
- `ci/check-x11-close.sh` 通过；`ci/soak-x11.sh 90` 全部阈值通过（静默期 CPU 0.20%，fd 不变）。
- 1920×1080、200×60、`seq 1 4000000`，三轮：

| | Xvfb CPU s | 终端 CPU s | 总耗时 s |
| --- | ---: | ---: | ---: |
| XPutImage | 1.05 / 1.10 / 1.04 | 4.04 / 4.22 / 4.00 | 5.53 / 5.79 / 5.39 |
| MIT-SHM | 0.33 / 0.37 / 0.35 | 4.00 / 4.07 / 4.06 | 5.25 / 5.33 / 5.27 |

Windows 分段提交只做了交叉编译检查，**未在真机验证**。真实 X 服务器（Xorg、XWayland）未测。
