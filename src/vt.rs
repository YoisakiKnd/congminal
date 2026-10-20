//! VT/ANSI 转义序列解析器
//!
//! 按照 Paul Williams 的 DEC 兼容解析器状态机实现：<https://vt100.net/emu/dec_ansi_parser>
//!
//! 解析器只负责"断句"：把字节流切成可打印字符、C0 控制字符、ESC/CSI/OSC/DCS 序列，
//! 再交给 [`Perform`] 的实现者解释含义。它不知道光标、颜色这些概念。

const MAX_PARAMS: usize = 32;
const MAX_INTERMEDIATES: usize = 2;
const MAX_OSC_LEN: usize = 4096;
const MAX_OSC_PARAMS: usize = 16;

/// 解析器回调。终端状态机实现这个 trait。
pub trait Perform {
    fn begin_batch(&mut self) {}
    fn osc_overflow(&mut self) {}
    /// 打印一个字符
    fn print(&mut self, c: char);
    /// 执行 C0 控制字符（换行、回车、退格……）
    fn execute(&mut self, byte: u8);
    /// `ESC [ 参数 中间字节 结束字节`，例如 `ESC [ 1 ; 2 H`
    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8);
    /// `ESC 中间字节 结束字节`，例如 `ESC ( 0`
    fn esc_dispatch(&mut self, intermediates: &[u8], ignore: bool, byte: u8);
    /// `ESC ] 参数;参数... BEL/ST`，例如设置窗口标题
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    /// DCS 序列开始
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: u8) {}
    /// DCS 序列中的数据字节
    fn put(&mut self, _byte: u8) {}
    /// DCS 序列结束
    fn unhook(&mut self) {}
}

/// CSI/DCS 的数字参数。
///
/// 参数之间用 `;` 分隔，一个参数内部还可以用 `:` 分出子参数（如 `38:2::255:0:0`）。
/// [`Params::iter`] 每次返回一组：主参数加上它的子参数。缺省的参数记为 0。
#[derive(Default)]
pub struct Params {
    values: [u16; MAX_PARAMS],
    is_sub: [bool; MAX_PARAMS],
    len: usize,
}

impl Params {
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter { params: self, pos: 0 }
    }

    fn clear(&mut self) {
        self.len = 0;
    }

    fn push(&mut self, value: u16, is_sub: bool) -> bool {
        if self.len == MAX_PARAMS {
            return false;
        }
        self.values[self.len] = value;
        self.is_sub[self.len] = is_sub;
        self.len += 1;
        true
    }
}

pub struct ParamsIter<'a> {
    params: &'a Params,
    pos: usize,
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];

    fn next(&mut self) -> Option<&'a [u16]> {
        let p = self.params;
        if self.pos >= p.len {
            return None;
        }
        let start = self.pos;
        self.pos += 1;
        while self.pos < p.len && p.is_sub[self.pos] {
            self.pos += 1;
        }
        Some(&p.values[start..self.pos])
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    OscString,
    SosPmApcString,
}

pub struct Parser {
    state: State,
    params: Params,
    /// 正在累积的参数值
    param: u16,
    /// 正在累积的参数是不是子参数（前面是 `:`）
    param_is_sub: bool,
    /// 当前序列里是否出现过参数字节
    param_started: bool,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_len: usize,
    /// 参数或中间字节溢出时置位，序列仍会分发，但会带上 ignore 标记
    ignoring: bool,
    osc: Vec<u8>,
    osc_overflow: bool,
    utf8: Utf8Decoder,
}

impl Default for Parser {
    fn default() -> Self {
        Self::new()
    }
}

impl Parser {
    pub fn new() -> Parser {
        Parser {
            state: State::Ground,
            params: Params::default(),
            param: 0,
            param_is_sub: false,
            param_started: false,
            intermediates: [0; MAX_INTERMEDIATES],
            intermediate_len: 0,
            ignoring: false,
            osc: Vec::new(),
            osc_overflow: false,
            utf8: Utf8Decoder::default(),
        }
    }

