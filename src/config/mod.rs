//! 配置文件（TOML）。
//!
//! 写错的地方不会让程序启动失败：那一项保留默认值，错误收集起来显示在终端里。
//! 完整的选项说明见 `assets/congminal.example.toml`（也就是 `congminal --print-config` 的输出）。

pub mod git;
pub mod monitor;
pub mod profiles;
#[cfg(any(test, all(feature = "sftp-preview", unix)))]
pub mod sftp;
pub mod ssh;
pub mod theme;
pub mod toml;

use crate::term::CursorShape;
use std::io::Read;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// 带注释的默认配置
pub static EXAMPLE: &str = include_str!("../../assets/congminal.example.toml");

pub const DEFAULT_FONT_FAMILIES: [&str; 4] = ["Maple Mono NF CN", "Maple Mono NF", "Maple Mono CN", "Maple Mono"];

/// 不透明度的下限：再低就基本看不清文字，不再往下放。
pub const MIN_OPACITY: f64 = 0.3;

#[derive(Debug, PartialEq)]
#[cfg_attr(test, derive(Clone))]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub colors: Colors,
    pub theme: theme::Theme,
    /// 配置里写的主题；`reset-theme` 动作回到这个值（`theme` 会被轮换改掉）。
    pub theme_default: theme::Theme,
    color_overrides: theme::Overrides,
    pub bindings: crate::action::Bindings,
    pub notifications: crate::notifications::Settings,
    pub git: git::Settings,
    pub monitor: monitor::Settings,
    /// 系统 SSH 配置的只读摘要：连接页列出的别名与连接它们用的 ssh 程序。
    pub ssh: ssh::Settings,
    #[cfg(any(test, all(feature = "sftp-preview", unix)))]
    pub sftp: sftp::Settings,
    /// 布局持久化在下次启动生效；默认关闭，不增加单终端的 I/O 或线程。
    pub restore_layout: bool,
    pub memory: MemoryConfig,
    pub cursor_shape: CursorShape,
    /// 光标闪烁。默认关：静止界面不持续重绘，需要时再打开。
    pub cursor_blink: bool,
    /// 命令行标记：在提示行左侧画结果色条（需要 shell 集成上报 OSC 133）
    pub command_marks: bool,
    /// 整窗不透明度，0.3–1.0；1.0 表示不透明（默认，走平台默认路径）。
    pub opacity: f64,
    pub shell: ShellConfig,
    pub connections: profiles::Profiles,
    /// [env] 里的额外环境变量
    pub env: Vec<(String, String)>,
    /// 回滚历史保留的行数
    pub scrollback: usize,
    pub option_as_alt: OptionAsAlt,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub families: Vec<String>,
    pub size: f32,
    pub line_height: f32,
    /// 编程连字（`->` `=>` `!=` 这类显示成连起来的样子），要字体支持
    pub ligatures: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowConfig {
    pub columns: usize,
    pub rows: usize,
    pub padding: f32,
}
#[derive(Clone, Debug, PartialEq)]
pub struct MemoryConfig {
    pub managed_mib: usize,
    pub mapped_font_mib: usize,
}
impl Default for MemoryConfig {
    fn default() -> Self {
        Self { managed_mib: 512, mapped_font_mib: 1024 }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Colors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub cursor_text: u32,
    pub selection_background: u32,
    pub preedit_background: u32,
    pub bold_is_bright: bool,
    pub palette: [u32; 256],
}

#[derive(Debug, Default, PartialEq)]
#[cfg_attr(test, derive(Clone))]
pub struct ShellConfig {
    /// None 表示用系统默认的 shell
    pub program: Option<String>,
    pub args: Vec<String>,
    pub working_directory: Option<PathBuf>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OptionAsAlt {
    None,
    Left,
    Right,
    Both,
}

impl Default for Config {
    fn default() -> Config {
        Config {
            font: FontConfig {
                families: DEFAULT_FONT_FAMILIES.iter().map(|s| s.to_string()).collect(),
                size: 15.0,
                line_height: 1.0,
                ligatures: true,
            },
            window: WindowConfig { columns: 80, rows: 24, padding: 4.0 },
            colors: Colors::default(),
            theme: theme::Theme::default(),
            theme_default: theme::Theme::default(),
            color_overrides: theme::Overrides::default(),
            bindings: crate::action::Bindings::default(),
            notifications: crate::notifications::Settings::default(),
            git: git::Settings::default(),
            monitor: monitor::Settings::default(),
            ssh: ssh::Settings::default(),
            #[cfg(any(test, all(feature = "sftp-preview", unix)))]
            sftp: sftp::Settings::default(),
            restore_layout: false,
            memory: MemoryConfig::default(),
            cursor_shape: CursorShape::Block,
            cursor_blink: false,
            command_marks: true,
            opacity: 1.0,
            shell: ShellConfig::default(),
            connections: profiles::Profiles::default(),
            env: Vec::new(),
            scrollback: crate::term::DEFAULT_SCROLLBACK,
            option_as_alt: OptionAsAlt::Both,
        }
    }
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: 0xD8D8D8,
            background: 0x1E1E1E,
            cursor: 0xE0E0E0,
            cursor_text: 0x1E1E1E,
            selection_background: 0x264F78,
            preedit_background: 0x3A3D41,
            bold_is_bright: true,
            palette: xterm_palette(),
        }
    }
}

