//! 配置文件（TOML）。
//!
//! 写错的地方不会让程序启动失败：那一项保留默认值，错误收集起来显示在终端里。
//! 完整的选项说明见 `assets/congminal.example.toml`（也就是 `congminal --print-config` 的输出）。

pub mod toml;

use crate::term::CursorShape;
use std::path::{Path, PathBuf};
use toml::{Table, Value};

/// 带注释的默认配置
pub static EXAMPLE: &str = include_str!("../../assets/congminal.example.toml");

pub const DEFAULT_FONT_FAMILIES: [&str; 4] = ["Maple Mono NF CN", "Maple Mono NF", "Maple Mono CN", "Maple Mono"];

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub font: FontConfig,
    pub window: WindowConfig,
    pub colors: Colors,
    pub cursor_shape: CursorShape,
    pub shell: ShellConfig,
    /// [env] 里的额外环境变量
    pub env: Vec<(String, String)>,
    pub option_as_alt: OptionAsAlt,
}

#[derive(Clone, Debug, PartialEq)]
pub struct FontConfig {
    pub families: Vec<String>,
    pub size: f32,
    pub line_height: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WindowConfig {
    pub columns: usize,
    pub rows: usize,
    pub padding: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Colors {
    pub foreground: u32,
    pub background: u32,
    pub cursor: u32,
    pub cursor_text: u32,
    pub bold_is_bright: bool,
    pub palette: [u32; 256],
}

#[derive(Clone, Debug, Default, PartialEq)]
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
            },
            window: WindowConfig { columns: 80, rows: 24, padding: 4.0 },
            colors: Colors {
                foreground: 0xD8D8D8,
                background: 0x1E1E1E,
                cursor: 0xE0E0E0,
                cursor_text: 0x1E1E1E,
                bold_is_bright: true,
                palette: xterm_palette(),
            },
            cursor_shape: CursorShape::Block,
            shell: ShellConfig::default(),
            env: Vec::new(),
            option_as_alt: OptionAsAlt::Both,
        }
    }
}

/// 允许出现的分组和键。[env] 里可以写任意环境变量。
const SCHEMA: &[(&str, &[&str])] = &[
    ("font", &["family", "size", "line-height"]),
    ("window", &["columns", "rows", "padding"]),
    ("colors", &["foreground", "background", "cursor", "cursor-text", "bold-is-bright", "palette"]),
    ("cursor", &["shape"]),
    ("shell", &["program", "args", "working-directory"]),
    ("env", &[]),
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
        let (mut config, warnings) = match path.map(std::fs::read_to_string) {
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

        if let Some(families) = r.strings("font", "family") {
            config.font.families = families;
        }
        if let Some(size) = r.number("font", "size", 4.0, 200.0) {
            config.font.size = size as f32;
        }
        if let Some(line_height) = r.number("font", "line-height", 0.5, 3.0) {
            config.font.line_height = line_height as f32;
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

        let colors = &mut config.colors;
        for (key, slot) in [
            ("foreground", &mut colors.foreground),
            ("background", &mut colors.background),
            ("cursor", &mut colors.cursor),
            ("cursor-text", &mut colors.cursor_text),
        ] {
            if let Some(color) = r.color("colors", key) {
                *slot = color;
            }
        }
        if let Some(bright) = r.boolean("colors", "bold-is-bright") {
            colors.bold_is_bright = bright;
        }
        if let Some(Value::Array(items)) = r.get("colors", "palette") {
            if items.len() > 256 {
                r.warn("colors", "palette", "最多 256 个颜色".into());
            }
            for (i, item) in items.iter().take(256).enumerate() {
                match item {
                    Value::String(s) => match parse_color(s) {
                        Some(color) => colors.palette[i] = color,
                        None => r.warn("colors", "palette", format!("第 {} 个颜色“{s}”无效", i + 1)),
                    },
                    other => {
                        r.warn("colors", "palette", format!("第 {} 个应该是字符串，不是{}", i + 1, other.type_name()))
                    }
                }
            }
        } else if let Some(other) = r.get("colors", "palette") {
            let got = other.type_name();
            r.warn("colors", "palette", format!("应该是颜色数组，不是{got}"));
        }

        if let Some(shape) = r.string("cursor", "shape") {
            match shape.as_str() {
                "block" => config.cursor_shape = CursorShape::Block,
                "underline" => config.cursor_shape = CursorShape::Underline,
                "bar" => config.cursor_shape = CursorShape::Bar,
                _ => r.warn("cursor", "shape", format!("“{shape}”无效，可以是 block、underline、bar")),
            }
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

        (config, r.warnings)
    }

    /// 环境变量优先于配置文件，方便临时试一下
    fn apply_env_overrides(&mut self) {
        if let Ok(fonts) = std::env::var("CONGMINAL_FONT") {
            let families: Vec<String> =
                fonts.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
            if !families.is_empty() {
                self.font.families = families;
            }
        }
        if let Some(size) = std::env::var("CONGMINAL_FONT_SIZE").ok().and_then(|s| s.parse().ok()) {
            self.font.size = size;
        }
        if let Ok(shell) = std::env::var("CONGMINAL_SHELL") {
            self.shell.program = Some(shell);
            self.shell.args.clear();
        }
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
            if section == "env" {
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
    fn example_matches_defaults() {
        // 示例配置里写的值必须和代码里的默认值一致，否则文档就骗人了
        let (config, warnings) = Config::parse(EXAMPLE);
        assert_eq!(warnings, Vec::<String>::new());
        assert_eq!(config, Config::default());
    }

    #[test]
    fn empty_file_is_default() {
        assert_eq!(Config::parse(""), (Config::default(), vec![]));
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
