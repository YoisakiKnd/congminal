//! 父端与独立 helper 共用的协议类型与提示上限。
pub const PROMPT_LIMIT: usize = 16 * 1024;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Limit,
    Budget,
    Busy,
    Stale,
    WrongPool,
    Cancelled,
    Timeout,
    Closed,
    Poisoned,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Secret,
    Confirm,
    Notice,
    /// OpenSSH未给出语义hint；可以编辑掩码答案，但键盘提交默认取消，需明确点击发送。
    Unspecified,
}
impl Kind {
    pub fn hint(hint: Option<&str>) -> Result<Self, Error> {
        match hint {
            None | Some("") => Ok(Self::Unspecified),
            Some("confirm") => Ok(Self::Confirm),
            Some("none") => Ok(Self::Notice),
            _ => Err(Error::Invalid),
        }
    }
    pub fn accepts_text(self) -> bool {
        matches!(self, Self::Secret | Self::Unspecified)
    }
}