/// 允许出现的分组和键。[env] 里可以写任意环境变量。
const SCHEMA: &[(&str, &[&str])] = &[
    ("appearance", &["theme", "cursor-blink", "command-marks", "opacity"]),
    ("keybindings", &[]),
    ("font", &["family", "size", "line-height", "ligatures"]),
    ("window", &["columns", "rows", "padding"]),
    ("workspace", &["restore-layout"]),
    ("git", &["enabled", "program", "refresh-seconds", "timeout-seconds", "max-files", "diff-mib"]),
    ("ssh", &["program", "config-file"]),
    (
        "monitor",
        &[
            "enabled",
            "interval-seconds",
            "history-seconds",
            "disk-seconds",
            "alert-cpu",
            "alert-memory",
            "alert-disk",
            "alert-seconds",
            "processes",
        ],
    ),
    ("sftp", &["enabled", "helper", "ssh-config"]),
    ("memory", &["managed-mib", "mapped-font-mib"]),
    (
        "colors",
        &[
            "foreground",
            "background",
            "cursor",
            "cursor-text",
            "selection-background",
            "preedit-background",
            "bold-is-bright",
            "palette",
        ],
    ),
    (
        "notifications",
        &[
            "muted",
            "system",
            "failures",
            "long-commands",
            "background",
            "transfers",
            "git",
            "monitor",
            "ssh",
            "long-seconds",
            "dedupe-seconds",
            "burst",
            "hide-context",
            "mute-hosts",
        ],
    ),
    ("cursor", &["shape"]),
    ("scrollback", &["lines"]),
    ("shell", &["program", "args", "working-directory"]),
    ("env", &[]),
    ("connections", &[]),
    ("macos", &["option-as-alt"]),
];

impl Config {
    /// 配置文件路径：CONGMINAL_CONFIG，否则是各平台的标准位置
    pub fn default_path() -> Option<PathBuf> {
        if let Some(path) = std::env::var_os("CONGMINAL_CONFIG") {
            return Some(PathBuf::from(path));
        }
        let base = if cfg!(windows) {
            std::env::var_os("APPDATA").map(PathBuf::from)
        } else {
            std::env::var_os("XDG_CONFIG_HOME")
                .map(PathBuf::from)
                .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        };
        base.map(|dir| dir.join("congminal").join("congminal.toml"))
    }

    /// 读取配置。文件不存在时用默认值。返回配置和需要提示给用户的问题。
    pub fn load(path: Option<&Path>) -> (Config, Vec<String>) {
        let read = |path: &Path| -> std::io::Result<String> {
            let file = std::fs::File::open(path)?;
            let mut bytes = Vec::new();
            file.take(toml::MAX_CONFIG_BYTES as u64 + 1).read_to_end(&mut bytes)?;
            if bytes.len() > toml::MAX_CONFIG_BYTES {
                return Err(std::io::Error::other("配置超过 1 MiB"));
            }
            String::from_utf8(bytes).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
        };
        let (mut config, warnings) = match path.map(read) {
            Some(Ok(text)) => Config::parse(&text),
            Some(Err(e)) if e.kind() != std::io::ErrorKind::NotFound => {
                (Config::default(), vec![format!("读取失败：{e}")])
            }
            _ => (Config::default(), Vec::new()),
        };
        config.apply_env_overrides();
        (config, warnings)
    }

    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut config = Config::default();
        let root = match toml::parse(text) {
            Ok(root) => root,
            // 语法错误时整个文件都没法用，全部用默认值
            Err(e) => return (config, vec![format!("{e}（整个文件被忽略）")]),
        };
        let mut r = Reader { root: &root, warnings: Vec::new() };
        r.check_unknown_keys();
        config.git = git::read(&mut r);
        config.monitor = monitor::read(&mut r);
        config.ssh = ssh::read(&mut r);
        #[cfg(any(test, all(feature = "sftp-preview", unix)))]
        {
            config.sftp = sftp::read(&mut r);
        }
        #[cfg(not(any(test, all(feature = "sftp-preview", unix))))]
        if r.boolean("sftp", "enabled") == Some(true) {
            r.warn("sftp", "enabled", "本构建没有 SFTP 预览入口，请使用 sftp-preview 构建".into());
        }

