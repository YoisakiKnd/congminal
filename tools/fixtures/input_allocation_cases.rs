#[cfg(test)]
mod allocation_input_probe {
    use super::*;
    use crate::auth_allocation_meter as meter;
    fn measure(name: &str, expected: Option<&str>, decode: impl FnOnce() -> Option<String>) {
        meter::start();
        let text = decode();
        let live = meter::peek();
        assert_eq!(text.as_deref(), expected);
        assert_eq!(live.live as usize, expected.map_or(0, str::len));
        assert_eq!(live.allocations, usize::from(expected.is_some_and(|s| !s.is_empty())));
        assert_eq!(live.reallocations, 0);
        drop(text);
        let counts = meter::stop();
        assert_eq!(counts.live, 0);
        eprintln!("INPUT_ALLOC case={name} allocations={} reallocations={} peak={} final_live={}", counts.allocations, counts.reallocations, counts.peak, counts.live);
    }
    #[test]
    fn measured_input_decoders_admit_before_allocating() {
        let exact = "x".repeat(1020) + "文";
        let utf16: Vec<_> = exact.encode_utf16().collect();
        let oversized = "x".repeat(CLIPBOARD_LIMIT + 1);
        let oversized16 = vec![0x4e2d; 342];
        let read = ClipboardRead { limit: 1023, strict_unicode: true };
        measure("utf8_exact", Some(&exact), || read.utf8(exact.as_bytes()));
        measure("utf16_exact", Some(&exact), || read.utf16(&utf16));
        measure("utf8_oversize", None, || read.utf8(oversized.as_bytes()));
        measure("utf16_byte_oversize", None, || read.utf16(&oversized16));
        measure("utf16_unpaired", None, || read.utf16(&[0xd800]));
        measure("utf8_invalid", None, || read.utf8(&[0xff]));
        measure("utf8_empty", Some(""), || read.utf8(b""));
        measure("utf16_empty", Some(""), || read.utf16(&[]));
        measure("utf8_embedded_nul", Some("a\0b"), || read.utf8(b"a\0b"));
        measure("plain_request", Some("plain"), || {
            let Event::Paste(text) = PasteRequest::default().finish(ClipboardRead::default().utf8(b"plain")) else {panic!()};
            Some(text)
        });
        let legacy = ClipboardRead { limit: 3, strict_unicode: false };
        measure("legacy_utf16_replacement", Some("�"), || legacy.utf16(&[0xd800]));
    }
}
