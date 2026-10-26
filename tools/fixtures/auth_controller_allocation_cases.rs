#[cfg(test)]
mod allocation_controller_probe {
    use super::*;
    use crate::{auth_allocation_meter as meter, config::Config, render::{Framebuffer,Renderer}, resources::Scope,
        platform::{ClipboardRequest,Event,protected_input::{InputCommand as C,InputKind}}};
    use controller::{Controller,Phase};
    #[derive(Clone,Copy,Default)]
    struct Sample {name:&'static str,counts:meter::Counts,used:usize,peak:usize}
    fn sample(pool:&Arc<Budget>,name:&'static str)->Sample {
        let counts=meter::peek();let stats=pool.snapshot();
        if counts.live<0 || counts.live as usize>stats.used || counts.peak<0 || counts.peak as usize>stats.peak {
            meter::stop();panic!("unfunded controller step={name} live={} peak={} used={} budget_peak={}",counts.live,counts.peak,stats.used,stats.peak);
        }
        Sample{name,counts,used:stats.used,peak:stats.peak}
    }
    fn command(controller:&mut Controller,command:C){let source=controller.input_context();controller.native_input(source.command(command).unwrap()).unwrap();}
    fn paint(controller:&mut Controller,renderer:&mut Renderer,frame:&mut Framebuffer){
        for _ in 0..256 {controller.step().unwrap();controller.render(renderer,frame,[40,30,920,560]).unwrap();if !controller.pending(){return;}}
        panic!("layout did not finish");
    }
    fn setup(pool:&Arc<Budget>,notify:Notify,prompt:&[u8],cancel:&Cancellation,profiles:&crate::config::profiles::Profiles)->(Backend,Ticket,Controller){
        let owner=Owner{session:12,generation:1};let identity=Identity::new(profiles.clone(),0,pool).unwrap();let (mut back,front)=Backend::with_identity(owner,pool,notify,identity).unwrap();
        let ticket=back.begin(Kind::Secret,prompt,cancel).unwrap();let controller=Controller::new(front,owner,true).unwrap();(back,ticket,controller)
    }
    #[test]
    fn measured_controller_lifecycle_and_warm_events_are_bounded(){
        let render_pool=Budget::new(64*1024*1024,1024*1024*1024);let _scope=Scope::enter(render_pool.clone());
        let mut renderer=Renderer::new(&Config::default(),1.0);let mut frame=Framebuffer::new(1000,620);
        let ascii=(32..=126u8).map(char::from).collect::<String>();assert!(renderer.auth_text(&mut frame,[0,0,1000,30],&ascii,false));
        let pool=Budget::new(2*1024*1024,0);let notify:Notify=Arc::new(||{});let cancel=Cancellation::default();
        let (id,name,host,user)=("p".repeat(64),("\u{202e}".repeat(42)+"xy"),"h".repeat(255),"u".repeat(128));
        let definition=format!("[connections.{id}]\nkind='ssh'\nname='{name}'\nhost='{host}'\nuser='{user}'\nport=65535\n");
        let (mut config,mut warnings)=Config::parse(&definition);config.admit_connections(&mut warnings);assert!(warnings.is_empty());let profiles=std::mem::take(&mut config.connections);drop((config,warnings,definition,id,name,host,user));
        let (back,_,mut warm)=setup(&pool,notify.clone(),b"fixture",&cancel,&profiles);paint(&mut warm,&mut renderer,&mut frame);drop((warm,back));
        assert_eq!(pool.snapshot().used,0);let _=pool.snapshot();let _=std::thread::current();let _=Instant::now();
        let prompt=[27;PROMPT_LIMIT];let answer=[b'x';ANSWER_LIMIT];let mut samples=[Sample::default();10];
        meter::start();
        let (mut back,ticket,mut controller)=setup(&pool,notify.clone(),&prompt,&cancel,&profiles);
        samples[0]=sample(&pool,"created");
        while controller.pending(){controller.step().unwrap();}
        samples[1]=sample(&pool,"indexed");
        controller.render(&mut renderer,&mut frame,[40,30,920,560]).unwrap();
        samples[2]=sample(&pool,"overlap");
        paint(&mut controller,&mut renderer,&mut frame);command(&mut controller,C::Page(isize::MAX));
        controller.render(&mut renderer,&mut frame,[40,30,920,560]).unwrap();
        samples[3]=sample(&pool,"reviewed");
        command(&mut controller,C::Paste);let ClipboardRequest::Paste(request)=controller.take_clipboard_request().unwrap() else{panic!()};
        let text=request.policy().utf8(&answer).unwrap();let Event::ProtectedPaste(paste)=request.finish(Some(text))else{panic!()};
        unsafe{meter::watch_clear(paste.text().unwrap().as_bytes());}
        samples[4]=sample(&pool,"inflight");
        let old=controller.input_context();controller.focus(false).unwrap();
        samples[5]=sample(&pool,"paused");
        assert!(!controller.input_context().enabled());controller.focus(true).unwrap();assert!(!old.same_source(&controller.input_context()));
        samples[6]=sample(&pool,"resumed");
        assert!(controller.paste(paste).is_err());assert_eq!(meter::peek().scrub_checks,1);
        let input=controller.input_context().utf8_input(InputKind::Commit,&answer).unwrap();
        unsafe{meter::watch_clear(input.text().as_bytes());}controller.native_input(input).unwrap();assert_eq!(meter::peek().scrub_checks,2);
        paint(&mut controller,&mut renderer,&mut frame);command(&mut controller,C::Page(isize::MAX));
        controller.render(&mut renderer,&mut frame,[40,30,920,560]).unwrap();samples[7]=sample(&pool,"ready_to_submit");
        command(&mut controller,C::Submit);assert_eq!(controller.phase(),Phase::Waiting);assert!(!controller.input_context().enabled());
        let received=back.take(ticket,&cancel).unwrap().unwrap();assert_eq!(received.bytes(),&answer);
        unsafe{meter::watch_clear(received.bytes());}drop(received);assert_eq!(meter::peek().scrub_checks,3);
        samples[8]=sample(&pool,"submitted");drop((controller,back,old));samples[9]=sample(&pool,"released");let counts=meter::stop();
        assert_eq!(counts.live,0);assert_eq!(pool.snapshot().used,0);assert_eq!(counts.scrub_failures,0);
        for s in samples{eprintln!("AUTH_ALLOC_SAMPLE label=controller step={} live={} peak={} used={} budget_peak={} allocations={} reallocations={}",s.name,s.counts.live,s.counts.peak,s.used,s.peak,s.counts.allocations,s.counts.reallocations);}
        eprintln!("AUTH_CONTROLLER_CLEAR scrub_checks={} scrub_failures={}",counts.scrub_checks,counts.scrub_failures);
        let (back,_,mut controller)=setup(&pool,notify.clone(),b"Fixture complete identity",&cancel,&profiles);paint(&mut controller,&mut renderer,&mut frame);
        let mut times=[0u128;200];let mut input_times=[0u128;200];let model_used=pool.snapshot().used;let render_used=render_pool.snapshot().used;
        meter::start();
        for (index,time) in times.iter_mut().enumerate(){
            let started=Instant::now();let source=controller.input_context();
            let input=if index%2==0{source.utf8_input(InputKind::Commit,b"x").unwrap()}else{source.command(C::Backward).unwrap()};
            controller.native_input(input).unwrap();input_times[index]=started.elapsed().as_nanos();
            controller.render(&mut renderer,&mut frame,[40,30,920,560]).unwrap();*time=started.elapsed().as_micros();
        }
        let counts=meter::stop();assert_eq!(counts.allocations,100);assert_eq!(counts.reallocations,0);assert_eq!(counts.live,0);
        assert_eq!(pool.snapshot().used,model_used);assert_eq!(render_pool.snapshot().used,render_used);
        times.sort_unstable();input_times.sort_unstable();
        eprintln!("AUTH_CONTROLLER_EVENTS events=200 allocations=100 reallocations=0 p50_us={} p95_us={} p99_us={} max_us={}",times[99],times[189],times[197],times[199]);
        eprint!("AUTH_CONTROLLER_EVENT_SAMPLES us=");for t in times{eprint!("{t},");}eprintln!();
        eprintln!("AUTH_CONTROLLER_INPUT p50_ns={} p95_ns={} p99_ns={} max_ns={}",input_times[99],input_times[189],input_times[197],input_times[199]);
        eprint!("AUTH_CONTROLLER_INPUT_SAMPLES ns=");for t in input_times{eprint!("{t},");}eprintln!();
        drop((controller,back,frame,renderer,ascii,profiles));assert_eq!(pool.snapshot().used,0);assert_eq!(render_pool.snapshot().used,0);
    }
}