    pub fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        if !bytes.is_empty() {
            performer.begin_batch();
        }
        for &b in bytes {
            self.byte(performer, b);
        }
    }

    fn byte<P: Perform>(&mut self, p: &mut P, b: u8) {
        // Ground 状态下，0x80 以上的字节属于 UTF-8 多字节序列
        if self.state == State::Ground && (b >= 0x80 || self.utf8.in_progress()) {
            match self.utf8.push(b) {
                Utf8::Pending => {}
                Utf8::Char(c) => p.print(c),
                Utf8::Invalid { reprocess } => {
                    p.print('\u{FFFD}');
                    if reprocess {
                        // 这个字节打断了上一个序列，它自己还要按正常流程处理一次
                        self.byte(p, b);
                    }
                }
            }
            return;
        }

        // "anywhere" 转移：任何状态下遇到这几个字节都会跳转
        match b {
            0x18 | 0x1A => {
                if self.state == State::OscString {
                    self.osc_overflow = true;
                }
                self.exit_string_state(p);
                p.execute(b);
                self.state = State::Ground;
                return;
            }
            0x1B => {
                self.exit_string_state(p);
                self.clear();
                self.state = State::Escape;
                return;
            }
            _ => {}
        }

        // 在非字符串状态里，8 位字节没有意义，直接丢弃
        if b >= 0x80 && !matches!(self.state, State::OscString | State::DcsPassthrough) {
            return;
        }

        match self.state {
            State::Ground => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x7E => p.print(b as char),
                _ => {}
            },

            State::Escape => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x2F => {
                    self.collect(b);
                    self.state = State::EscapeIntermediate;
                }
                b'[' => self.state = State::CsiEntry,
                b']' => {
                    self.osc.clear();
                    self.osc_overflow = false;
                    self.state = State::OscString;
                }
                b'P' => self.state = State::DcsEntry,
                b'X' | b'^' | b'_' => self.state = State::SosPmApcString,
                0x30..=0x7E => {
                    p.esc_dispatch(self.intermediates(), self.ignoring, b);
                    self.state = State::Ground;
                }
                _ => {}
            },

            State::EscapeIntermediate => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x2F => self.collect(b),
                0x30..=0x7E => {
                    p.esc_dispatch(self.intermediates(), self.ignoring, b);
                    self.state = State::Ground;
                }
                _ => {}
            },

            State::CsiEntry => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x2F => {
                    self.collect(b);
                    self.state = State::CsiIntermediate;
                }
                0x30..=0x3B => {
                    self.param_byte(b);
                    self.state = State::CsiParam;
                }
                // `<` `=` `>` `?` 是私有标记，比如 `ESC [ ? 25 h`
                0x3C..=0x3F => {
                    self.collect(b);
                    self.state = State::CsiParam;
                }
                0x40..=0x7E => self.csi_dispatch(p, b),
                _ => {}
            },

            State::CsiParam => match b {
                0x00..=0x1F => p.execute(b),
                0x30..=0x3B => self.param_byte(b),
                0x3C..=0x3F => self.state = State::CsiIgnore,
                0x20..=0x2F => {
                    self.collect(b);
                    self.state = State::CsiIntermediate;
                }
                0x40..=0x7E => self.csi_dispatch(p, b),
                _ => {}
            },

            State::CsiIntermediate => match b {
                0x00..=0x1F => p.execute(b),
                0x20..=0x2F => self.collect(b),
                0x30..=0x3F => self.state = State::CsiIgnore,
                0x40..=0x7E => self.csi_dispatch(p, b),
                _ => {}
            },

            State::CsiIgnore => match b {
                0x00..=0x1F => p.execute(b),
                0x40..=0x7E => self.state = State::Ground,
                _ => {}
            },

            State::DcsEntry => match b {
                0x20..=0x2F => {
                    self.collect(b);
                    self.state = State::DcsIntermediate;
                }
                0x30..=0x3B => {
                    self.param_byte(b);
                    self.state = State::DcsParam;
                }
                0x3C..=0x3F => {
                    self.collect(b);
                    self.state = State::DcsParam;
                }
                0x40..=0x7E => self.hook(p, b),
                _ => {}
            },

            State::DcsParam => match b {
                0x30..=0x3B => self.param_byte(b),
                0x3C..=0x3F => self.state = State::DcsIgnore,
                0x20..=0x2F => {
                    self.collect(b);
                    self.state = State::DcsIntermediate;
                }
                0x40..=0x7E => self.hook(p, b),
                _ => {}
            },

            State::DcsIntermediate => match b {
                0x20..=0x2F => self.collect(b),
                0x30..=0x3F => self.state = State::DcsIgnore,
                0x40..=0x7E => self.hook(p, b),
                _ => {}
            },

            State::DcsPassthrough => {
                if b != 0x7F {
                    p.put(b);
                }
            }

            State::OscString => match b {
                // xterm 扩展：OSC 也可以用 BEL 结束
                0x07 => {
                    self.osc_dispatch(p, true);
                    self.state = State::Ground;
                }
                0x00..=0x1F => self.osc_overflow = true,
                _ => {
                    if self.osc.len() < MAX_OSC_LEN {
                        self.osc.push(b);
                    } else {
                        self.osc_overflow = true;
                    }
                }
            },

            State::DcsIgnore | State::SosPmApcString => {}
        }
    }

    /// 离开 OSC / DCS 这类"字符串"状态时要做收尾（它们通常以 `ESC \` 结束）
    fn exit_string_state<P: Perform>(&mut self, p: &mut P) {
        match self.state {
            State::OscString => self.osc_dispatch(p, false),
            State::DcsPassthrough => p.unhook(),
            _ => {}
        }
    }

    fn clear(&mut self) {
        self.params.clear();
        self.param = 0;
        self.param_is_sub = false;
        self.param_started = false;
        self.intermediate_len = 0;
        self.ignoring = false;
    }

    fn collect(&mut self, b: u8) {
        if self.intermediate_len == MAX_INTERMEDIATES {
            self.ignoring = true;
        } else {
            self.intermediates[self.intermediate_len] = b;
            self.intermediate_len += 1;
        }
    }

    fn intermediates(&self) -> &[u8] {
        &self.intermediates[..self.intermediate_len]
    }

    fn param_byte(&mut self, b: u8) {
        self.param_started = true;
        match b {
            b'0'..=b'9' => {
                self.param = self.param.saturating_mul(10).saturating_add((b - b'0') as u16);
            }
            b';' | b':' => {
                if !self.params.push(self.param, self.param_is_sub) {
                    self.ignoring = true;
                }
                self.param = 0;
                self.param_is_sub = b == b':';
            }
            _ => {}
        }
    }

    fn finish_params(&mut self) {
        if self.param_started && !self.params.push(self.param, self.param_is_sub) {
            self.ignoring = true;
        }
    }

    fn csi_dispatch<P: Perform>(&mut self, p: &mut P, action: u8) {
        self.finish_params();
        p.csi_dispatch(&self.params, self.intermediates(), self.ignoring, action);
        self.state = State::Ground;
    }

    fn hook<P: Perform>(&mut self, p: &mut P, action: u8) {
        self.finish_params();
        p.hook(&self.params, self.intermediates(), self.ignoring, action);
        self.state = State::DcsPassthrough;
    }

    fn osc_dispatch<P: Perform>(&mut self, p: &mut P, bell_terminated: bool) {
        if self.osc_overflow {
            p.osc_overflow();
            return;
        }
        // 按 `;` 切分，最多切 MAX_OSC_PARAMS 段，最后一段保留剩下的全部内容
        let mut parts: [&[u8]; MAX_OSC_PARAMS] = [&[]; MAX_OSC_PARAMS];
        let mut count = 0;
        let mut rest = &self.osc[..];
        while count < MAX_OSC_PARAMS - 1 {
            match rest.iter().position(|&b| b == b';') {
                Some(i) => {
                    parts[count] = &rest[..i];
                    rest = &rest[i + 1..];
                    count += 1;
                }
                None => break,
            }
        }
        parts[count] = rest;
        count += 1;
        p.osc_dispatch(&parts[..count], bell_terminated);
    }
}

