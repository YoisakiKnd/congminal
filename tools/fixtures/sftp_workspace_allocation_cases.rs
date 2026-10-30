#[cfg(test)]
mod allocation_probe {
    use super::*;
    use crate::{executor_allocation_meter as meter,resources::{Budget,Scope},ssh::auth::{Backend,Identity,Kind as PromptKind},platform::protected_input::InputKind};
    fn wait(mut condition:impl FnMut()->bool){let until=Instant::now()+Duration::from_secs(5);while !condition(){assert!(Instant::now()<until);std::thread::yield_now();}}
    #[derive(Clone,Copy,Default)]
    struct Sample{step:&'static str,counts:meter::Counts,parent_used:usize,parent_peak:usize,child_used:usize,root_total:usize}
    fn sample(root:&Arc<Budget>,child:Option<&Arc<Budget>>,step:&'static str)->Sample{
        let counts=meter::peek();let budget=root.snapshot();let reserved=budget.kinds[Kind::Sftp as usize];
        if counts.live<0 || counts.live as usize>reserved || counts.peak<0 || counts.peak as usize>16*1024*1024+std::mem::size_of::<Budget>()+128 {meter::stop();panic!("unbacked workspace SFTP allocations {step}: live={} peak={} parent={}",counts.live,counts.peak,reserved);}
        Sample{step,counts,parent_used:reserved,parent_peak:16*1024*1024+std::mem::size_of::<Budget>()+128,child_used:child.map_or(0,|c|c.snapshot().used),root_total:budget.used}
    }
    #[test]
    fn measured_workspace_directory_modal_and_late_input_keep_one_parent_reservation(){
        let parent=Budget::new(256*1024*1024,1024*1024*1024);let _scope=Scope::enter(parent.clone());
        let mut app=Workspace::new(crate::integration_tests::app("")).unwrap();let(mut config,mut warnings)=Config::parse("[connections.fixture]\nkind='ssh'\nname='Workspace fixture'\nhost='fixture'\n");config.admit_connections(&mut warnings);assert!(warnings.is_empty());app.panes[0].profile=Some((config.connections,0));assert!(app.enable());
        let owner=Owner{session:1,generation:1};let prompt=[27u8;crate::ssh::auth::PROMPT_LIMIT];let answer=[b'x';1023];
        let mut warm=Service::new(&parent,app.driver.notify.clone()).unwrap();warm.shutdown();wait(||warm.poll_shutdown()==Shutdown::Complete(Cleanup::Confirmed));drop(warm);
        let _=std::thread::current();let _=parent.snapshot();let picker_dir=std::env::current_dir().unwrap().join(".cg-local-picker-fixture");std::fs::create_dir(&picker_dir).unwrap();for i in 0..511 {std::fs::write(picker_dir.join(format!("entry-{i:04}")),b"").unwrap();}let metadata_source=picker_dir.join("source");let metadata_file=std::fs::OpenOptions::new().write(true).create_new(true).open(&metadata_source).unwrap();metadata_file.set_len(32*1024*1024).unwrap();drop(metadata_file);let mut samples=[Sample::default();7];let mut review_raw=vec![255u8;16384];review_raw[0]=b'/';review_raw[16319]=b'/';meter::start();
        app.start_remote_files(Paths{configuration:service::Config::None,helper:Path::new("/never-created-cg-helper"),temporary:Path::new("/never-created-cg-auth-parent")},b".").unwrap();let child=app.sftp.as_ref().unwrap().service.budget().clone();samples[0]=sample(&parent,Some(&child),"created");
        wait(||{app.tick();app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v|!v.loading)});samples[1]=sample(&parent,Some(&child),"reply");
        // 合成绝对目录只用于表单计量；真实目录/所选Version在回环夹具验证。
        app.sftp.as_mut().unwrap().view.as_mut().unwrap().target=PathValue::new(&child,b"/remote").unwrap();
        app.open_transfer_paths(true).unwrap();assert!(app.transfer_paths_paint());
        for _ in 0..16{let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Commit,&answer).unwrap());}
        let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Commit,b"xxxxxxxxxxxxxxxx").unwrap());drop(c);
        let c=app.protected_context().unwrap();app.protected_input(c.command(crate::platform::protected_input::InputCommand::NextField(false)).unwrap());drop(c);
        let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Commit,&answer).unwrap());app.protected_input(c.utf8_input(InputKind::Commit,b"x").unwrap());app.protected_input(c.utf8_input(InputKind::Preedit,&answer).unwrap());drop(c);
        assert!(app.transfer_paths_paint());let paths_counts=meter::peek();let paths_budget=child.snapshot();let before_paths=meter::peek();for _ in 0..200{assert!(app.transfer_paths_paint());}let after_paths=meter::peek();assert_eq!(before_paths.allocations,after_paths.allocations);assert_eq!(before_paths.reallocations,after_paths.reallocations);
        let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Preedit,b"").unwrap());app.protected_input(c.command(crate::platform::protected_input::InputCommand::NextField(false)).unwrap());drop(c);
        let c=app.protected_context().unwrap();app.protected_input(c.command(crate::platform::protected_input::InputCommand::Paste).unwrap());let crate::platform::ClipboardRequest::Paste(late_paths)=app.take_clipboard_request().unwrap()else{panic!()};assert_eq!(late_paths.policy().limit(),16384);
        app.protected_input(c.command(crate::platform::protected_input::InputCommand::Cancel).unwrap());app.event(late_paths.finish(Some("late path must not type".into())));drop(c);
        {
            let state=app.sftp.as_mut().unwrap();let v=state.view.as_mut().unwrap();
            let mut scan=crate::sftp::model::Scan::new(v.binding.owner.session,v.binding.owner.generation,v.target.clone(),false,crate::sftp::model::Sort::Name).unwrap();
            scan.push_entry(b"unknown attributes",crate::sftp::model::Attributes::default(),&Cancellation::default()).unwrap();
            let memory=child.lease(Kind::Sftp,std::mem::size_of::<service::Reply>()+128).unwrap();
            v.reply=Some(Tracked::new(Ok(service::Output::Directory(scan.finish(true).unwrap())),memory));v.selected=Some(0);v.error=None;
        }
        app.open_transfer_paths(false).unwrap();assert!(app.transfer_paths_paint());
        for _ in 0..16{let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Commit,&answer).unwrap());}
        let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Commit,b"xxxxxxxxxxxxxxxx").unwrap());drop(c);
        wait(||{app.tick();paths_form::tests::lookup_error(&app).is_some()});
        let c=app.protected_context().unwrap();app.protected_input(c.utf8_input(InputKind::Preedit,&answer).unwrap());drop(c);
        assert!(app.transfer_paths_paint());let lookup_counts=meter::peek();let lookup_budget=child.snapshot();
        let before_lookup=meter::peek();for _ in 0..200{assert!(app.transfer_paths_paint());}let after_lookup=meter::peek();assert_eq!(before_lookup.allocations,after_lookup.allocations);assert_eq!(before_lookup.reallocations,after_lookup.reallocations);
        let c=app.protected_context().unwrap();app.protected_input(c.command(crate::platform::protected_input::InputCommand::Cancel).unwrap());drop(c);
        paths_form::local_picker::tests::begin_probe(&mut app,&picker_dir);wait(||{app.tick();paths_form::local_picker::tests::loaded_probe(&app)});assert_eq!(paths_form::local_picker::tests::entries_probe(&app),512);assert!(paths_form::local_picker::tests::paint_probe(&mut app));
        let picker_counts=meter::peek();let picker_budget=child.snapshot();let before_picker=meter::peek();let mut picker_paint_ns=[0u64;200];for ns in &mut picker_paint_ns{let began=Instant::now();assert!(paths_form::local_picker::tests::paint_probe(&mut app));*ns=began.elapsed().as_nanos() as u64;}let after_picker=meter::peek();assert_eq!(before_picker.allocations,after_picker.allocations);assert_eq!(before_picker.reallocations,after_picker.reallocations);paths_form::local_picker::tests::cancel_probe(&mut app);
        app.prepare_remote_upload(&metadata_source,b"/never-created-cg-target").unwrap();
        wait(||{app.tick();app.last_draw=Instant::now()-FRAME_INTERVAL;let _=app.draw();app.sftp.as_ref().unwrap().review.as_ref().is_some_and(|r|!r.pending())});
        let prepare_counts=meter::peek();let prepare_budget=child.snapshot();app.event(Event::Key{key:Key::Escape,mods:Mods::default()});assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
        // 真实失败记录启动显式重试；源只读元数据检查完成后重新审阅，不产生文件任务。
        let old=app.submit_remote_transfer(service::TransferSelection::Upload{local:&metadata_source,remote:b"/never-created-cg-retry-target",length:33554432,replace:service::Replace::No,resume:None}).unwrap();
        wait(||{app.tick();app.sftp.as_ref().unwrap().files.iter().flatten().find(|f|f.ticket==old).unwrap().complete});
        let preparation=app.retry_remote_transfer(old).unwrap();assert_ne!(preparation,old);
        wait(||{app.tick();app.last_draw=Instant::now()-FRAME_INTERVAL;let _=app.draw();app.sftp.as_ref().unwrap().review.as_ref().is_some_and(|r|!r.pending())});
        assert!(review::tests::overwrite_is_off(&app));assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(),1);
        let retry_counts=meter::peek();let retry_budget=child.snapshot();let before_retry=meter::peek();for _ in 0..200{assert!(app.transfer_review_paint());}let after_retry=meter::peek();assert_eq!(before_retry.allocations,after_retry.allocations);assert_eq!(before_retry.reallocations,after_retry.reallocations);
        app.event(Event::Key{key:Key::Escape,mods:Mods::default()});assert!(app.forget_remote_transfer(old));
        // 小路径的66条真实文件DTO依次失败并取走结果，固定记录仍保留所有Status/路径。
        for _ in 0..66 {
            let ticket=app.submit_remote_transfer(service::TransferSelection::Upload{local:Path::new("/never-created-cg-source"),remote:b"/never-created-cg-target",length:1,replace:service::Replace::No,resume:None}).unwrap();
            wait(||{app.tick();app.sftp.as_ref().unwrap().files.iter().flatten().find(|f|f.ticket==ticket).unwrap().complete});
        }
        assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(),66);
        assert_eq!(app.submit_remote_transfer(service::TransferSelection::Upload{local:Path::new("/never-created-cg-source"),remote:b"/never-created-cg-target",length:1,replace:service::Replace::No,resume:None}).err(),Some(Failure::Full));
        let local=Path::new(<std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&review_raw));
        app.open_transfer_review(service::TransferSelection::Upload{local,remote:&review_raw,length:1,replace:service::Replace::No,resume:None}).unwrap();
        wait(||{app.tick();app.last_draw=Instant::now()-FRAME_INTERVAL;let _=app.draw();!app.sftp.as_ref().unwrap().review.as_ref().unwrap().pending()});
        assert!(app.transfer_review_paint());let review_counts=meter::peek();let review_budget=child.snapshot();let before_paint=meter::peek();for _ in 0..200{assert!(app.transfer_review_paint());}let after_paint=meter::peek();assert_eq!(before_paint.allocations,after_paint.allocations);assert_eq!(before_paint.reallocations,after_paint.reallocations);
        app.event(Event::Key{key:Key::Escape,mods:Mods::default()});assert!(!app.sftp_review_active());
        // 合成回收失败快照只测退出视图；真实回收失败/明确确认由回环夹具验证。
        let mut exit=transfer_exit::tests::snapshot(&mut app);transfer_exit::tests::paint_snapshot(&mut app,&mut exit);
        let exit_counts=meter::peek();let exit_budget=child.snapshot();let before_exit=meter::peek();for _ in 0..200{transfer_exit::tests::paint_snapshot(&mut app,&mut exit);}let after_exit=meter::peek();assert_eq!(before_exit.allocations,after_exit.allocations);assert_eq!(before_exit.reallocations,after_exit.reallocations);drop(exit);
        let(profiles,index)=app.panes[0].profile.as_ref().unwrap();let identity=Identity::new(profiles.clone(),*index,&child).unwrap();let(mut backend,front)=Backend::with_identity(owner,&child,Arc::new(||{}),identity).unwrap();backend.begin(PromptKind::Unspecified,&prompt,&Cancellation::default()).unwrap();app.open_authentication(front).unwrap();
        let source=app.protected_context().unwrap();app.protected_input(source.utf8_input(InputKind::Commit,&answer).unwrap());
        for _ in 0..512{app.tick();app.last_draw=Instant::now()-FRAME_INTERVAL;app.driver.last_draw=Instant::now()-FRAME_INTERVAL;let _=app.draw();if !app.authentication.as_ref().unwrap().controller.pending(){break;}}
        assert!(!app.authentication.as_ref().unwrap().controller.pending());samples[2]=sample(&parent,Some(&child),"modal");
        let mut keep=app.event(Event::CloseRequested);wait(||{if keep{keep=app.tick();}!keep});assert_eq!(app.sftp.as_ref().unwrap().shutdown,Shutdown::Complete(Cleanup::Confirmed));samples[3]=sample(&parent,Some(&child),"joined");
        let late=source.utf8_input(InputKind::Commit,&answer).unwrap();drop((backend,app,child));samples[4]=sample(&parent,Some(source.budget()),"workspace_dropped");drop(late);samples[5]=sample(&parent,Some(source.budget()),"late_input_dropped");drop(source);
        wait(||meter::peek().live==0);samples[6]=sample(&parent,None,"released");let counts=meter::stop();assert_eq!(counts.live,0);assert_eq!(parent.snapshot().kinds[Kind::Sftp as usize],0);
        eprintln!("WORKSPACE_PICKER live={} peak={} child_used={} input_used={} entries=512 paint_allocations={} paint_reallocations={}",picker_counts.live,picker_counts.peak,picker_budget.used,picker_budget.kinds[Kind::SftpInput as usize],after_picker.allocations-before_picker.allocations,after_picker.reallocations-before_picker.reallocations);
        eprintln!("WORKSPACE_PICKER_TIMES ns={:?}",picker_paint_ns);
        eprintln!("WORKSPACE_EXIT live={} peak={} child_used={} input_used={} paint_allocations={} paint_reallocations={}",exit_counts.live,exit_counts.peak,exit_budget.used,exit_budget.kinds[Kind::SftpInput as usize],after_exit.allocations-before_exit.allocations,after_exit.reallocations-before_exit.reallocations);
        eprintln!("WORKSPACE_RETRY live={} peak={} child_used={} input_used={} paint_allocations={} paint_reallocations={}",retry_counts.live,retry_counts.peak,retry_budget.used,retry_budget.kinds[Kind::SftpInput as usize],after_retry.allocations-before_retry.allocations,after_retry.reallocations-before_retry.reallocations);
        eprintln!("WORKSPACE_PREPARE live={} peak={} child_used={} input_used={} length=33554432",prepare_counts.live,prepare_counts.peak,prepare_budget.used,prepare_budget.kinds[Kind::SftpInput as usize]);
        eprintln!("WORKSPACE_PATHS live={} peak={} child_used={} input_used={} paint_allocations={} paint_reallocations={}",paths_counts.live,paths_counts.peak,paths_budget.used,paths_budget.kinds[Kind::SftpInput as usize],after_paths.allocations-before_paths.allocations,after_paths.reallocations-before_paths.reallocations);
        eprintln!("WORKSPACE_LOOKUP live={} peak={} child_used={} input_used={} paint_allocations={} paint_reallocations={}",lookup_counts.live,lookup_counts.peak,lookup_budget.used,lookup_budget.kinds[Kind::SftpInput as usize],after_lookup.allocations-before_lookup.allocations,after_lookup.reallocations-before_lookup.reallocations);
        eprintln!("WORKSPACE_REVIEW live={} peak={} child_used={} input_used={} paint_allocations={} paint_reallocations={}",review_counts.live,review_counts.peak,review_budget.used,review_budget.kinds[Kind::SftpInput as usize],after_paint.allocations-before_paint.allocations,after_paint.reallocations-before_paint.reallocations);
        for s in samples{eprintln!("WORKSPACE_ALLOC step={} live={} peak={} parent_used={} parent_peak={} child_used={} allocations={} reallocations={} root_total={}",s.step,s.counts.live,s.counts.peak,s.parent_used,s.parent_peak,s.child_used,s.counts.allocations,s.counts.reallocations,s.root_total);}
    }
}