        if let Some(families) = r.strings("font", "family") {
            if families.len() > 16 {
                r.warn("font", "family", "最多使用前 16 个字体".into());
            }
            config.font.families = families.into_iter().take(16).collect();
        }
        if let Some(size) = r.number("font", "size", 4.0, 200.0) {
            config.font.size = size as f32;
        }
        if let Some(line_height) = r.number("font", "line-height", 0.5, 3.0) {
            config.font.line_height = line_height as f32;
        }
        if let Some(ligatures) = r.boolean("font", "ligatures") {
            config.font.ligatures = ligatures;
        }

        if let Some(columns) = r.number("window", "columns", 2.0, 1000.0) {
            config.window.columns = columns as usize;
        }
        if let Some(rows) = r.number("window", "rows", 1.0, 500.0) {
            config.window.rows = rows as usize;
        }
        if let Some(padding) = r.number("window", "padding", 0.0, 200.0) {
            config.window.padding = padding as f32;
        }
        if let Some(enabled) = r.boolean("workspace", "restore-layout") {
            config.restore_layout = enabled;
        }
        if let Some(value) = r.integer("memory", "managed-mib", 128, 4096) {
            config.memory.managed_mib = value;
        }
        if let Some(value) = r.integer("memory", "mapped-font-mib", 64, 4096) {
            config.memory.mapped_font_mib = value;
        }

        if let Some(name) = r.string("appearance", "theme") {
            match theme::Theme::parse(&name) {
                Some(theme) => {
                    config.theme = theme;
                    config.theme_default = theme;
                }
                None => r.warn("appearance", "theme", format!("“{name}”无效，可以是 dark、light、midnight")),
            }
        }
        for (key, slot) in [
            ("muted", &mut config.notifications.muted),
            ("system", &mut config.notifications.system),
            ("failures", &mut config.notifications.failures),
            ("long-commands", &mut config.notifications.long_commands),
            ("background", &mut config.notifications.background),
            ("transfers", &mut config.notifications.transfers),
            ("git", &mut config.notifications.git),
            ("monitor", &mut config.notifications.monitor),
            ("ssh", &mut config.notifications.ssh),
            ("hide-context", &mut config.notifications.hide_context),
        ] {
            if let Some(value) = r.boolean("notifications", key) {
                *slot = value;
            }
        }
        if let Some(value) = r.number("notifications", "long-seconds", 1.0, 86400.0) {
            config.notifications.long_seconds = value as u64;
        }
        if let Some(value) = r.number("notifications", "dedupe-seconds", 0.0, 600.0) {
            config.notifications.dedupe_seconds = value as u64;
        }
        if let Some(value) = r.number("notifications", "burst", 1.0, 1000.0) {
            config.notifications.burst = value as u32;
        }
        if let Some(list) = r.strings("notifications", "mute-hosts") {
            let mut hosts: Vec<String> = Vec::new();
            for entry in list {
                let host = entry.trim();
                if host.is_empty() || host.len() > crate::notifications::MUTE_HOST_BYTES {
                    r.warn("notifications", "mute-hosts", format!("“{entry}”不是有效主机名，已忽略"));
                    continue;
                }
                if hosts.len() == crate::notifications::MUTE_HOSTS {
                    r.warn(
                        "notifications",
                        "mute-hosts",
                        format!("最多 {} 个主机，其余已忽略", crate::notifications::MUTE_HOSTS),
                    );
                    break;
                }
                if hosts.iter().any(|kept| kept.eq_ignore_ascii_case(host)) {
                    continue;
                }
                // 统一小写后按大小写不敏感比较，配置里的大小写不影响命中。
                hosts.push(host.to_ascii_lowercase());
            }
            config.notifications.mute_hosts = hosts;
        }
        let overrides = &mut config.color_overrides;
        for (key, slot) in [
            ("foreground", &mut overrides.foreground),
            ("background", &mut overrides.background),
            ("cursor", &mut overrides.cursor),
            ("cursor-text", &mut overrides.cursor_text),
            ("selection-background", &mut overrides.selection_background),
            ("preedit-background", &mut overrides.preedit_background),
        ] {
            *slot = r.color("colors", key);
        }
        overrides.bold_is_bright = r.boolean("colors", "bold-is-bright");
        if let Some(Value::Array(items)) = r.get("colors", "palette") {
            if items.len() > 256 {
                r.warn("colors", "palette", "最多 256 个颜色".into());
            }
            for (i, item) in items.iter().take(256).enumerate() {
                match item {
                    Value::String(s) => match parse_color(s) {
                        Some(color) => overrides.palette.push((i, color)),
                        None => r.warn("colors", "palette", format!("第 {} 个颜色“{s}”无效", i + 1)),
                    },
                    other => {
                        r.warn("colors", "palette", format!("第 {} 个应该是字符串，不是{}", i + 1, other.type_name()))
                    }
                }
            }
        } else if let Some(other) = r.get("colors", "palette") {
            r.warn("colors", "palette", format!("应该是颜色数组，不是{}", other.type_name()));
        }
        config.refresh_theme();
        if let Some(Value::Table(table)) = root.get("keybindings") {
            r.warnings.extend(config.bindings.configure(table));
        }