#[derive(Default)]
struct Utf8Decoder {
    codepoint: u32,
    /// 还需要几个后续字节
    needed: u8,
    /// 这个长度允许的最小码点，用来拒绝"超长编码"
    min: u32,
}

enum Utf8 {
    Pending,
    Char(char),
    /// 非法序列；`reprocess` 表示当前字节不属于这个序列，需要重新处理
    Invalid {
        reprocess: bool,
    },
}

impl Utf8Decoder {
    fn in_progress(&self) -> bool {
        self.needed > 0
    }

    fn push(&mut self, b: u8) -> Utf8 {
        if self.needed == 0 {
            let (needed, bits, min) = match b {
                0xC2..=0xDF => (1, b & 0x1F, 0x80),
                0xE0..=0xEF => (2, b & 0x0F, 0x800),
                0xF0..=0xF4 => (3, b & 0x07, 0x10000),
                _ => return Utf8::Invalid { reprocess: false },
            };
            self.needed = needed;
            self.codepoint = bits as u32;
            self.min = min;
            return Utf8::Pending;
        }

        if b & 0xC0 != 0x80 {
            self.needed = 0;
            return Utf8::Invalid { reprocess: true };
        }
        self.codepoint = (self.codepoint << 6) | (b & 0x3F) as u32;
        self.needed -= 1;
        if self.needed > 0 {
            return Utf8::Pending;
        }
        match char::from_u32(self.codepoint) {
            Some(c) if self.codepoint >= self.min => Utf8::Char(c),
            _ => Utf8::Invalid { reprocess: false },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default)]
    struct Log(Vec<String>);

