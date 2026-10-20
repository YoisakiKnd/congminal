//! TOML 子集解析器（手写）。
//!
//! 支持：注释、`[表]` 和 `[a.b]` 表头、裸键 / 引号键 / 点分键、基本字符串（含转义）、字面字符串、
//! 整数（十进制、0x、0o、0b）、浮点数（含 inf、nan）、布尔值、数组（可以跨行）、内联表。
//! 不支持（遇到会报错）：多行字符串、日期时间、表数组 `[[x]]`。
//!
//! 规范：<https://toml.io/cn/v1.0.0>

use std::fmt;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    String(String),
    Integer(i64),
    Float(f64),
    Boolean(bool),
    Array(Vec<Value>),
    Table(Table),
}

impl Value {
    /// 给错误提示用的类型名
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::String(_) => "字符串",
            Value::Integer(_) => "整数",
            Value::Float(_) => "小数",
            Value::Boolean(_) => "布尔值",
            Value::Array(_) => "数组",
            Value::Table(_) => "表",
        }
    }
}

/// 表：保持键的书写顺序
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Table {
    pub entries: Vec<(String, Value)>,
}

impl Table {
    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.iter().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn get_mut(&mut self, key: &str) -> Option<&mut Value> {
        self.entries.iter_mut().find(|(k, _)| k == key).map(|(_, v)| v)
    }

    fn insert(&mut self, key: String, value: Value) -> Result<(), String> {
        if self.get(&key).is_some() {
            return Err(format!("键“{key}”重复定义"));
        }
        self.entries.push((key, value));
        Ok(())
    }

    /// 沿着路径找到（必要时创建）子表
    fn subtable(&mut self, path: &[String]) -> Result<&mut Table, String> {
        let Some((first, rest)) = path.split_first() else { return Ok(self) };
        if self.get(first).is_none() {
            self.entries.push((first.clone(), Value::Table(Table::default())));
        }
        match self.get_mut(first) {
            Some(Value::Table(table)) => table.subtable(rest),
            _ => Err(format!("“{first}”已经定义为普通值，不能再当作表")),
        }
    }
}

#[derive(Debug, PartialEq)]
pub struct Error {
    pub line: usize,
    pub message: String,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "第 {} 行：{}", self.line, self.message)
    }
}

pub const MAX_CONFIG_BYTES: usize = 1024 * 1024;
pub fn parse(text: &str) -> Result<Table, Error> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err(Error { line: 1, message: "配置超过 1 MiB".into() });
    }
    // Windows 的记事本保存 UTF-8 时可能在开头加 BOM
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut parser = Parser { chars: text.chars().collect(), pos: 0, line: 1, depth: 0, items: 0 };
    parser.document().map_err(|message| Error { line: parser.line, message })
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    line: usize,
    depth: usize,
    items: usize,
}

type Result2<T> = Result<T, String>;

impl Parser {
    fn document(&mut self) -> Result2<Table> {
        let mut root = Table::default();
        let mut current: Vec<String> = Vec::new();
        // 用 [表头] 显式定义过的表，同一个表不能定义两次
        let mut defined: Vec<Vec<String>> = Vec::new();
        loop {
            self.skip_blank_lines();
            // 重复定义这类错误在整行读完之后才发现，报错时用这一行开头的行号
            let start_line = self.line;
            let at_start = |parser: &mut Parser, message: String| {
                parser.line = start_line;
                message
            };
            match self.peek() {
                None => return Ok(root),
                Some('[') => {
                    self.pos += 1;
                    if self.peek() == Some('[') {
                        return Err("还不支持表数组 [[...]]".into());
                    }
                    self.skip_spaces();
                    let path = self.key()?;
                    self.skip_spaces();
                    self.expect(']')?;
                    self.end_of_line()?;
                    if defined.contains(&path) {
                        return Err(at_start(self, format!("表 [{}] 重复定义", path.join("."))));
                    }
                    root.subtable(&path).map_err(|e| at_start(self, e))?;
                    defined.push(path.clone());
                    current = path;
                }
                Some(_) => {
                    let key = self.key()?;
                    self.skip_spaces();
                    self.expect('=')?;
                    self.skip_spaces();
                    let value = self.value()?;
                    self.end_of_line()?;
                    let (last, parents) = key.split_last().expect("键至少有一段");
                    root.subtable(&current)
                        .and_then(|table| table.subtable(parents))
                        .and_then(|table| table.insert(last.clone(), value))
                        .map_err(|e| at_start(self, e))?;
                }
            }
        }
    }