        if let Some(blink) = r.boolean("appearance", "cursor-blink") {
            config.cursor_blink = blink;
        }
        if let Some(marks) = r.boolean("appearance", "command-marks") {
            config.command_marks = marks;
        }
        if let Some(opacity) = r.number("appearance", "opacity", MIN_OPACITY, 1.0) {
            config.opacity = opacity;
        }
        if let Some(shape) = r.string("cursor", "shape") {
            match shape.as_str() {
                "block" => config.cursor_shape = CursorShape::Block,
                "underline" => config.cursor_shape = CursorShape::Underline,
                "bar" => config.cursor_shape = CursorShape::Bar,
                _ => r.warn("cursor", "shape", format!("“{shape}”无效，可以是 block、underline、bar")),
            }
        }

        if let Some(lines) = r.number("scrollback", "lines", 0.0, 1_000_000.0) {
            config.scrollback = lines as usize;
        }

        config.shell.program = r.string("shell", "program").filter(|p| !p.is_empty());
        if let Some(args) = r.strings("shell", "args") {
            config.shell.args = args;
        }
        config.shell.working_directory = r.string("shell", "working-directory").map(|dir| expand_home(&dir));

        if let Some(Value::Table(env)) = root.get("env") {
            for (key, value) in &env.entries {
                match value {
                    Value::String(v) => config.env.push((key.clone(), v.clone())),
                    other => r.warn("env", key, format!("环境变量的值要写成字符串，不是{}", other.type_name())),
                }
            }
        }

        match r.get("macos", "option-as-alt") {
            None => {}
            Some(Value::Boolean(true)) => config.option_as_alt = OptionAsAlt::Both,
            Some(Value::Boolean(false)) => config.option_as_alt = OptionAsAlt::None,
            Some(Value::String(s)) if s == "left" => config.option_as_alt = OptionAsAlt::Left,
            Some(Value::String(s)) if s == "right" => config.option_as_alt = OptionAsAlt::Right,
            Some(Value::String(s)) if s == "both" => config.option_as_alt = OptionAsAlt::Both,
            Some(Value::String(s)) if s == "none" => config.option_as_alt = OptionAsAlt::None,
            Some(_) => r.warn("macos", "option-as-alt", "可以是 true、false、\"left\"、\"right\"".into()),
        }