    impl Perform for Log {
        fn print(&mut self, c: char) {
            self.0.push(format!("print {c}"));
        }
        fn execute(&mut self, byte: u8) {
            self.0.push(format!("exec {byte:02x}"));
        }
        fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], ignore: bool, action: u8) {
            let params: Vec<String> =
                params.iter().map(|g| g.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(":")).collect();
            self.0.push(format!(
                "csi {} [{}]{} {}",
                params.join(";"),
                String::from_utf8_lossy(intermediates),
                if ignore { " ignore" } else { "" },
                action as char
            ));
        }
        fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
            self.0.push(format!("esc [{}] {}", String::from_utf8_lossy(intermediates), byte as char));
        }
        fn osc_dispatch(&mut self, params: &[&[u8]], bell: bool) {
            let params: Vec<_> = params.iter().map(|p| String::from_utf8_lossy(p)).collect();
            self.0.push(format!("osc {} bell={bell}", params.join("|")));
        }
    }

    fn parse(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = Parser::new();
        let mut log = Log::default();
        for chunk in chunks {
            parser.advance(&mut log, chunk);
        }
        log.0
    }

    #[test]
    fn plain_text_and_controls() {
        assert_eq!(parse(&[b"a\r\n"]), ["print a", "exec 0d", "exec 0a"]);
    }

    #[test]
    fn utf8() {
        assert_eq!(parse(&["你好".as_bytes()]), ["print 你", "print 好"]);
        // 多字节字符被拆到两次 read 里也要能拼回来
        let bytes = "é".as_bytes();
        assert_eq!(parse(&[&bytes[..1], &bytes[1..]]), ["print é"]);
    }

    #[test]
    fn invalid_utf8() {
        assert_eq!(parse(&[b"\xffA"]), ["print \u{FFFD}", "print A"]);
        // 序列被 ASCII 打断：先输出替换字符，再正常处理被打断的字节
        assert_eq!(parse(&[b"\xe4\xbdA"]), ["print \u{FFFD}", "print A"]);
        // 超长编码
        assert_eq!(parse(&[b"\xe0\x80\x80"]), ["print \u{FFFD}"]);
    }

    #[test]
    fn csi() {
        assert_eq!(parse(&[b"\x1b[1;2H"]), ["csi 1;2 [] H"]);
        assert_eq!(parse(&[b"\x1b[H"]), ["csi  [] H"]);
        assert_eq!(parse(&[b"\x1b[;5H"]), ["csi 0;5 [] H"]);
        assert_eq!(parse(&[b"\x1b[?25l"]), ["csi 25 [?] l"]);
        assert_eq!(parse(&[b"\x1b[2 q"]), ["csi 2 [ ] q"]);
        assert_eq!(parse(&[b"\x1b[38:2::10:20:30m"]), ["csi 38:2:0:10:20:30 [] m"]);
        // 序列被拆开
        assert_eq!(parse(&[b"\x1b[3", b"8;5;1", b"m"]), ["csi 38;5;1 [] m"]);
    }

    #[test]
    fn csi_with_embedded_control() {
        // C0 控制字符可以出现在 CSI 序列中间，会被立即执行
        assert_eq!(parse(&[b"\x1b[1\n2H"]), ["exec 0a", "csi 12 [] H"]);
    }

    #[test]
    fn csi_cancelled() {
        assert_eq!(parse(&[b"\x1b[12\x18A"]), ["exec 18", "print A"]);
    }

    #[test]
    fn too_many_params() {
        let mut seq = b"\x1b[".to_vec();
        for _ in 0..40 {
            seq.extend_from_slice(b"1;");
        }
        seq.push(b'm');
        assert!(parse(&[&seq])[0].ends_with(" ignore m"));
    }

    #[test]
    fn esc() {
        assert_eq!(parse(&[b"\x1b7\x1b(0"]), ["esc [] 7", "esc [(] 0"]);
    }

    #[test]
    fn osc() {
        assert_eq!(parse(&[b"\x1b]0;hi;there\x07"]), ["osc 0|hi|there bell=true"]);
        // 以 ST (ESC \) 结束；ST 本身会作为一个 ESC 序列分发出去
        assert_eq!(parse(&["\x1b]2;标题\x1b\\".as_bytes()]), ["osc 2|标题 bell=false", "esc [] \\"]);
    }

    #[test]
    fn dcs_is_skipped_cleanly() {
        assert_eq!(parse(&[b"\x1bPq#0;2;0;0;0\x1b\\x"]), ["esc [] \\", "print x"]);
    }
}
