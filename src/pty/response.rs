//! ConPTY interprets ESC + '_' as Alt+_ when the child disables VT input.
//! Send the ESC characters of APC replies as literal Win32 Unicode key records.
//! Other replies (including ConPTY's own DA handshake) remain byte-for-byte intact.
//! https://github.com/microsoft/terminal/blob/main/src/terminal/parser/InputStateMachineEngine.cpp
pub const MAX_RESPONSE: usize = 4096;
pub const ENCODED_LIMIT: usize = MAX_RESPONSE * 4;

pub fn encode<'a>(data: &[u8], out: &'a mut [u8; ENCODED_LIMIT]) -> Option<&'a [u8]> {
    if data.len() > MAX_RESPONSE {
        return None;
    }
    let mut written = 0;
    let mut at = 0;
    while at < data.len() {
        if data[at..].starts_with(b"\x1b_G") {
            let end = at + data[at..].windows(2).position(|s| s == b"\x1b\\")? + 2;
            for &byte in &data[at..end] {
                let part: &[u8] = if byte == 0x1b { b"\x1b[0;0;27;1;0;1_" } else { std::slice::from_ref(&byte) };
                out.get_mut(written..written.checked_add(part.len())?)?.copy_from_slice(part);
                written += part.len();
            }
            at = end;
        } else {
            *out.get_mut(written)? = data[at];
            written += 1;
            at += 1;
        }
    }
    Some(&out[..written])
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn apc_escapes_are_literal_records_and_other_queries_stay_native() {
        let mut out = [0; ENCODED_LIMIT];
        assert_eq!(
            encode(b"\x1b[?62;22c\x1b_Gi=31;OK\x1b\\\x1b[6;16;8t\x1b[0n", &mut out),
            Some(&b"\x1b[?62;22c\x1b[0;0;27;1;0;1__Gi=31;OK\x1b[0;0;27;1;0;1_\\\x1b[6;16;8t\x1b[0n"[..])
        );
        assert_eq!(encode(b"\x1b_Gi=31;OK", &mut out), None);
        assert_eq!(encode(&[b'x'; MAX_RESPONSE + 1], &mut out), None);
        let burst = b"\x1b_Gi=1;OK\x1b\\".repeat(MAX_RESPONSE / 12);
        assert!(encode(&burst, &mut out).is_some());
    }
}
