//! Fixed-record, selected-PID adapters. All callers admit batch work first.
use super::Proc;
pub(super) struct Record {
    pub start: u64,
    pub milliseconds: u64,
    pub row: Proc,
}
#[cfg(any(target_os = "linux", test))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(windows)]
mod windows;
#[cfg(target_os = "linux")]
pub(super) use linux::{Clock, read};
#[cfg(target_os = "macos")]
pub(super) use macos::{Clock, read};
#[cfg(windows)]
pub(super) use windows::{Clock, read};

pub(crate) fn label(chars: impl Iterator<Item = char>) -> String {
    let mut text = String::with_capacity(super::PROCESS_NAME_BYTES);
    for c in chars.filter(|c| !c.is_control()) {
        if text.len() + c.len_utf8() > super::PROCESS_NAME_BYTES {
            break;
        }
        text.push(c);
    }
    text
}
