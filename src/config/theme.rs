//! 主题只决定缺省值；用户显式设置的颜色在切换主题时继续覆盖。
use super::Colors;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Theme {
    #[default]
    Dark,
    Light,
    Midnight,
}
impl Theme {
    pub fn parse(name: &str) -> Option<Self> {
        match name {
            "dark" => Some(Self::Dark),
            "light" => Some(Self::Light),
            "midnight" => Some(Self::Midnight),
            _ => None,
        }
    }
    pub fn next(self) -> Self {
        match self {
            Self::Dark => Self::Light,
            Self::Light => Self::Midnight,
            Self::Midnight => Self::Dark,
        }
    }
    pub fn colors(self) -> Colors {
        let mut colors = Colors::default();
        let (foreground, background, cursor, cursor_text, selection, preedit, palette) = match self {
            Self::Dark => return colors,
            Self::Light => (
                0x30343b,
                0xfafaf7,
                0x3659b8,
                0xffffff,
                0xcbd9f1,
                0xe2e7ef,
                [
                    0x24272e, 0xb42d35, 0x287a3e, 0x906118, 0x315bb5, 0x8547a4, 0x16777b, 0xd9dce3, 0x626977, 0xd64b54,
                    0x398a50, 0xa27122, 0x4675d1, 0xa160bc, 0x258b90, 0xffffff,
                ],
            ),
            Self::Midnight => (
                0xd7e1f2,
                0x111827,
                0x8cb4ff,
                0x111827,
                0x30476a,
                0x24334c,
                [
                    0x182030, 0xf07883, 0x91c99a, 0xe3c77b, 0x83aaff, 0xc39bea, 0x7bcad6, 0xd7e1f2, 0x6a7c99, 0xff9ca6,
                    0xaddfb3, 0xf3dc96, 0xa8c6ff, 0xd9b8f5, 0x9ce0e9, 0xf4f7ff,
                ],
            ),
        };
        colors.foreground = foreground;
        colors.background = background;
        colors.cursor = cursor;
        colors.cursor_text = cursor_text;
        colors.selection_background = selection;
        colors.preedit_background = preedit;
        colors.palette[..16].copy_from_slice(&palette);
        colors
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Overrides {
    pub foreground: Option<u32>,
    pub background: Option<u32>,
    pub cursor: Option<u32>,
    pub cursor_text: Option<u32>,
    pub selection_background: Option<u32>,
    pub preedit_background: Option<u32>,
    pub bold_is_bright: Option<bool>,
    pub palette: Vec<(usize, u32)>,
}
impl Overrides {
    pub fn apply(&self, colors: &mut Colors) {
        for (value, target) in [
            (self.foreground, &mut colors.foreground),
            (self.background, &mut colors.background),
            (self.cursor, &mut colors.cursor),
            (self.cursor_text, &mut colors.cursor_text),
            (self.selection_background, &mut colors.selection_background),
            (self.preedit_background, &mut colors.preedit_background),
        ] {
            if let Some(value) = value {
                *target = value;
            }
        }
        if let Some(value) = self.bold_is_bright {
            colors.bold_is_bright = value;
        }
        for &(index, value) in &self.palette {
            colors.palette[index] = value;
        }
    }
}
