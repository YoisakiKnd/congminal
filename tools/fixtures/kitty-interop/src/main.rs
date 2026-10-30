use ratatui::{buffer::Buffer,layout::{Rect,Size},widgets::Widget};
use ratatui_image::{picker::{Picker,ProtocolType},Resize};
fn main() {
 let out=std::path::PathBuf::from(std::env::args_os().nth(1).unwrap());
 let image=image::RgbaImage::from_fn(128,128,|x,y|image::Rgba([(x*2)as u8,(y*2)as u8,127,if x<8&&y<8{0}else{255}]));
 std::fs::write(out.with_extension("rgba"),image.as_raw()).unwrap();
 let mut picker=Picker::from_fontsize((8,16).into());picker.set_protocol_type(ProtocolType::Kitty);
 let protocol=picker.new_protocol(image::DynamicImage::ImageRgba8(image),Size::new(16,8),Resize::Fit(None)).unwrap();
 let area=Rect::new(0,0,16,8);let mut buffer=Buffer::empty(area);
 ratatui_image::Image::new(&protocol).render(area,&mut buffer);
 let mut wire=String::from("\x1b[?25l\x1b[H");
 for y in 0..8 {wire.push_str(&format!("\x1b[{};1H",y+1));wire.push_str(buffer[(0,y)].symbol());}
 std::fs::write(out,wire.as_bytes()).unwrap();
}
