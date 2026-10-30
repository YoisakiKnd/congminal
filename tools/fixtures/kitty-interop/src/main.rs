use ratatui::{
    buffer::Buffer,
    layout::{Rect, Size},
    widgets::Widget,
};
use ratatui_image::{
    Resize,
    picker::{Picker, ProtocolType},
};
use std::io::Write;
fn main() {
    let argument = std::env::args_os().nth(1).unwrap();
    let probe = argument == "--probe";
    let image = image::RgbaImage::from_fn(128, 128, |x, y| {
        image::Rgba([(x * 2) as u8, (y * 2) as u8, 127, if x < 8 && y < 8 { 0 } else { 255 }])
    });
    let mut picker = if probe {
        let picker = Picker::from_query_stdio().expect("real picker query failed");
        println!("\r\nREAL_PICKER protocol={:?} font={:?}\r", picker.protocol_type(), picker.font_size());
        assert_eq!(picker.protocol_type(), ProtocolType::Kitty, "real library fell back to character blocks");
        picker
    } else {
        #[allow(deprecated)]
        let mut picker = Picker::from_fontsize((8, 16).into());
        picker.set_protocol_type(ProtocolType::Kitty);
        picker
    };
    if !probe {
        std::fs::write(std::path::Path::new(&argument).with_extension("rgba"), image.as_raw()).unwrap();
    }
    let protocol =
        picker.new_protocol(image::DynamicImage::ImageRgba8(image), Size::new(16, 8), Resize::Fit(None)).unwrap();
    let area = Rect::new(0, 0, 16, 8);
    let mut buffer = Buffer::empty(area);
    ratatui_image::Image::new(&protocol).render(area, &mut buffer);
    let mut wire = String::from("\x1b[?25l\x1b[H");
    for y in 0..8 {
        wire.push_str(&format!("\x1b[{};1H", y + 1));
        wire.push_str(buffer[(0, y)].symbol());
    }
    if probe {
        // Same order as Nakuru: probe first, then enter the crossterm TUI.
        crossterm::terminal::enable_raw_mode().unwrap();
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen).unwrap();
        std::io::stdout().write_all(wire.as_bytes()).unwrap();
        std::io::stdout().flush().unwrap();
        crossterm::terminal::disable_raw_mode().unwrap();
    } else {
        std::fs::write(argument, wire.as_bytes()).unwrap();
    }
}
