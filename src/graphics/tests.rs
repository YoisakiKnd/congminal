use super::*;
use crate::{resources::Scope, term::Terminal, vt::Parser};
fn feed(t: &mut Terminal, s: &str) {
    Parser::new().advance(t, s.as_bytes());
}
#[test]
fn query_and_virtual_rgba_matches_nakuru_wire_format() {
    let mut t = Terminal::new(5, 3);
    t.set_cell_pixels(1, 1);
    feed(&mut t, "\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    assert_eq!(t.take_response(), b"\x1b_Gi=31;OK\x1b\\");
    feed(&mut t, "\x1b_Gq=2,i=42,a=T,U=1,f=32,t=d,s=1,v=1,m=0;/wAA/w==\x1b\\");
    let placement = t.graphics.virtual_placement(42, 0).unwrap();
    assert_eq!(t.graphics.tile(placement, (1, 1)).unwrap().pixel(0, 0), Some([255, 0, 0, 255]));
    assert!(t.take_response().is_empty());
}
#[test]
fn chunks_and_invalid_input_release_credit_and_keep_previous_image() {
    let pool = Budget::new(4 * 1024 * 1024, 0);
    let scope = Scope::enter(pool.clone());
    let mut t = Terminal::new(5, 3);
    t.set_cell_pixels(1, 1);
    feed(&mut t, "\x1b_Gq=2,i=42,a=T,U=1,f=32,t=d,s=2,v=1,m=1;/wAA\x1b\\");
    feed(&mut t, "\x1b_Gq=2,m=0;/wD/AP8=\x1b\\");
    assert!(t.graphics.virtual_placement(42, 0).is_some());
    feed(&mut t, "\x1b_Gi=42,a=t,f=32,s=999999,v=999999;AAAA\x1b\\");
    assert!(t.take_response().starts_with(b"\x1b_Gi=42;E2BIG"));
    assert!(t.graphics.virtual_placement(42, 0).is_some());
    feed(&mut t, "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    assert!(t.graphics.virtual_placement(42, 0).is_none());
    drop(t);
    drop(scope);
    assert_eq!(pool.snapshot().used, 0);
}
#[test]
fn fragmented_apc_and_cancel_are_not_terminal_text() {
    let mut t = Terminal::new(12, 3);
    t.set_cell_pixels(1, 1);
    let mut p = Parser::new();
    for b in b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\OK" {
        p.advance(&mut t, &[*b]);
    }
    assert_eq!(t.grid()[0][0].c, 'O');
    assert_eq!(t.grid()[0][1].c, 'K');
    p.advance(&mut t, b"\x1b_Gi=32,s=1,v=1,a=q,f=24;AAAA\x18next");
    assert!(t.take_response().windows(6).any(|b| b == b"EINVAL"));
}
#[test]
fn placeholder_inherits_columns_and_full_32_bit_id() {
    use crate::term::Color;
    let a = Placeholder::parse(Color::Indexed(42), Color::Default, "\u{305}\u{305}\u{30e}", None).unwrap();
    assert_eq!(a.image, 42 + (2 << 24));
    assert_eq!(a.row, 0);
    assert_eq!(a.column, 0);
    let b = Placeholder::parse(Color::Indexed(42), Color::Default, "", Some(a)).unwrap();
    assert_eq!(b.column, 1);
    assert_eq!(b.image, a.image);
    assert!(Placeholder::parse(Color::Indexed(42), Color::Default, "", None).is_none());
}
fn b64(bytes: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for v in bytes.chunks(3) {
        let a = u32::from(v[0]) << 16 | u32::from(*v.get(1).unwrap_or(&0)) << 8 | u32::from(*v.get(2).unwrap_or(&0));
        out.push(A[(a >> 18) as usize] as char);
        out.push(A[((a >> 12) & 63) as usize] as char);
        out.push(if v.len() > 1 { A[((a >> 6) & 63) as usize] as char } else { '=' });
        out.push(if v.len() > 2 { A[(a & 63) as usize] as char } else { '=' });
    }
    out
}
#[test]
fn png_and_compressed_rgb_decode_with_exact_limits() {
    let mut t = Terminal::new(8, 3);
    t.set_cell_pixels(1, 1);
    let png = include_bytes!("../image/testdata/rgba8.png");
    feed(&mut t, &format!("\x1b_Ga=T,U=1,i=7,f=100;{}\x1b\\", b64(png)));
    assert!(t.graphics.virtual_placement(7, 0).is_some());
    let mut z = vec![0x78, 0x01, 0x01, 3, 0, 0xfc, 0xff, 255, 0, 0];
    z.extend_from_slice(&[3, 0, 1, 0]);
    feed(&mut t, &format!("\x1b_Ga=T,U=1,i=8,f=24,o=z,s=1,v=1;{}\x1b\\", b64(&z)));
    assert_eq!(
        t.graphics.tile(t.graphics.virtual_placement(8, 0).unwrap(), (1, 1)).unwrap().pixel(0, 0),
        Some([255, 0, 0, 255])
    );
}
#[test]
fn cache_eviction_and_replacement_cannot_leave_out_of_range_crops() {
    let mut t = Terminal::new(8, 3);
    t.set_cell_pixels(1, 1);
    for id in 1..=32 {
        feed(&mut t, &format!("\x1b_Ga=T,U=1,i={id},f=32,s=1,v=1,q=2;/wAA/w==\x1b\\"));
    }
    assert!(t.graphics.virtual_placement(1, 0).is_none());
    assert!(t.graphics.virtual_placement(32, 0).is_some());
    assert_eq!(t.graphics.images.iter().flatten().count(), IMAGE_COUNT);
    feed(&mut t, "\x1b_Ga=t,i=32,f=32,s=1,v=1,q=2;AAD//w==\x1b\\");
    assert!(t.graphics.virtual_placement(32, 0).is_none());
    feed(&mut t, "\x1b_Ga=p,U=1,i=32,p=99,c=1,r=1\x1b\\");
    assert_eq!(
        t.graphics.tile(t.graphics.virtual_placement(32, 99).unwrap(), (1, 1)).unwrap().pixel(0, 0),
        Some([0, 0, 255, 255])
    );
}
#[test]
fn oversized_packets_cancelled_chunks_and_unsupported_file_transport_fail_closed() {
    let mut t = Terminal::new(8, 3);
    t.set_cell_pixels(1, 1);
    feed(&mut t, "\x1b_Ga=q,i=8,t=f,f=100;L2V0Yy9wYXNzd2Q=\x1b\\");
    assert!(t.take_response().windows(7).any(|s| s == b"ENOTSUP"));
    feed(&mut t, &format!("\x1b_Ga=q,i=9,f=24,s=1,v=1;{}\x1b\\hello", "A".repeat(6000)));
    assert!(t.take_response().windows(6).any(|s| s == b"EINVAL"));
    assert_eq!(t.grid()[0][0].c, 'h');
    feed(&mut t, "\x1b_Ga=t,i=9,f=32,s=999999999,v=999999999;AAAA\x1b\\");
    assert!(t.graphics.images.iter().all(Option::is_none));
}
#[test]
fn renderer_paints_unicode_placeholder_and_repaints_after_delete() {
    use crate::{
        config::Config,
        render::{Framebuffer, Renderer},
    };
    let mut cfg = Config::default();
    cfg.window.padding = 0.;
    cfg.colors.background = 0;
    let mut renderer = Renderer::new(&cfg, 1.);
    let (cw, ch) = renderer.cell_size();
    let mut t = Terminal::new(4, 2);
    t.set_cell_pixels(cw as u32, ch as u32);
    feed(
        &mut t,
        "\x1b[?25l\x1b_Gq=2,i=42,a=T,U=1,f=32,t=d,s=1,v=1,m=0;/wAA/w==\x1b\\\x1b[38;2;0;0;42m\u{10eeee}\u{305}\u{305}\u{305}",
    );
    let mut fb = Framebuffer::new(cw * 4, ch * 2);
    assert!(renderer.draw(&t, "", None, false, false, &mut fb).is_some());
    let pixel = (cw.min(ch) / 2) * fb.width + cw / 2;
    assert_eq!(fb.pixels[pixel] & 0xffffff, 0xff0000);
    assert!(renderer.draw(&t, "", None, false, false, &mut fb).is_none());
    feed(&mut t, "\x1b_Ga=d,d=I,i=42,q=2\x1b\\");
    assert!(renderer.draw(&t, "", None, false, false, &mut fb).is_some());
    assert_eq!(fb.pixels[pixel] & 0xffffff, 0);
}
#[test]
fn sgr_underline_selects_virtual_placement_and_inherited_row_survives_scroll() {
    let mut t = Terminal::new(4, 2);
    t.set_cell_pixels(1, 1);
    feed(
        &mut t,
        "\x1b_Ga=t,i=42,f=32,s=1,v=1,q=2;/wAA/w==\x1b\\\x1b_Ga=p,U=1,i=42,p=7,c=4,r=2,q=2\x1b\\\x1b[38;5;42;58;5;7m\u{10eeee}\u{305}\u{305}\u{305}\u{10eeee}\r\n\r\n",
    );
    let row = t.visible_row(0);
    let point =
        Placeholder::parse(row[0].fg, t.graphics.underline_color(row[0].marks).unwrap(), t.marks(row[0].marks), None);
    // The first image row has moved into history. Text ownership, not a second cell grid, retains it.
    assert!(point.is_none());
    t.scroll_display(1);
    let row = t.visible_row(0);
    let a =
        Placeholder::parse(row[0].fg, t.graphics.underline_color(row[0].marks).unwrap(), t.marks(row[0].marks), None)
            .unwrap();
    assert_eq!(a.placement, 7);
    let b = Placeholder::parse(
        row[1].fg,
        t.graphics.underline_color(row[1].marks).unwrap(),
        t.marks(row[1].marks),
        Some(a),
    )
    .unwrap();
    assert_eq!(b.column, 1);
}

#[test]
fn image_attributes_keep_the_original_cell_size_and_never_retarget_old_indices() {
    assert_eq!(std::mem::size_of::<crate::term::Cell>(), 20);
    let mut g = Graphics::new(Budget::new(1024 * 1024, 0));
    let first = g.underline_index(crate::term::Color::Rgb(0, 0, 7));
    for v in 1..=256 {
        let _ = g.underline_index(crate::term::Color::Rgb(1, v as u8, 0));
    }
    assert_eq!(g.underline_color(first << 17), Some(crate::term::Color::Rgb(0, 0, 7)));
    assert!(g.underline_color(32767 << 17).is_none());
}