        // 引导阶段未初始化进程池；独立额度先保护新增持久数据，发布前 adopt。
        if root.get("connections").is_some() {
            let pool = crate::resources::configuration(config.memory.managed_mib * 1024 * 1024);
            config.connections = profiles::Profiles::parse(&root, &pool, &mut r.warnings);
        }
        (config, r.warnings)
    }

    pub fn admit_connections(&mut self, warnings: &mut Vec<String>) {
        if let Err(error) = self.connections.adopt(&crate::resources::current()) {
            self.connections = profiles::Profiles::default();
            warnings.push(error.into());
        }
    }

    /// 环境变量优先于配置文件，方便临时试一下
    fn apply_env_overrides(&mut self) {
        if let Ok(fonts) = std::env::var("CONGMINAL_FONT") {
            let families: Vec<String> =
                fonts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if !families.is_empty() {
                self.font.families = families.into_iter().take(16).collect();
            }
        }
        if let Some(size) = std::env::var("CONGMINAL_FONT_SIZE")
            .ok()
            .and_then(|s| s.parse::<f32>().ok())
            .filter(|n| n.is_finite() && (4.0..=200.0).contains(n))
        {
            self.font.size = size;
        }
        if let Ok(shell) = std::env::var("CONGMINAL_SHELL") {
            self.shell.program = Some(shell);
            self.shell.args.clear();
        }
    }

    /// 换了主题之后重算颜色；主题预览也会直接调它。
    pub fn refresh_theme(&mut self) {
        self.colors = self.theme.colors();
        self.color_overrides.apply(&mut self.colors);
    }

    pub fn cycle_theme(&mut self) {
        self.theme = self.theme.next();
        self.refresh_theme();
    }

    /// 回到配置里写的主题，用于误按轮换之后复位。
    pub fn reset_theme(&mut self) {
        self.theme = self.theme_default;
        self.refresh_theme();
    }

    /// 用户是否自己指定了字体（只有这时找不到字体才值得提示）
    pub fn font_is_customized(&self) -> bool {
        self.font.families != DEFAULT_FONT_FAMILIES
    }
}

struct Reader<'a> {
    root: &'a Table,
    warnings: Vec<String>,
}

impl<'a> Reader<'a> {
    fn integer(&mut self, section: &str, key: &str, min: i64, max: i64) -> Option<usize> {
        match self.get(section, key)? {
            Value::Integer(value) if (min..=max).contains(value) => Some(*value as usize),
            _ => {
                self.warn(section, key, format!("需要 {min} 到 {max} 的整数"));
                None
            }
        }
    }
    fn warn(&mut self, section: &str, key: &str, message: String) {
        self.warnings.push(format!("[{section}] {key}：{message}"));
    }

    fn check_unknown_keys(&mut self) {
        for (section, value) in &self.root.entries {
            let Some((_, keys)) = SCHEMA.iter().find(|(name, _)| name == section) else {
                self.warnings.push(format!("不认识的分组或选项“{section}”"));
                continue;
            };
            let Value::Table(table) = value else {
                self.warnings.push(format!("“{section}”应该是一个分组 [{section}]"));
                continue;
            };
            if section == "env" || section == "keybindings" || section == "connections" {
                continue;
            }
            for (key, _) in &table.entries {
                if !keys.contains(&key.as_str()) {
                    self.warnings.push(format!("[{section}] 里不认识的选项“{key}”"));
                }
            }
        }
    }

    fn get(&self, section: &str, key: &str) -> Option<&'a Value> {
        match self.root.get(section)? {
            Value::Table(table) => table.get(key),
            _ => None,
        }
    }

    fn string(&mut self, section: &str, key: &str) -> Option<String> {
        match self.get(section, key)? {
            Value::String(s) => Some(s.clone()),
            other => {
                self.warn(section, key, format!("应该是字符串，不是{}（记得加引号）", other.type_name()));
                None
            }
        }
    }

    /// 字符串，或者字符串数组
    fn strings(&mut self, section: &str, key: &str) -> Option<Vec<String>> {
        match self.get(section, key)? {
            Value::String(s) => Some(vec![s.clone()]),
            Value::Array(items) => {
                let strings: Option<Vec<String>> = items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => Some(s.clone()),
                        _ => None,
                    })
                    .collect();
                if strings.is_none() {
                    self.warn(section, key, "数组里只能是字符串".into());
                }
                strings
            }
            other => {
                self.warn(section, key, format!("应该是字符串或字符串数组，不是{}", other.type_name()));
                None
            }
        }
    }

    fn number(&mut self, section: &str, key: &str, min: f64, max: f64) -> Option<f64> {
        let n = match self.get(section, key)? {
            Value::Integer(i) => *i as f64,
            Value::Float(f) => *f,
            other => {
                self.warn(section, key, format!("应该是数字，不是{}", other.type_name()));
                return None;
            }
        };
        if !(min..=max).contains(&n) {
            self.warn(section, key, format!("{n} 超出范围（{min} 到 {max}）"));
            return None;
        }
        Some(n)
    }

    fn boolean(&mut self, section: &str, key: &str) -> Option<bool> {
        match self.get(section, key)? {
            Value::Boolean(b) => Some(*b),
            other => {
                self.warn(section, key, format!("应该是 true 或 false，不是{}", other.type_name()));
                None
            }
        }
    }

    fn color(&mut self, section: &str, key: &str) -> Option<u32> {
        let s = self.string(section, key)?;
        let color = parse_color(&s);
        if color.is_none() {
            self.warn(section, key, format!("“{s}”不是有效的颜色（写成 \"#rrggbb\"）"));
        }
        color
    }
}

