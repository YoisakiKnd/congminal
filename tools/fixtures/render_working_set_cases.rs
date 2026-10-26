#[cfg(test)]
mod working_set_cost {
    use super::*;
    use crate::{resources::{Budget,Scope},vt::Parser};
    use std::time::Instant;
    #[test]
    #[ignore="same fixture appended to baseline/current copies by check_render_work.py"]
    fn same_pixels_full_and_idle_views() {
        let outline=std::env::var_os("CONGMINAL_RENDER_OUTLINE").as_deref()==Some(std::ffi::OsStr::new("1"));
        let pool=Budget::new(256*1024*1024,if outline {1024*1024*1024}else{0});let _scope=Scope::enter(pool.clone());
        let config=Config::default();let mut renderer=Renderer::new(&config,1.0);
        if !outline {assert!(renderer.fonts.description.contains("Terminus"));}
        eprintln!("RENDER_FONT {:?}",renderer.fonts.description);
        let mut terms=[Terminal::new(160,48),Terminal::new(81,25)];let mut views=[ViewState::default(),ViewState::default()];
        let mut frames=terms.each_ref().map(|t|{let(w,h)=renderer.pixel_size(t.cols(),t.rows());Framebuffer::new(w as usize,h as usize)});
        for term in &mut terms {
            let mut parser=Parser::new();
            for row in 0..term.rows() {let text=if row%2==0 {"fn main(){return x!=0;} e\u{301} x\u{301}\u{302} 👨\u{200D}👩\u{200D}👧 🇨🇳"}else{"\x1b[1m==> != <= -> \x1b[0m1\u{fe0f}\u{20e3} ❤\u{fe0f}\u{200d}🔥  abcxyz"};parser.advance(term,format!("\x1b[{};1H{}",row+1,text).as_bytes());}
        }
        for i in 0..16 {let v=i%2;renderer.swap_view(&mut views[v]);renderer.invalidate();renderer.draw(&terms[v],"",None,true,&mut frames[v]);renderer.swap_view(&mut views[v]);}
        // RENDER_METER_START
        for full in [true,false] {
            let mode=if full{"full"}else{"idle"};let mut samples=[0u64;2000];eprintln!("RENDER_WORK_START mode={mode}");let total=Instant::now();
            for (i,ns) in samples.iter_mut().enumerate() {
                let v=i%2;renderer.swap_view(&mut views[v]);if full{renderer.invalidate();}
                let started=Instant::now();let damage=renderer.draw(&terms[v],"",None,true,&mut frames[v]);*ns=started.elapsed().as_nanos() as u64;
                if !full {assert!(damage.is_none());}renderer.swap_view(&mut views[v]);
            }
            let elapsed=total.elapsed().as_nanos();let pixels=hash(&(&frames[0].pixels,&frames[1].pixels));
            eprintln!("RENDER_WORK_END mode={mode} total_ns={elapsed} pixels={pixels} samples={samples:?}");
        }
        drop((renderer,terms,views,frames));assert_eq!(pool.snapshot().used,0);assert_eq!(pool.snapshot().mappings,0);
        // RENDER_METER_STOP
        eprintln!("RENDER_WORK_CLEAN final=0");
    }
}
