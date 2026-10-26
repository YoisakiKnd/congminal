//! name表有界解码；无UTF-16中间Vec、无trim副本，优先ID16。
use super::{FAMILIES, NAME_LIMIT, u16_at};
fn record(name: &[u8], i: usize) -> Option<(u16, u16, &[u8])> {
    let start = 6 + 12 * i;
    let platform = u16_at(name, start)?;
    let id = u16_at(name, start + 6)?;
    let strings = u16_at(name, 4)? as usize;
    let len = u16_at(name, start + 8)? as usize;
    let offset = strings + u16_at(name, start + 10)? as usize;
    Some((platform, id, name.get(offset..offset + len)?))
}
fn decode(platform: u16, bytes: &[u8]) -> Option<String> {
    let chars = || {
        char::decode_utf16(bytes.as_chunks::<2>().0.iter().map(|b| u16::from_be_bytes(*b)))
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
    };
    let length = match platform {
        0 | 3 => {
            if bytes.len() > 2 * NAME_LIMIT + 1 {
                return None;
            }
            chars().map(char::len_utf8).sum()
        }
        1 => {
            if bytes.len() > NAME_LIMIT {
                return None;
            }
            bytes.iter().map(|b| if *b < 128 { 1 } else { 2 }).sum()
        }
        _ => return None,
    };
    if length > NAME_LIMIT {
        return None;
    }
    let mut text = String::with_capacity(length);
    if platform == 1 {
        for b in bytes {
            text.push(*b as char);
        }
    } else {
        text.extend(chars());
    }
    let start = text.len() - text.trim_start().len();
    let len = text.trim().len();
    drop(text.drain(..start));
    text.truncate(len);
    Some(text)
}
pub(super) fn family_names(
    name: &[u8],
    records: &mut usize,
    decoded: &mut usize,
    limited: &mut bool,
) -> Option<Vec<String>> {
    let count = u16_at(name, 2)? as usize;
    if count > 4096 || count > *records {
        *limited = true;
        return None;
    }
    *records -= count;
    name.get(..6 + 12 * count)?;
    let preferred = if (0..count).any(|i| record(name, i).is_some_and(|(p, id, _)| matches!(p, 0 | 1 | 3) && id == 16))
    {
        16
    } else {
        1
    };
    let mut result = Vec::with_capacity(FAMILIES);
    let mut seen = [None; FAMILIES];
    let mut seen_len = 0;
    for i in 0..count {
        let Some((platform, id, bytes)) = record(name, i) else {
            continue;
        };
        if id != preferred || !matches!(platform, 0 | 1 | 3) {
            continue;
        }
        let key = (if platform == 3 { 0 } else { platform }, bytes.as_ptr() as usize, bytes.len());
        if seen[..seen_len].contains(&Some(key)) {
            continue;
        }
        if seen_len < FAMILIES {
            seen[seen_len] = Some(key);
            seen_len += 1;
        }
        let cost = bytes.len().saturating_mul(2);
        if cost > *decoded {
            *limited = true;
            *decoded = 0;
            break;
        }
        *decoded -= cost;
        let Some(text) = decode(platform, bytes) else {
            *limited = true;
            continue;
        };
        if result.contains(&text) {
            continue;
        }
        if result.len() == FAMILIES {
            *limited = true;
            break;
        }
        result.push(text);
    }
    result.sort();
    result.shrink_to_fit();
    Some(result)
}
