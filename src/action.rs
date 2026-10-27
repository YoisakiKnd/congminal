//! 界面动作和有界快捷键配置。按键查找不分配；平台提交文本与按键编码分开。
use crate::config::toml::{Table, Value};
use crate::hash::FnvHashMap;
use crate::input::{Key, Mods};

/// 终端、搜索与平台输入法共用焦点入口，后续侧栏继续在此接入。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusTarget {
    Terminal,
    Composition,
    Search,
    Inactive,
}
#[derive(Clone, Copy, Debug)]
pub struct Focus {
    pub active: bool,
}
impl Focus {
    pub fn target(self, preedit: &str) -> FocusTarget {
        if !self.active {
            FocusTarget::Inactive
        } else if !preedit.is_empty() {
            FocusTarget::Composition
        } else {
            FocusTarget::Terminal
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Copy,
    Paste,
    PageUp,
    PageDown,
    HistoryTop,
    HistoryBottom,
    FontIncrease,
    FontDecrease,
    FontReset,
    NextTheme,
    /// 回到配置里写的主题（轮换按多了以后复位）
    ResetTheme,
    ShowResources,
    Search,
    SearchNext,
    SearchPrevious,
    OpenLink,
    CopyLink,
    CommandPrevious,
    CommandNext,
    SelectCommandOutput,
    CopyCommandOutput,
    ShowShellStatus,
    ShowNotifications,
    ToggleNotifications,
    /// 清空通知记录；默认不绑定按键，按名字 `clear-notifications` 配置。
    ClearNotifications,
    NewTab,
    ClosePane,
    NextTab,
    PreviousTab,
    MoveTabLeft,
    MoveTabRight,
    SplitRight,
    SplitDown,
    NextPane,
    PreviousPane,
    ToggleLeftSidebar,
    ToggleRightSidebar,
    ShowGitPanel,
    ShowMonitorPanel,
    SendText(String),
    Ignore,
    PassThrough,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct Chord {
    key: Key,
    mods: Mods,
}
impl Chord {
    fn normalized(key: Key, mut mods: Mods) -> Self {
        let key = match key {
            Key::Char(c) => {
                let c = if mods.ctrl {
                    match c {
                        ')' if mods.shift => '0',
                        '\x01'..='\x1a' => ((c as u8 - 1) + b'a') as char,
                        _ => c,
                    }
                } else {
                    c
                };
                let c = match c {
                    '+' => {
                        mods.shift = true;
                        '='
                    }
                    '_' => {
                        mods.shift = true;
                        '-'
                    }
                    c => c.to_ascii_lowercase(),
                };
                Key::Char(c)
            }
            key => key,
        };
        Self { key, mods }
    }
    fn parse(text: &str) -> Result<Self, String> {
        if text.len() > 64 {
            return Err("快捷键名称过长".into());
        }
        let parts: Vec<_> = text.split('+').collect();
        let (key, modifiers) = parts.split_last().ok_or("缺少按键")?;
        let mut mods = Mods::default();
        for modifier in modifiers {
            match modifier.to_ascii_lowercase().as_str() {
                "ctrl" | "control" if !mods.ctrl => mods.ctrl = true,
                "shift" if !mods.shift => mods.shift = true,
                "alt" | "option" if !mods.alt => mods.alt = true,
                "cmd" | "command" | "super" | "logo" if !mods.logo => mods.logo = true,
                "primary" => {
                    if cfg!(target_os = "macos") {
                        mods.logo = true;
                    } else {
                        mods.ctrl = true;
                        mods.shift = true;
                    }
                }
                _ => return Err(format!("修饰键“{modifier}”未知或重复")),
            }
        }
        let key = match key.to_ascii_lowercase().as_str() {
            "enter" => Key::Enter,
            "escape" | "esc" => Key::Escape,
            "tab" => Key::Tab,
            "backspace" => Key::Backspace,
            "up" => Key::Up,
            "down" => Key::Down,
            "left" => Key::Left,
            "right" => Key::Right,
            "home" => Key::Home,
            "end" => Key::End,
            "pageup" => Key::PageUp,
            "pagedown" => Key::PageDown,
            "insert" => Key::Insert,
            "delete" => Key::Delete,
            "space" => Key::Char(' '),
            "plus" => {
                mods.shift = true;
                Key::Char('=')
            }
            "equal" => Key::Char('='),
            "minus" => Key::Char('-'),
            name if name.starts_with('f') && name[1..].parse::<u8>().is_ok_and(|n| (1..=12).contains(&n)) => {
                Key::F(name[1..].parse().unwrap())
            }
            name if name.chars().count() == 1 => Key::Char(name.chars().next().unwrap()),
            _ => return Err(format!("不认识的按键“{key}”")),
        };
        let chord = Self::normalized(key, mods);
        if (cfg!(target_os = "macos") && chord.key == Key::Char('q') && chord.mods.logo)
            || (!cfg!(target_os = "macos") && chord.key == Key::F(4) && chord.mods.alt)
        {
            return Err("该快捷键由系统/应用菜单保留".into());
        }
        Ok(chord)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Bindings(FnvHashMap<Chord, Action>);
impl Default for Bindings {
    fn default() -> Self {
        let mut bindings = Self(FnvHashMap::default());
        for (chord, action) in [
            ("Shift+PageUp", Action::PageUp),
            ("Shift+PageDown", Action::PageDown),
            ("Shift+Home", Action::HistoryTop),
            ("Shift+End", Action::HistoryBottom),
            ("Ctrl+Shift+C", Action::Copy),
            ("Ctrl+Shift+V", Action::Paste),
            ("Shift+Insert", Action::Paste),
            ("Primary+C", Action::Copy),
            ("Primary+V", Action::Paste),
            ("Primary+Equal", Action::FontIncrease),
            ("Primary+Plus", Action::FontIncrease),
            ("Primary+Minus", Action::FontDecrease),
            ("Primary+0", Action::FontReset),
            ("Primary+F6", Action::NextTheme),
            ("Primary+F12", Action::ShowResources),
            ("Primary+T", Action::NewTab),
            ("Primary+W", Action::ClosePane),
            ("Ctrl+Tab", Action::NextTab),
            ("Ctrl+Shift+Tab", Action::PreviousTab),
            ("Primary+B", Action::ToggleLeftSidebar),
            ("Primary+I", Action::ToggleRightSidebar),
            ("Primary+G", Action::ShowGitPanel),
            ("Primary+M", Action::ShowMonitorPanel),
            ("Primary+D", Action::SplitRight),
            ("Primary+Enter", Action::SplitDown),
            ("Ctrl+PageDown", Action::NextPane),
            ("Ctrl+PageUp", Action::PreviousPane),
            ("Ctrl+Shift+PageUp", Action::MoveTabLeft),
            ("Ctrl+Shift+PageDown", Action::MoveTabRight),
            ("Primary+F", Action::Search),
            ("Primary+Up", Action::CommandPrevious),
            ("Primary+Down", Action::CommandNext),
            ("Primary+F7", Action::SelectCommandOutput),
            ("Primary+F8", Action::CopyCommandOutput),
            ("Primary+F9", Action::ShowShellStatus),
            ("Primary+F10", Action::ShowNotifications),
            ("Primary+F11", Action::ToggleNotifications),
            ("F3", Action::SearchNext),
            ("Shift+F3", Action::SearchPrevious),
        ] {
            bindings.0.insert(Chord::parse(chord).unwrap(), action);
        }
        bindings
    }
}
impl Bindings {
    pub fn action(&self, key: Key, mods: Mods) -> Option<&Action> {
        self.0.get(&Chord::normalized(key, mods))
    }
    pub fn configure(&mut self, table: &Table) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut seen = FnvHashMap::default();
        for (name, value) in table.entries.iter().take(128) {
            let parsed = (|| {
                let chord = Chord::parse(name)?;
                let action = parse_action(value)?;
                if let Some(previous) = seen.insert(chord, name.clone()) {
                    return Err(format!("与“{previous}”是同一快捷键"));
                }
                self.0.insert(chord, action);
                Ok(())
            })();
            if let Err(error) = parsed {
                warnings.push(format!("[keybindings] {name}：{error}"));
            }
        }
        if table.entries.len() > 128 {
            warnings.push("[keybindings] 最多 128 个自定义快捷键".into());
        }
        warnings
    }
}
fn parse_action(value: &Value) -> Result<Action, String> {
    if let Value::Table(table) = value {
        if table.entries.iter().any(|(key, _)| !matches!(key.as_str(), "action" | "text")) {
            return Err("发送文本只支持 action、text 字段".into());
        }
        if table.get("action") != Some(&Value::String("send-text".into())) {
            return Err("内联表的 action 应为 send-text".into());
        }
        let Some(Value::String(text)) = table.get("text") else {
            return Err("send-text 缺少字符串 text".into());
        };
        if text.len() > 8192 {
            return Err("发送文本最多 8 KiB".into());
        }
        return Ok(Action::SendText(text.clone()));
    }
    let Value::String(name) = value else {
        return Err("动作应为名称或发送文本的内联表".into());
    };
    Ok(match name.as_str() {
        "copy" => Action::Copy,
        "paste" => Action::Paste,
        "page-up" => Action::PageUp,
        "page-down" => Action::PageDown,
        "history-top" => Action::HistoryTop,
        "history-bottom" => Action::HistoryBottom,
        "font-increase" => Action::FontIncrease,
        "font-decrease" => Action::FontDecrease,
        "font-reset" => Action::FontReset,
        "next-theme" => Action::NextTheme,
        "reset-theme" => Action::ResetTheme,
        "show-resources" => Action::ShowResources,
        "search" => Action::Search,
        "search-next" => Action::SearchNext,
        "search-previous" => Action::SearchPrevious,
        "open-link" => Action::OpenLink,
        "copy-link" => Action::CopyLink,
        "command-previous" => Action::CommandPrevious,
        "command-next" => Action::CommandNext,
        "select-command-output" => Action::SelectCommandOutput,
        "copy-command-output" => Action::CopyCommandOutput,
        "show-shell-status" => Action::ShowShellStatus,
        "show-notifications" => Action::ShowNotifications,
        "toggle-notifications" => Action::ToggleNotifications,
        "clear-notifications" => Action::ClearNotifications,
        "new-tab" => Action::NewTab,
        "close-pane" => Action::ClosePane,
        "next-tab" => Action::NextTab,
        "previous-tab" => Action::PreviousTab,
        "move-tab-left" => Action::MoveTabLeft,
        "move-tab-right" => Action::MoveTabRight,
        "split-right" => Action::SplitRight,
        "split-down" => Action::SplitDown,
        "next-pane" => Action::NextPane,
        "previous-pane" => Action::PreviousPane,
        "toggle-left-sidebar" => Action::ToggleLeftSidebar,
        "toggle-right-sidebar" => Action::ToggleRightSidebar,
        "show-git-panel" => Action::ShowGitPanel,
        "show-monitor-panel" => Action::ShowMonitorPanel,
        "none" => Action::Ignore,
        "pass-through" => Action::PassThrough,
        _ => return Err(format!("不认识的动作“{name}”")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn control_bytes_and_shifted_symbols_match_without_losing_modifiers() {
        let bindings = Bindings::default();
        assert_eq!(
            bindings.action(Key::Char('\x03'), Mods { ctrl: true, shift: true, ..Default::default() }),
            Some(&Action::Copy)
        );
        let mut mods = Mods::default();
        if cfg!(target_os = "macos") {
            mods.logo = true;
        } else {
            mods.ctrl = true;
            mods.shift = true;
        }
        assert_eq!(bindings.action(Key::Char('+'), mods), Some(&Action::FontIncrease));
        assert!(bindings.action(Key::Char('c'), Mods { ctrl: true, ..Default::default() }).is_none());
    }
    #[test]
    fn invalid_and_conflicting_bindings_keep_other_actions() {
        let table = crate::config::toml::parse(
            "\"Ctrl+Shift+C\"='none'\n\"Shift+Ctrl+c\"='paste'\nF6={action='send-text',text='hello'}\nF7='bad'\n",
        )
        .unwrap();
        let mut bindings = Bindings::default();
        let warnings = bindings.configure(&table);
        assert_eq!(warnings.len(), 2);
        assert_eq!(
            bindings.action(Key::Char('c'), Mods { ctrl: true, shift: true, ..Default::default() }),
            Some(&Action::Ignore)
        );
        assert_eq!(bindings.action(Key::F(6), Mods::default()), Some(&Action::SendText("hello".into())));
    }
}
