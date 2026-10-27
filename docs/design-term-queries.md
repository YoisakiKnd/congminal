# 终端兼容：应用小键盘、尺寸与颜色查询、DECRQSS 与应答上限

这几批把终端内核里长期缺口接上，都属于"程序问、终端答"的兼容性工作，没有新增
依赖、线程或子进程。范围只到终端内核与按键编码，界面不变。

## 1. 应用小键盘（DECKPAM / DECNKM）

| 位置 | 改动 |
|---|---|
| `src/vt.rs` | 已有 `ESC =` / `ESC >` 解析，写入 `Modes::app_keypad`（本批前已存在，但没人读） |
| `src/input.rs` | 新增 `Keypad` 枚举与 `Key::Keypad`；`encode` 的第三参数从 `bool` 换成 `Flags { app_cursor, app_keypad }` |
| `src/main.rs` | 两处 `input::encode` 调用改用新增的 `App::key_flags()`，从当前窗格 `term.modes()` 取两个模式 |
| `src/platform/x11.rs` | keysym `0xffb0..=0xffb9` → `Digit(0..9)`，`0xff8d` → `Keypad::Enter`，`0xffaa..0xffbd` → 运算符 |
| `src/platform/win32.rs` | `VK_NUMPAD0..9`、`VK_MULTIPLY/ADD/SEPARATOR/SUBTRACT/DECIMAL/DIVIDE`、`VK_OEM_NEC_EQUAL`；用 `lparam` 的扩展位区分小键盘回车与主回车 |
| `src/platform/macos.rs` | keycode 65/67/69/75/78/81/76 与 82..=92（`Digit(0..9)`）；macOS 小键盘没有分隔符键 |

编码规则：

- 应用键区模式且无修饰键时发 SS3 单字节：`Digit(0..9)` → `ESC O p..y`，
  `Multiply` → `j`、`Add` → `k`、`Separator` → `l`、`Subtract` → `m`、
  `Decimal` → `n`、`Divide` → `o`、`Equal` → `X`、`Enter` → `M`。
- 其余情况（普通模式、带修饰键）退化成主键盘上的字符，Alt 前缀照旧。
- 小键盘按键不影响主键盘数字：`Key::Char('1')` 与 `Key::Keypad(Digit(1))` 是两条独立路径。

文本框（搜索框、SFTP 名称输入）用 `Key::text()` 取值，小键盘数字与运算符在那里按普通
字符处理，`Keypad::Enter` 不算文字。

## 2. 窗口与字符尺寸查询（`CSI t`）

`src/term/mod.rs` 的 `respond()` 新增分支，应答都走既有的 `response` 缓冲：

| 请求 | 应答 | 含义 |
|---|---|---|
| `CSI 11 t` | `CSI 1 t` | 窗口已图标化 |
| `CSI 14 t` | `CSI 4 ; height ; width t` | 文本区像素尺寸（`cols × cell_w`、`rows × cell_h`） |
| `CSI 16 t` | `CSI 6 ; cell_h ; cell_w t` | 单个字符格的像素尺寸 |
| `CSI 18 t` | `CSI 8 ; rows ; cols t` | 文本区字符尺寸 |
| 其他参数 | 无应答，写 trace | 未实现的报告不假装支持 |

`Terminal` 不知道像素，新增 `cell_pixels: (u32, u32)` 与 `set_cell_pixels()`：

- `main.rs` 的 `resize_prepared` 用 `Renderer::cell_size()` 写入主窗口；
- `workspace.rs` 的重排循环在每个窗格 `load()` 之后写入该窗格自己的格尺寸，
  所以多分屏时每个会话答的是自己的尺寸。

像素未知（尚未布局）时按 0 应答，程序拿到 0 会自己退回字符尺寸。`RIS` 复位像
`default_cursor_shape` 一样保留 `cell_pixels`，因为那是窗口属性不是终端状态。

## 3. 颜色查询（`OSC 4` / `10` / `11` / `12`）

程序问调色板颜色时，只有按配置回答才不会让应用自己猜（猜错的后果是用反色、真彩
渲染错）。实现是"值拷贝 + 只读"：

