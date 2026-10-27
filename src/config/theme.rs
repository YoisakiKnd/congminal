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
    /// 主题预览按这个顺序轮换，配置里的名字也用 `name`。
    pub const ALL: [Self; 3] = [Self::Dark, Self::Light, Self::Midnight];
    pub fn name(self) -> &'static str {
        match self {
            Self::Dark => "dark",
            Self::Light => "light",
            Self::Midnight => "midnight",
        }
    }
    /// 预览条上显示的中文名
    pub fn label(self) -> &'static str {
        match self {
            Self::Dark => "深色",
            Self::Light => "浅色",
            Self::Midnight => "午夜",
        }
    }
    pub fn index(self) -> usize {
        Self::ALL.iter().position(|theme| *theme == self).unwrap_or(0)
    }
    pub fn at(index: usize) -> Self {
        Self::ALL[index % Self::ALL.len()]
    }
    pub fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|theme| theme.name() == name)
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn every_theme_round_trips_through_name_index_and_preview_order() {
        assert_eq!(Theme::ALL.len(), 3);
        for (index, theme) in Theme::ALL.into_iter().enumerate() {
            assert_eq!(theme.index(), index);
            assert_eq!(Theme::at(index), theme);
            assert_eq!(Theme::parse(theme.name()), Some(theme));
            assert!(!theme.label().is_empty());
        }
        // 预览按 ALL 顺序循环，越界也回到合法主题
        assert_eq!(Theme::at(Theme::ALL.len()), Theme::ALL[0]);
        assert_eq!(Theme::at(Theme::ALL.len() + 1), Theme::ALL[1]);
        assert_eq!(Theme::parse("Solarized"), None);
        // 名字互不重复，否则 parse 会挑错
        let mut names: Vec<_> = Theme::ALL.into_iter().map(Theme::name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Theme::ALL.len());
    }
}
