# 依赖清单与合计成本（M8：新依赖记录）

本文件把 `Cargo.toml` 里的每一项依赖、它启用的特性、传递依赖、运行时成本与验证状态汇总到一处，供审查「新依赖的确切版本、特性、传递依赖、合计性能成本、未验证平台及采用/自写结论」这一条。单项的完整评估仍以 `docs/evaluations/` 下的记录为准，这里只做汇总与索引。

## 1. 直接依赖（运行时）

| 依赖 | 版本 | 启用特性 | 目标平台 | 用途与接入点 | 运行时成本 | 评估记录 |
| --- | --- | --- | --- | --- | --- | --- |
| `sysinfo` | `=0.38.4` | `default-features = false`，`system`、`disk` | linux / macos / windows | 本地状态监控的薄适配层（`src/monitor`）：CPU、内存、磁盘、进程表 | 无自有线程（未启用 `multithread`）；进程表刷新会为每个进程保留一个 `/proc/<pid>/stat` 句柄，已用 `sysinfo::set_open_files_limit(16)` 封顶（`FILE_HANDLES`）；采样成本见下 | [监控面板](../design-monitor-panel.md)、[M7 监控评估](m7-monitor.md) |
| `libc` | `=0.2.191` | 默认（`std`） | unix | 平台层 FFI（X11、PTY、`termios`、`ioctl`、`poll`） | 纯声明，无运行时成本 | [M3 执行记录](../execution-m3.md) |
| `libssh2-sys` | `=0.3.3` | `default-features = false`，可选特性 `sftp-libssh2` | unix（默认不启用） | SFTP 的库后端（`src/sftp/native.rs`），与 OpenSSH 子系统后端并列 | 仅在启用 `sftp-libssh2` 时链接；默认构建不引入 | [M6 记录](../execution-m6.md)、`docs/evaluations/m6-sftp*.json` |

## 2. 传递依赖（运行时 + 构建）

`cargo tree --locked --edges normal,build --all-features` 的全部节点（12 个，含本包）：

| 依赖 | 版本 | 类型 | 引入者 | 说明 |
| --- | --- | --- | --- | --- |
| `libc` | 0.2.191 | 运行时 | 直接 + `sysinfo` + `libssh2-sys` + `libz-sys` + `openssl-sys` | 同一版本，只编译一份 |
| `memchr` | 2.8.3 | 运行时 | `sysinfo` | SIMD 字节搜索，无额外线程 |
| `libssh2-sys` | 0.3.3 | 运行时（可选） | 直接 | 只在 `sftp-libssh2` 下 |
| `libz-sys` | 1.1.30 | 运行时（可选） | `libssh2-sys` | 链接系统 zlib 或自带源码 |
| `openssl-sys` | 0.9.117 | 运行时（可选） | `libssh2-sys` | 链接系统 OpenSSL |
| `cc` | 1.9.0 | 构建 | `libz-sys`、`openssl-sys`、`libssh2-sys` | 只在构建期，不进二进制 |
| `find-msvc-tools` | 0.1.15 | 构建 | `cc` | 同上 |
| `shlex` | 2.0.1 | 构建 | `cc` | 同上 |
| `pkg-config` | 0.3.34 | 构建 | `libz-sys`、`openssl-sys`、`libssh2-sys` | 同上 |
| `vcpkg` | 0.2.15 | 构建 | `libz-sys`、`openssl-sys` | 同上（Windows 找库） |

默认特性（`default = []`）下实际参与编译的只有：`libc`、`sysinfo`、`memchr`。其余全部由可选特性 `sftp-libssh2` 带入。

## 3. 特性与构建组合

| 特性 | 内容 | 用途 |
| --- | --- | --- |
| `default` | 空 | 默认构建不带任何可选依赖 |
| `directory-helper` | 无依赖 | 本地目录读取走隔离子进程（`src/local_directory/isolated.rs`） |
| `ssh-auth-helper` | 无依赖 | 认证交互走隔离子进程（`src/ssh`） |
| `sftp-preview` | `ssh-auth-helper` | SFTP 面板的预览构建 |
| `sftp-libssh2` | `dep:libssh2-sys`、`dep:libc` | 启用 SFTP 库后端 |

CI 的 `msrv` 作业用 1.88 检查四个目标；`linux-x86_64`、`macos-universal`、`windows-x86_64` 三个作业跑默认特性 + `--all-targets` 的测试与 clippy。

## 4. 合计成本（本机实测，Linux x86_64）

| 项 | 数值 | 来源 |
| --- | --- | --- |
| 监控采样（不含磁盘、不含进程表） | 中位 155 µs / 轮 | `cargo test monitor_sample_cost -- --ignored --nocapture` |
| 监控采样（含磁盘） | 中位 734 µs / 轮 | 同上 |
| 进程表刷新（5 行） | 中位约 24 ms / 5 秒一次 | 同上 |
| 监控采样占用的文件句柄 | ≤ 16（`set_open_files_limit`） | `monitor::monitor_tests::process_list_keeps_the_file_handle_count_bounded` |
| 30 分钟长测 RSS 增量 | +19 028 KiB（阈值 40 960） | [M8 长测](m8-soak.md) |
| 30 分钟长测线程 / fd 增量 | +1 / +1 | 同上 |
| 静默期 CPU | 0.35% | 同上 |

## 5. 未验证平台与场景

- `sysinfo`：只在 Linux 真机跑过；Windows/macOS 只有交叉编译与 clippy，**未实机采样**。进程表在 macOS 上的句柄行为未测。
- `libssh2-sys`：`sftp-libssh2` 后端有单元测试与成本记录，但没有与 OpenSSH 后端做同负载吞吐对照的完整矩阵（见 [M6 记录](../execution-m6.md)）。
- 构建依赖（`cc`/`pkg-config`/`vcpkg`）在 Windows 与 macOS 上的实际构建未验证。

## 6. 结论

- 三项直接依赖都保持薄适配、固定版本（`=` 精确版本）、默认关闭可选后端；没有引入异步运行时、没有新增常驻线程。
- 传递依赖合计 11 个（含构建依赖），默认构建只编译 3 个。
- 待审查：`sysinfo` 的进程表成本（约 24 ms / 5 秒）是否可接受、是否需要按平台调整；`libssh2-sys` 后端是否保留（当前默认不启用）。