| 位置 | 改动 |
|---|---|
| `src/term/mod.rs` | 新增 `Palette { foreground, background, cursor, palette: [u32; 256] }`、`Terminal::set_palette()`；`osc_dispatch` 增加 `4`、`10`/`11`/`12` 分支 |
| `src/main.rs` | `palette_from(&Config)` 把 `config.colors` 折算成 `term::Palette`，由 `apply_runtime_settings` 在配置重载时写入当前窗格 |
| `src/session.rs` | `spawn_with` 在创建终端时调用一次 `set_palette`，新窗格一开始就答得对 |

应答格式按 xterm 的 16 位写法（8 位通道重复一次）：

| 请求 | 应答 |
|---|---|
| `OSC 4 ; index ; ? ST` | `OSC 4 ; index ; rgb:rrrr/gggg/bbbb ST`，成对出现时逐对回答 |
| `OSC 10 ; ? ST` | 前景色 |
| `OSC 11 ; ? ST` | 背景色 |
| `OSC 12 ; ? ST` | 光标色 |
| `OSC 4 ; index ; <颜色>` | 不回答、不生效，写 trace（"忽略 OSC 4 的颜色设置"） |
| `OSC 10/11/12 ; <颜色>` | 同上 |

只读是刻意的：改调色板属于外观写操作，需要和配置、渲染缓存一起设计，本批不做。
未接调色板（`Terminal::new` 直接构造，例如渲染测试）时一律不回答，避免给出错颜色；
索引越界或参数不是数字时跳过该对，不影响同一帧里的其它查询。

`Palette` 放在 `term` 而不是读 `config`，是为了保持终端内核不依赖配置类型；`u32` 的
RGB 取值与 `config::Colors` 一致，不需要额外转换表。

## 4. DECRQSS（`DCS $ q ... ST`）

