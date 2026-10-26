#[cfg(test)]
mod allocation_view_probe {
    use super::*;
    use crate::{auth_allocation_meter as meter, config::Config, render::{Framebuffer, Renderer}, resources::Scope};
    use super::{editor::Editor, viewport::{Viewport,SLICE_CHARS}, view::Panel};
    #[derive(Clone, Copy, Default)]
    struct Sample { name:&'static str, counts:meter::Counts, used:usize, peak:usize }
    fn sample(pool:&Arc<Budget>,name:&'static str) -> Sample {
        let counts=meter::peek(); let stats=pool.snapshot();
        if counts.live<0 || counts.live as usize>stats.used || counts.peak<0 || counts.peak as usize>stats.peak {
            meter::stop(); panic!("unfunded view step={name} live={} peak={} used={} budget_peak={}",counts.live,counts.peak,stats.used,stats.peak);
        }
        Sample { name,counts,used:stats.used,peak:stats.peak }
    }
    fn emit(s:Sample) {
        eprintln!("AUTH_ALLOC_SAMPLE label=view step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.name,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations);
    }
    fn setup(pool:&Arc<Budget>,notify:Notify,text:&[u8],cancel:&Cancellation) -> (Backend,Front) {
        let (mut back,front)=Backend::new(Owner {session:12,generation:1},pool,notify).unwrap();
        back.begin(Kind::Secret,text,cancel).unwrap(); (back,front)
    }
    #[test]
    fn measured_viewport_overlap_and_warm_masked_render_are_bounded() {
        let pool=Budget::new(2*1024*1024,0);
        let notify:Notify=Arc::new(||{}); let cancel=Cancellation::default();
        let prompt=[27;PROMPT_LIMIT]; let _=pool.snapshot(); let _=std::thread::current();
        let mut samples=[Sample::default();7];
        meter::start();
        let (back,front)=setup(&pool,notify.clone(),&prompt,&cancel);
        let mut editor=Editor::new(&front,front.snapshot().input.unwrap()).unwrap();
        let mut view=Viewport::new(&editor,80,20).unwrap();
        samples[0]=sample(&pool,"created");
        let before=meter::peek(); let mut slices=0; let mut max_chars=0;
        while view.pending() { let n=view.step(); assert!(n>0 && n<=SLICE_CHARS); max_chars=max_chars.max(n); slices+=1; }
        assert_eq!(meter::peek().allocations,before.allocations);
        samples[1]=sample(&pool,"laid_out");
        view.resize(2,20).unwrap();
        samples[2]=sample(&pool,"overlap");
        let resize_before=meter::peek();
        for columns in [3,20,100,50,10,160,90] { view.resize(columns,20).unwrap(); }
        let resize_after=meter::peek();
        assert_eq!(resize_before.allocations,resize_after.allocations);
        assert_eq!(resize_before.reallocations,resize_after.reallocations);
        samples[3]=sample(&pool,"coalesced");
        while view.pending() { let n=view.step(); assert!(n>0 && n<=SLICE_CHARS); max_chars=max_chars.max(n); slices+=1; }
        samples[4]=sample(&pool,"swapped");
        view.end(); view.drawn(); view.review(&mut editor).unwrap();
        assert!(view.lines().all(|l|l.text.len()<=128*4));
        samples[5]=sample(&pool,"reviewed");
        drop((view,editor,front,back));
        samples[6]=sample(&pool,"released");
        let counts=meter::stop();
        assert_eq!(counts.live,0); assert_eq!(pool.snapshot().used,0);
        for s in samples { emit(s); }
        eprintln!("AUTH_VIEW_WORK slices={slices} max_chars={max_chars} layout_allocations=0 resize_allocations=0");

        // 另测预热后的编辑+软件绘制；已有画布/字体排除在本次增量分配区间之外。
        let render_pool=Budget::new(64*1024*1024,1024*1024*1024);
        let _scope=Scope::enter(render_pool.clone());
        let mut renderer=Renderer::new(&Config::default(),1.0);
        let mut frame=Framebuffer::new(1000,620);
        let ascii=(32..=126u8).map(char::from).collect::<String>();
        assert!(renderer.auth_text(&mut frame,[0,0,1000,30],&ascii,false));
        let model_pool=Budget::new(2*1024*1024,0);
        let text="Fixture authentication identity and complete key fingerprint. ".repeat(100);
        let (back,front)=setup(&model_pool,notify.clone(),text.as_bytes(),&cancel);
        let mut panel=Panel::new(&front,front.snapshot().input.unwrap()).unwrap();
        let token=panel.editor().token();
        let bounds=[40,30,920,560];
        panel.render(&mut renderer,&mut frame,bounds).unwrap();
        while panel.pending() { panel.step(); }
        panel.end(); panel.render(&mut renderer,&mut frame,bounds).unwrap();
        let model_used=model_pool.snapshot().used;
        let render_used=render_pool.snapshot().used;
        let mut times=[0u128;200];
        meter::start();
        for (index,time) in times.iter_mut().enumerate() {
            let started=Instant::now();
            if index%2==0 { panel.editor().character(token,'x').unwrap(); }
            else { panel.editor().delete(token,editor::Delete::Backward).unwrap(); }
            panel.sync(&front).unwrap();
            panel.render(&mut renderer,&mut frame,bounds).unwrap();
            *time=started.elapsed().as_micros();
        }
        let counts=meter::stop();
        assert_eq!(counts.allocations,0); assert_eq!(counts.reallocations,0); assert_eq!(counts.live,0);
        assert_eq!(model_pool.snapshot().used,model_used); assert_eq!(render_pool.snapshot().used,render_used);
        times.sort_unstable();
        eprintln!("AUTH_VIEW_RENDER frames=200 allocations=0 reallocations=0 p50_us={} p95_us={} p99_us={} max_us={}",times[99],times[189],times[197],times[199]);
        eprint!("AUTH_VIEW_RENDER_SAMPLES us=");
        for time in times { eprint!("{time},"); } eprintln!();
        drop((panel,front,back,frame,renderer));
        assert_eq!(model_pool.snapshot().used,0); assert_eq!(render_pool.snapshot().used,0);
    }
}