/// "#rrggbb"、"#rgb" 或 "0xrrggbb"
pub fn parse_color(s: &str) -> Option<u32> {
    let hex = s.strip_prefix('#').or_else(|| s.strip_prefix("0x"))?;
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    match hex.len() {
        6 => u32::from_str_radix(hex, 16).ok(),
        3 => {
            let v = u32::from_str_radix(hex, 16).ok()?;
            let (r, g, b) = ((v >> 8) & 0xF, (v >> 4) & 0xF, v & 0xF);
            Some(((r * 0x11) << 16) | ((g * 0x11) << 8) | (b * 0x11))
        }
        _ => None,
    }
}

/// "~" 开头的路径换成用户主目录
fn expand_home(path: &str) -> PathBuf {
    let home = std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" });
    match (path.strip_prefix('~'), home) {
        (Some(rest), Some(home)) if rest.is_empty() || rest.starts_with(['/', '\\']) => {
            PathBuf::from(home).join(rest.trim_start_matches(['/', '\\']))
        }
        _ => PathBuf::from(path),
    }
}

/// xterm 的 256 色调色板：16 个基本色 + 6×6×6 色立方 + 24 级灰度
fn xterm_palette() -> [u32; 256] {
    let rgb = |r: u32, g: u32, b: u32| r << 16 | g << 8 | b;
    let mut palette = [0; 256];
    palette[..16].copy_from_slice(&[
        0x000000, 0xCD3131, 0x0DBC79, 0xE5E510, 0x2472C8, 0xBC3FBC, 0x11A8CD, 0xE5E5E5, //
        0x666666, 0xF14C4C, 0x23D18B, 0xF5F543, 0x3B8EEA, 0xD670D6, 0x29B8DB, 0xFFFFFF,
    ]);
    let level = |v: usize| if v == 0 { 0 } else { (55 + v * 40) as u32 };
    for i in 0..216 {
        palette[16 + i] = rgb(level(i / 36), level(i / 6 % 6), level(i % 6));
    }
    for i in 0..24 {
        let v = (8 + i * 10) as u32;
        palette[232 + i] = rgb(v, v, v);
    }
    palette
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opacity_parses_within_range_and_warns_outside_it() {
        // 默认 1.0：不碰窗口属性，行为与加这个功能之前完全一样。
        let (config, warnings) = Config::parse("");
        assert_eq!(config.opacity, 1.0);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (config, warnings) = Config::parse("[appearance]\nopacity = 0.85");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!((config.opacity - 0.85).abs() < 1e-9);
        // 低于下限与高于 1.0 都只告警并保留默认值，不会让窗口“看不见”或反相。
        let (config, warnings) = Config::parse("[appearance]\nopacity = 0.05");
        assert_eq!(config.opacity, 1.0);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        let (config, warnings) = Config::parse("[appearance]\nopacity = 3");
        assert_eq!(config.opacity, 1.0);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
    }

    #[test]
    fn appearance_parses_cursor_blink_and_reset_returns_to_the_configured_theme() {
        let (mut config, warnings) = Config::parse("[appearance]\ntheme='midnight'\ncursor-blink=true");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert!(config.cursor_blink);
        assert_eq!(config.theme, theme::Theme::Midnight);
        assert_eq!(config.theme_default, theme::Theme::Midnight);

        // 轮换之后能回到配置里写的主题，而不是编译默认的 dark
        config.cycle_theme();
        assert_ne!(config.theme, theme::Theme::Midnight);
        config.reset_theme();
        assert_eq!(config.theme, theme::Theme::Midnight);

        // 默认不闪烁：静止界面不为了光标持续重绘；命令行标记默认开
        let (config, warnings) = Config::parse("");
        assert!(!config.cursor_blink);
        assert!(config.command_marks);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (config, warnings) = Config::parse("[appearance]\ncommand-marks=false");
        assert!(!config.command_marks);
        assert!(warnings.is_empty(), "{warnings:?}");

        // 未知键仍然被拦住，说明新键加进了 schema
        let (_, warnings) = Config::parse("[appearance]\ncursor-blink=1");
        assert!(!warnings.is_empty());
    }

    #[test]
    fn notification_settings_parse_hidden_context_and_clamp_rate_limits() {
        let (config, warnings) = Config::parse(
            r#"
[notifications]
hide-context = true
dedupe-seconds = 900
burst = 0
"#,
        );
        assert!(config.notifications.hide_context);
        // 越界值不生效，保留默认并各给一条警告。
        assert_eq!(config.notifications.dedupe_seconds, 5);
        assert_eq!(config.notifications.burst, 12);
        assert_eq!(warnings.len(), 2, "{warnings:?}");
        let (default, warnings) = Config::parse("");
        assert!(!default.notifications.hide_context);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn ssh_notifications_switch_is_read_and_defaults_to_on() {
        let (config, warnings) = Config::parse("[notifications]\nssh = false\n");
        assert!(!config.notifications.ssh);
        assert!(warnings.is_empty(), "{warnings:?}");
        let (default, warnings) = Config::parse("");
        assert!(default.notifications.ssh);
        assert!(warnings.is_empty(), "{warnings:?}");
    }

    #[test]
    fn ssh_settings_read_the_program_and_alias_list_from_the_configured_file() {
        let dir = std::env::temp_dir().join(format!("cg-config-ssh-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("config");
        std::fs::write(&file, "Host web-1 db\n  HostName 10.0.0.3\nHost *\n").unwrap();
        let (config, warnings) =
            Config::parse(&format!("[ssh]\nprogram = \"/opt/ssh\"\nconfig-file = {:?}\n", file.to_str().unwrap()));
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(config.ssh.program, std::path::PathBuf::from("/opt/ssh"));
        assert_eq!(config.ssh.aliases, vec!["web-1".to_string(), "db".to_string()]);
        // 默认：PATH 里的 ssh 与 ~/.ssh/config；文件不存在时列表为空且不报警。
        let (default, warnings) = Config::parse("[ssh]\nconfig-file = \"/nonexistent/congminal/ssh-config\"\n");
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(default.ssh.program, std::path::PathBuf::from("ssh"));
        assert!(default.ssh.aliases.is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn mute_hosts_are_normalized_deduped_and_bounded() {
        let (config, warnings) = Config::parse(
            r#"
[notifications]
mute-hosts = [" Web-1 ", "web-1", "", "db.example.com"]
"#,
        );
        // 去空白、统一小写、去重；空串被忽略并给出一条警告。
        assert_eq!(config.notifications.mute_hosts, vec!["web-1".to_string(), "db.example.com".to_string()]);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        // 单个字符串也接受。
        let (single, warnings) = Config::parse("[notifications]\nmute-hosts = \"host-a\"\n");
        assert_eq!(single.notifications.mute_hosts, vec!["host-a".to_string()]);
        assert!(warnings.is_empty(), "{warnings:?}");
        // 超长列表只保留上限条数，并提示其余被忽略。
        let many: Vec<String> = (0..40).map(|i| format!("\"host-{i}\"")).collect();
        let (bounded, warnings) = Config::parse(&format!("[notifications]\nmute-hosts = [{}]\n", many.join(",")));
        assert_eq!(bounded.notifications.mute_hosts.len(), crate::notifications::MUTE_HOSTS);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("最多"), "{warnings:?}");
    }

    #[test]
    fn themes_keep_explicit_overrides_and_reject_bad_names() {
        let (mut config, warnings) = Config::parse(
            r##"
[appearance]
theme = "light"
[colors]
background = "#123456"
preedit-background = "#234567"
palette = ["#345678"]
[keybindings]
"Primary+Equal" = "font-increase"
"Primary+Minus" = "font-decrease"
"Primary+0" = "font-reset"
"F9" = {action = "send-text", text = "中文"}
"F10" = "pass-through"
"F11" = "none"
"##,
        );
        assert!(warnings.is_empty(), "{warnings:?}");
        let light = config.colors.foreground;
        config.cycle_theme();
        assert_ne!(config.colors.foreground, light);
        for _ in 0..99 {
            assert_eq!(config.colors.background, 0x123456);
            assert_eq!(config.colors.preedit_background, 0x234567);
            assert_eq!(config.colors.palette[0], 0x345678);
            config.cycle_theme();
        }
        let (bad, warnings) = Config::parse("[appearance]\ntheme='typo'");
        assert_eq!(bad, Config::default());
        assert_eq!(warnings.len(), 1);
    }

    #[test]
    fn example_matches_defaults() {
        // 示例配置里写的值必须和代码里的默认值一致，否则文档就骗人了
        let (mut config, warnings) = Config::parse(EXAMPLE);
        assert_eq!(warnings, Vec::<String>::new());
        // 别名列表来自这台机器的 ~/.ssh/config，不属于“默认值”，单独比。
        config.ssh.aliases.clear();
        assert_eq!(config, Config::default());
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(Config::parse(""), (Config::default(), vec![]));
    }

    #[test]
    fn layout_persistence_is_explicit_and_type_checked() {
        let (config, warnings) = Config::parse("[workspace]\nrestore-layout=true");
        assert!(config.restore_layout);
        assert!(warnings.is_empty());
        let (config, warnings) = Config::parse("[workspace]\nrestore-layout='true'");
        assert!(!config.restore_layout);
        assert_eq!(warnings.len(), 1);
    }
    #[test]
    fn shared_memory_limits_are_bounded_integers() {
        let (config, warnings) = Config::parse("[memory]\nmanaged-mib=128\nmapped-font-mib=64");
        assert!(warnings.is_empty());
        assert_eq!(config.memory, MemoryConfig { managed_mib: 128, mapped_font_mib: 64 });
        for value in ["127", "4097", "512.5", "true", "'512'", "nan", "-1"] {
            let (config, warnings) = Config::parse(&format!("[memory]\nmanaged-mib={value}"));
            assert_eq!(config.memory, MemoryConfig::default());
            assert!(!warnings.is_empty());
        }
    }

    #[test]
    fn overrides() {
        let (config, warnings) = Config::parse(
            r##"
[font]
family = "JetBrains Mono"
size = 13.5
[window]
columns = 120
[colors]
background = "#000"
palette = ["#111111"]
[cursor]
shape = "bar"
[shell]
program = "/bin/zsh"
args = ["-l"]
[env]
LANG = "zh_CN.UTF-8"
[macos]
option-as-alt = "left"
"##,
        );
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(config.font.families, ["JetBrains Mono"]);
        assert_eq!(config.font.size, 13.5);
        assert_eq!(config.window.columns, 120);
        assert_eq!(config.window.rows, 24);
        assert_eq!(config.colors.background, 0x000000);
        assert_eq!(config.colors.palette[0], 0x111111);
        assert_eq!(config.colors.palette[1], 0xCD3131);
        assert_eq!(config.cursor_shape, CursorShape::Bar);
        assert_eq!(config.shell.program.as_deref(), Some("/bin/zsh"));
        assert_eq!(config.shell.args, ["-l"]);
        assert_eq!(config.env, [("LANG".to_string(), "zh_CN.UTF-8".to_string())]);
        assert_eq!(config.option_as_alt, OptionAsAlt::Left);
    }

    #[test]
    fn mistakes_keep_defaults_and_warn() {
        let (config, warnings) = Config::parse(
            r#"
[font]
size = "big"
colour = 1
[window]
columns = 5000
[colors]
foreground = "red"
[cursor]
shape = "circle"
[typo]
"#,
        );
        assert_eq!(config, Config::default());
        assert_eq!(
            warnings,
            [
                "[font] 里不认识的选项“colour”",
                "不认识的分组或选项“typo”",
                "[font] size：应该是数字，不是字符串",
                "[window] columns：5000 超出范围（2 到 1000）",
                "[colors] foreground：“red”不是有效的颜色（写成 \"#rrggbb\"）",
                "[cursor] shape：“circle”无效，可以是 block、underline、bar",
            ]
        );
    }

    #[test]
    fn syntax_error_ignores_file() {
        let (config, warnings) = Config::parse("[font]\nsize = \n");
        assert_eq!(config, Config::default());
        assert_eq!(warnings, ["第 2 行：等号后面缺少值（整个文件被忽略）"]);
    }

    #[test]
    fn colors() {
        assert_eq!(parse_color("#1e1e1e"), Some(0x1E1E1E));
        assert_eq!(parse_color("#fff"), Some(0xFFFFFF));
        assert_eq!(parse_color("0x123456"), Some(0x123456));
        assert_eq!(parse_color("red"), None);
        assert_eq!(parse_color("#12345"), None);
    }
}