程序（bash 的 `\eP$qm\e\`、tmux、部分编辑器）用 DECRQSS 读当前设置。`src/vt.rs`
早有 `Dcs*` 状态与 `hook`/`put`/`unhook` 回调，本批在 `impl Perform for Terminal` 里
补上这三个方法：

- `hook`：只有中间字节是 `$`、终止字节是 `q` 才开缓冲，其它 DCS（例如 DECDLD）直接
  忽略，不占内存；
- `put`：请求最长 32 字节，超过部分丢弃，防止程序用超长 DCS 顶内存；
- `unhook`：按请求回答，不认识的回 `DCS 0 $ r ST`。

| 请求 | 应答 |
|---|---|
| `m`（SGR） | `DCS 1 $ r 0;… ST`，按当前属性位与前景/背景色拼 SGR 参数（基本色 30-37/40-47、亮色 90-97/100-107、256 色 `38;5;n`、真彩 `38;2;r;g;b`） |
| `r`（DECSTBM） | `DCS 1 $ r top;bottom ST`（1 起算） |
| ` q`（DECSCUSR） | `DCS 1 $ r n q ST`，方块 2、下划线 4、竖线 6、默认 0 |
| 其他 | `DCS 0 $ r ST` |

`DECSLRM`（`s`）没有回答，因为左右边距模式还没实现，答了会误导程序。

## 5. 应答缓冲上限

应答此前是无界 `Vec<u8>`，程序可以不断发查询把内存顶上去。现在：

- `RESPONSE_LIMIT = 4096`：追加会超限时整笔丢弃，累加 `dropped_responses`；
- `session.rs` 在每轮取走应答后比较计数，增长时打一条 trace（"终端应答缓冲已满，
  累计丢弃 N 字节"），不刷屏、不进 UI；
- 取走（`take_response()`）后缓冲清空，计数保留，用于事后判断程序是否在刷查询。

## 6. 资源与所有权

| 项目 | 结论 |
|---|---|
| 新增依赖 | 无 |
| 新增线程 / 子进程 | 无 |
| 新增缓存 | `cell_pixels`（8 字节/会话）、`dropped_responses`（8 字节/会话）、`Palette`（约 1 KiB/会话，`Option` 未设置时不占堆）、`decrqss` 请求缓冲（上限 32 字节/会话，仅解析期间存在） |
| 数据持有 | `response` 上限 4096 字节/会话；应答仍由 `Session` 在解析循环内同步写回 PTY，不排队、不跨代次；`Palette` 是配置的值拷贝，不持有 `Config` 引用 |
| 取消与回收 | 不涉及新任务；`Terminal` 关闭时随会话一起释放，`cell_pixels` 随窗格重排更新 |

对比：多分屏下每个窗格各有一份 `response` 上限，`MAX_PANES` 个窗格的最坏情况是
`MAX_PANES × 4096` 字节，与既有的每窗格 PTY 队列预算相比可忽略。

## 7. 测试

| 测试 | 覆盖 |
|---|---|
| `input::tests::keypad_follows_application_keypad_mode` | SS3 全映射、普通模式退化、Alt/Shift 退化、主键盘不受影响 |
| `input::tests::text_input_takes_keypad_characters` | `Key::text()` 的字符折算与回车/越界数字 |
| `term::tests::window_size_reports` | `CSI 11/14/16/18 t` 应答与未实现参数无应答 |
| `term::tests::window_pixel_size_uses_cell_size` | 像素尺寸换算、`RIS` 后保留 |
| `term::tests::response_buffer_is_bounded` | 超限丢弃计数、取走后可继续积压 |
| `term::tests::color_queries_answer_from_the_configured_palette` | `OSC 4/10/11/12` 应答、未接调色板不答、设置类请求不生效、越界与非法索引跳过 |
| `term::tests::decrqss_reports_sgr_region_and_cursor_style` | SGR 参数串（含亮色、真彩、256 色）、滚动区域、光标形状、未知设置回 0、超长请求与非 DECRQSS 的 DCS 不回答 |
| `session::tests::terminal_answers_color_queries_from_the_config` | 新建会话按配置回答颜色（spawn 路径） |
| `integration_tests::reloading_config_refreshes_color_query_answers` | 配置重载后颜色立即更新（`apply_runtime_settings` 路径） |
| `workspace::tests::panes_learn_the_cell_size_for_pixel_queries` | 分屏后每个窗格写入自己的格尺寸 |
| `platform::*` 的 `named_key` 测试 | 三平台小键盘键码映射（X11/macOS 直接测映射表，Win32 测 `is_keypad_enter`） |
| `integration_tests::search_ime_copy_and_link_gestures_...` | 小键盘数字进搜索框且不落进终端 |
| `workspace::remote_files::tests::name_form_owns_the_keyboard_...` | 小键盘数字进 SFTP 名称输入 |

运行：

```bash
cargo test --locked input::          # 按键编码
cargo test --locked term::           # 终端应答
cargo test --locked platform::       # 平台键位
cargo test --locked panes_learn      # 分屏格尺寸
```

## 8. 验证状态

| 平台 | 状态 |
|---|---|
| Linux/X11 | 编译、测试、clippy 在本机通过；小键盘按键与 `CSI t` 查询按映射表验证，未接真实终端程序 |
| Windows | 仅 `cargo check/clippy --target x86_64-pc-windows-gnu` 交叉检查，未真机验证 |
| macOS | 仅 `cargo check/clippy --target aarch64-apple-darwin` 交叉检查，未真机验证 |

交叉编译不算真机验证：Windows 的 ConPTY 与 macOS 的 keycode 实际取值仍需真机确认。

## 9. 已知限制

- 颜色查询只答只读部分：`OSC 4/10/11/12` 的**设置**（程序改调色板）被忽略，只写 trace；
- `DECRQSS` 只答 `m`、`r`、` q`，`DECSLRM`（`s`）与其它设置回 `0`；
- `OSC 104` / `110` / `111` / `112`（恢复默认颜色）未实现，也不影响外观；
- `CSI t` 只答只读查询，不支持 `CSI 4/8 ; h ; w t` 这类改变窗口尺寸的请求；
- Kitty 键盘协议未实现（roadmap 第 9 节单独一条）；
- 像素尺寸取整到整数格，程序若按小数缩放会看到舍入误差；
- 应用小键盘模式只影响按键编码，不改变键位重复/锁定行为（那是系统键盘层的事）。