    // ---- 键 ----

    /// 点分键：a.b."c d"
    fn key(&mut self) -> Result2<Vec<String>> {
        if self.items >= 4096 {
            return Err("配置条目数超过限制".into());
        }
        self.items += 1;
        let mut parts = vec![self.simple_key()?];
        loop {
            let saved = self.pos;
            self.skip_spaces();
            if self.peek() != Some('.') {
                self.pos = saved;
                return Ok(parts);
            }
            self.pos += 1;
            self.skip_spaces();
            if parts.len() >= 32 {
                return Err("键路径超过 32 层".into());
            }
            parts.push(self.simple_key()?);
        }
    }

    fn simple_key(&mut self) -> Result2<String> {
        match self.peek() {
            Some('"') => self.basic_string(),
            Some('\'') => self.literal_string(),
            _ => {
                let start = self.pos;
                while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || c == '_' || c == '-') {
                    self.pos += 1;
                }
                if start == self.pos {
                    return Err(match self.peek() {
                        Some(c) => format!("这里应该是一个键，但遇到了“{c}”"),
                        None => "这里应该是一个键，但文件结束了".into(),
                    });
                }
                Ok(self.chars[start..self.pos].iter().collect())
            }
        }
    }

    // ---- 值 ----

    fn value(&mut self) -> Result2<Value> {
        if self.depth >= 32 || self.items >= 4096 {
            return Err("配置嵌套或条目数超过限制".into());
        }
        self.depth += 1;
        self.items += 1;
        let result = self.value_inner();
        self.depth -= 1;
        result
    }

    fn value_inner(&mut self) -> Result2<Value> {
        match self.peek() {
            Some('"') => Ok(Value::String(self.basic_string()?)),
            Some('\'') => Ok(Value::String(self.literal_string()?)),
            Some('[') => self.array(),
            Some('{') => self.inline_table(),
            Some('t' | 'f') if self.starts_with("true") || self.starts_with("false") => {
                let value = self.starts_with("true");
                self.pos += if value { 4 } else { 5 };
                Ok(Value::Boolean(value))
            }
            Some(c) if c.is_ascii_digit() || matches!(c, '+' | '-') => self.number(),
            Some('i' | 'n') if self.starts_with("inf") || self.starts_with("nan") => self.number(),
            Some('\n' | '\r' | '#') | None => Err("等号后面缺少值".into()),
            Some(c) => Err(format!("无法识别的值，以“{c}”开头（字符串要加引号）")),
        }
    }

    fn basic_string(&mut self) -> Result2<String> {
        if self.starts_with("\"\"\"") {
            return Err("还不支持多行字符串 \"\"\"...\"\"\"".into());
        }
        self.pos += 1;
        let mut s = String::new();
        loop {
            let c = self.next().ok_or("字符串没有结束（缺少右引号）")?;
            match c {
                '"' => return Ok(s),
                '\n' => return Err("字符串没有结束（缺少右引号）".into()),
                '\\' => {
                    let escaped = self.next().ok_or("字符串没有结束")?;
                    s.push(match escaped {
                        'b' => '\u{8}',
                        't' => '\t',
                        'n' => '\n',
                        'f' => '\u{c}',
                        'r' => '\r',
                        '"' => '"',
                        '\\' => '\\',
                        'u' => self.unicode_escape(4)?,
                        'U' => self.unicode_escape(8)?,
                        other => return Err(format!("不认识的转义 \\{other}")),
                    });
                }
                c if c.is_control() && c != '\t' => return Err("字符串里不能直接出现控制字符".into()),
                c => s.push(c),
            }
        }
    }

    fn unicode_escape(&mut self, digits: usize) -> Result2<char> {
        let hex: String = (0..digits).filter_map(|_| self.next()).collect();
        u32::from_str_radix(&hex, 16)
            .ok()
            .filter(|_| hex.len() == digits)
            .and_then(char::from_u32)
            .ok_or_else(|| format!("无效的 Unicode 转义 \\u{hex}"))
    }

    /// 单引号字符串：内容原样保留，没有转义（适合写 Windows 路径）
    fn literal_string(&mut self) -> Result2<String> {
        if self.starts_with("'''") {
            return Err("还不支持多行字符串 '''...'''".into());
        }
        self.pos += 1;
        let mut s = String::new();
        loop {
            match self.next() {
                Some('\'') => return Ok(s),
                Some('\n') | None => return Err("字符串没有结束（缺少右引号）".into()),
                Some(c) => s.push(c),
            }
        }
    }

    fn number(&mut self) -> Result2<Value> {
        let start = self.pos;
        while matches!(self.peek(), Some(c) if c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '.')) {
            self.pos += 1;
        }
        let raw: String = self.chars[start..self.pos].iter().collect();
        if raw.contains("__") || raw.starts_with('_') || raw.ends_with('_') {
            return Err(format!("数字“{raw}”里的下划线位置不对"));
        }
        let text = raw.replace('_', "");
        let (sign, body) = match text.strip_prefix('-') {
            Some(rest) => (-1, rest),
            None => (1, text.strip_prefix('+').unwrap_or(&text)),
        };
        let invalid = || format!("无效的数字“{raw}”");

        match body {
            "inf" => return Ok(Value::Float(sign as f64 * f64::INFINITY)),
            "nan" => return Ok(Value::Float(f64::NAN)),
            _ => {}
        }
        for (prefix, radix) in [("0x", 16), ("0o", 8), ("0b", 2)] {
            if let Some(digits) = body.strip_prefix(prefix) {
                if sign < 0 || text.starts_with('+') {
                    return Err(format!("{prefix} 开头的数字不能带符号"));
                }
                return i64::from_str_radix(digits, radix).map(Value::Integer).map_err(|_| invalid());
            }
        }
        if body.contains(['.', 'e', 'E']) {
            // TOML 要求小数点两边都有数字
            if body.starts_with('.') || body.ends_with('.') || body.contains(".e") || body.contains(".E") {
                return Err(invalid());
            }
            return body.parse::<f64>().map(|f| Value::Float(sign as f64 * f)).map_err(|_| invalid());
        }
        if body.len() > 1 && body.starts_with('0') {
            return Err(format!("整数“{raw}”不能有前导零"));
        }
        text.parse::<i64>().map(Value::Integer).map_err(|_| invalid())
    }

    fn array(&mut self) -> Result2<Value> {
        self.pos += 1;
        let mut items = Vec::new();
        loop {
            self.skip_blank_lines();
            if self.peek() == Some(']') {
                self.pos += 1;
                return Ok(Value::Array(items));
            }
            items.push(self.value()?);
            self.skip_blank_lines();
            match self.next() {
                Some(',') => {}
                Some(']') => return Ok(Value::Array(items)),
                Some(c) => return Err(format!("数组元素之间要用逗号分隔，遇到了“{c}”")),
                None => return Err("数组没有结束（缺少 ]）".into()),
            }
        }
    }

    /// 内联表：{ a = 1, b = "x" }，按规范不能跨行
    fn inline_table(&mut self) -> Result2<Value> {
        self.pos += 1;
        let mut table = Table::default();
        self.skip_spaces();
        if self.peek() == Some('}') {
            self.pos += 1;
            return Ok(Value::Table(table));
        }
        loop {
            self.skip_spaces();
            if matches!(self.peek(), Some('\n' | '\r')) {
                return Err("内联表 {...} 不能跨行".into());
            }
            let key = self.key()?;
            self.skip_spaces();
            self.expect('=')?;
            self.skip_spaces();
            let value = self.value()?;
            let (last, parents) = key.split_last().expect("键至少有一段");
            table.subtable(parents)?.insert(last.clone(), value)?;
            self.skip_spaces();
            match self.next() {
                Some(',') => {}
                Some('}') => return Ok(Value::Table(table)),
                Some('\n') => return Err("内联表 {...} 不能跨行".into()),
                Some(c) => return Err(format!("内联表的元素之间要用逗号分隔，遇到了“{c}”")),
                None => return Err("内联表没有结束（缺少 }）".into()),
            }
        }
    }

    // ---- 空白、注释、行尾 ----

    fn skip_spaces(&mut self) {
        while matches!(self.peek(), Some(' ' | '\t')) {
            self.pos += 1;
        }
    }

    fn skip_comment(&mut self) {
        if self.peek() == Some('#') {
            while !matches!(self.peek(), Some('\n') | None) {
                self.pos += 1;
            }
        }
    }

    /// 跳过空格、注释和换行（数组内部、文件里的空行）
    fn skip_blank_lines(&mut self) {
        loop {
            self.skip_spaces();
            self.skip_comment();
            match self.peek() {
                Some('\n') => {
                    self.next();
                }
                Some('\r') if self.chars.get(self.pos + 1) == Some(&'\n') => {
                    self.pos += 1;
                }
                _ => return,
            }
        }
    }

    /// 一个键值对或表头之后，这一行只能剩下空格和注释
    fn end_of_line(&mut self) -> Result2<()> {
        self.skip_spaces();
        self.skip_comment();
        if self.peek() == Some('\r') {
            self.pos += 1;
        }
        match self.peek() {
            None => Ok(()),
            Some('\n') => {
                self.next();
                Ok(())
            }
            Some(c) => Err(format!("这一行后面多了内容“{c}”（一行只能写一个设置）")),
        }
    }

    fn expect(&mut self, wanted: char) -> Result2<()> {
        match self.next() {
            Some(c) if c == wanted => Ok(()),
            Some(c) => Err(format!("这里应该是“{wanted}”，但遇到了“{c}”")),
            None => Err(format!("这里应该是“{wanted}”，但文件结束了")),
        }
    }

    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn next(&mut self) -> Option<char> {
        let c = self.peek()?;
        self.pos += 1;
        if c == '\n' {
            self.line += 1;
        }
        Some(c)
    }

    fn starts_with(&self, s: &str) -> bool {
        s.chars().enumerate().all(|(i, c)| self.chars.get(self.pos + i) == Some(&c))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn get<'a>(table: &'a Table, path: &str) -> &'a Value {
        let mut value = None;
        let mut current = table;
        for part in path.split('.') {
            let v = current.get(part).unwrap_or_else(|| panic!("缺少 {path}"));
            if let Value::Table(t) = v {
                current = t;
            }
            value = Some(v);
        }
        value.unwrap()
    }

    fn string(s: &str) -> Value {
        Value::String(s.into())
    }

    #[test]
    fn basics() {
        let t = parse(
            r#"
# 注释
title = "终端"   # 行尾注释
count = 42
negative = -7
ratio = 1.5
big = 1_000
hex = 0xff
yes = true
no = false

[font]
family = ["Maple Mono NF CN", 'PingFang SC']
size = 15
"#,
        )
        .unwrap();
        assert_eq!(get(&t, "title"), &string("终端"));
        assert_eq!(get(&t, "count"), &Value::Integer(42));
        assert_eq!(get(&t, "negative"), &Value::Integer(-7));
        assert_eq!(get(&t, "ratio"), &Value::Float(1.5));
        assert_eq!(get(&t, "big"), &Value::Integer(1000));
        assert_eq!(get(&t, "hex"), &Value::Integer(255));
        assert_eq!(get(&t, "yes"), &Value::Boolean(true));
        assert_eq!(get(&t, "no"), &Value::Boolean(false));
        assert_eq!(get(&t, "font.family"), &Value::Array(vec![string("Maple Mono NF CN"), string("PingFang SC")]));
        assert_eq!(get(&t, "font.size"), &Value::Integer(15));
    }

    #[test]
    fn string_escapes() {
        let t = parse(r#"a = "tab\tquote\"back\\slash\u4e2d\U0001F980""#).unwrap();
        assert_eq!(get(&t, "a"), &string("tab\tquote\"back\\slash中🦀"));
        // 单引号字符串不处理转义，适合写 Windows 路径
        let t = parse(r"path = 'C:\Users\me'").unwrap();
        assert_eq!(get(&t, "path"), &string(r"C:\Users\me"));
    }

    #[test]
    fn dotted_and_quoted_keys() {
        let t = parse("a.b.c = 1\n\"key with space\" = 2\n[x.y]\nz = 3\n").unwrap();
        assert_eq!(get(&t, "a.b.c"), &Value::Integer(1));
        assert_eq!(get(&t, "key with space"), &Value::Integer(2));
        assert_eq!(get(&t, "x.y.z"), &Value::Integer(3));
    }

    #[test]
    fn multiline_array_and_inline_table() {
        let t = parse(
            "colors = [\n  \"#000000\", # 黑\n  \"#ffffff\",\n]\nenv = { TERM = \"xterm\", LANG = \"zh_CN.UTF-8\" }\n",
        )
        .unwrap();
        assert_eq!(get(&t, "colors"), &Value::Array(vec![string("#000000"), string("#ffffff")]));
        assert_eq!(get(&t, "env.TERM"), &string("xterm"));
        assert_eq!(get(&t, "env.LANG"), &string("zh_CN.UTF-8"));
    }

    #[test]
    fn floats() {
        let t = parse("a = 1e3\nb = -2.5E-1\nc = inf\nd = -inf\n").unwrap();
        assert_eq!(get(&t, "a"), &Value::Float(1000.0));
        assert_eq!(get(&t, "b"), &Value::Float(-0.25));
        assert_eq!(get(&t, "c"), &Value::Float(f64::INFINITY));
        assert_eq!(get(&t, "d"), &Value::Float(f64::NEG_INFINITY));
    }

    #[test]
    fn byte_order_mark() {
        let t = parse("\u{feff}a = 1\n").unwrap();
        assert_eq!(get(&t, "a"), &Value::Integer(1));
    }

    #[test]
    fn windows_line_endings() {
        let t = parse("a = 1\r\n[b]\r\nc = 'x'\r\n").unwrap();
        assert_eq!(get(&t, "b.c"), &string("x"));
    }

    #[test]
    fn errors_have_line_numbers() {
        let err = |text: &str| parse(text).unwrap_err();
        assert_eq!(err("a = 1\nb = \n").line, 2);
        assert_eq!(err("a = 1\na = 2\n"), Error { line: 2, message: "键“a”重复定义".into() });
        assert_eq!(err("[t]\n[t]\n").line, 2);
        assert!(err("a = hello").message.contains("加引号"));
        assert!(err("a = no").message.contains("加引号"));
        assert_eq!(err("a =\n").message, "等号后面缺少值");
        assert!(err("a = \"unterminated\n").message.contains("没有结束"));
        assert!(err("a = 1 b = 2").message.contains("多了内容"));
        assert!(err("[[bin]]").message.contains("表数组"));
        assert!(err("a = 007").message.contains("前导零"));
        assert!(err("a = [1, 2").message.contains("没有结束"));
        assert!(err("a = 1\nb = { c = 1,\n d = 2 }").message.contains("不能跨行"));
    }

    #[test]
    fn value_then_table_conflict() {
        let e = parse("a = 1\n[a]\nb = 2\n").unwrap_err();
        assert!(e.message.contains("不能再当作表"));
    }
}

#[cfg(test)]
mod boundary_tests {
    use super::*;
    #[test]
    fn rejects_deep_values_paths_and_oversized_files() {
        let nested = format!("x = {}0{}", "[".repeat(10000), "]".repeat(10000));
        assert!(parse(&nested).is_err());
        assert!(parse(&format!("{}x = 0", "a.".repeat(10000))).is_err());
        assert!(parse(&" ".repeat(MAX_CONFIG_BYTES + 1)).is_err());
        let mut many = String::new();
        for i in 0..4097 {
            many.push_str(&format!("x{i}=1\n"));
        }
        assert!(parse(&many).is_err());
    }
}
