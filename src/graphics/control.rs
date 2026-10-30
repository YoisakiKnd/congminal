#[derive(Clone, Copy, Debug)]
pub(super) struct Control {
    pub action: u8,
    pub format: u32,
    pub transport: u8,
    pub compression: u8,
    pub id: u32,
    pub placement: u32,
    pub width: u32,
    pub height: u32,
    pub columns: u32,
    pub rows: u32,
    pub virtual_: bool,
    pub more: bool,
    pub quiet: u32,
    pub crop: [u32; 4],
    pub offset: [u32; 2],
    pub z: i32,
    pub no_move: bool,
    pub delete: u8,
    pub continuation: bool,
}
impl Default for Control {
    fn default() -> Self {
        Self {
            action: b't',
            format: 32,
            transport: b'd',
            compression: 0,
            id: 0,
            placement: 0,
            width: 0,
            height: 0,
            columns: 0,
            rows: 0,
            virtual_: false,
            more: false,
            quiet: 0,
            crop: [0; 4],
            offset: [0; 2],
            z: 0,
            no_move: false,
            delete: b'a',
            continuation: true,
        }
    }
}
impl Control {
    pub fn parse(bytes: &[u8]) -> Result<Self, Self> {
        let mut c = Self::default();
        let mut seen = 0u64;
        let mut invalid = false;
        for part in bytes.split(|b| *b == b',').filter(|p| !p.is_empty()) {
            if part.len() < 3 || part[1] != b'=' {
                invalid = true;
                continue;
            }
            let key = part[0];
            let value = &part[2..];
            let bit = match key {
                b'A'..=b'Z' => key - b'A',
                b'a'..=b'z' => key - b'a' + 26,
                _ => {
                    invalid = true;
                    continue;
                }
            };
            if seen & (1u64 << bit) != 0 {
                invalid = true;
            }
            seen |= 1u64 << bit;
            if !matches!(key, b'm' | b'q') {
                c.continuation = false;
            }
            let ch = || if value.len() == 1 { Some(value[0]) } else { None };
            let number = || std::str::from_utf8(value).ok().and_then(|v| v.parse::<u32>().ok());
            match key {
                b'a' => {
                    if let Some(v) = ch() {
                        c.action = v
                    } else {
                        invalid = true;
                    }
                }
                b't' => {
                    if let Some(v) = ch() {
                        c.transport = v
                    } else {
                        invalid = true;
                    }
                }
                b'o' => {
                    if let Some(v) = ch() {
                        c.compression = v
                    } else {
                        invalid = true;
                    }
                }
                b'd' => {
                    if let Some(v) = ch() {
                        c.delete = v
                    } else {
                        invalid = true;
                    }
                }
                b'z' => {
                    if let Some(v) = std::str::from_utf8(value).ok().and_then(|v| v.parse::<i32>().ok()) {
                        c.z = v
                    } else {
                        invalid = true;
                    }
                }
                _ => {
                    if let Some(v) = number() {
                        match key {
                            b'f' => c.format = v,
                            b'i' => c.id = v,
                            b'p' => c.placement = v,
                            b's' => c.width = v,
                            b'v' => c.height = v,
                            b'c' => c.columns = v,
                            b'r' => c.rows = v,
                            b'm' if v <= 1 => c.more = v == 1,
                            b'q' if v <= 2 => c.quiet = v,
                            b'U' if v <= 1 => c.virtual_ = v == 1,
                            b'C' if v <= 1 => c.no_move = v == 1,
                            b'x' => c.crop[0] = v,
                            b'y' => c.crop[1] = v,
                            b'w' => c.crop[2] = v,
                            b'h' => c.crop[3] = v,
                            b'X' => c.offset[0] = v,
                            b'Y' => c.offset[1] = v,
                            _ => invalid = true,
                        }
                    } else {
                        invalid = true;
                    }
                }
            }
        }
        if invalid { Err(c) } else { Ok(c) }
    }
}
