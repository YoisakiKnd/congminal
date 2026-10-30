use super::{tests::*, *};
use crate::{
    platform::protected_input::InputKind,
    resources::{Budget, Scope},
    ssh::auth::{Backend, Identity, Kind as PromptKind, controller::Phase},
};

fn upload<'a>(local: &'a Path, remote: &'a [u8]) -> service::TransferSelection<'a> {
    service::TransferSelection::Upload { local, remote, length: 1, replace: service::Replace::No, resume: None }
}
#[test]
fn file_record_survives_refresh_hide_and_other_pane_then_explicit_release() {
    let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
    let _scope = Scope::enter(parent.clone());
    let mut app = workspace();
    app.start_remote_files(paths(), b".").unwrap();
    let ticket = app.submit_remote_transfer(upload(Path::new("/unused-source"), b"/unused-target")).unwrap();
    let status = app.remote_transfer_status(ticket).unwrap().clone();
    let generation = app.panes[0].auth_generation;
    for _ in 0..4 {
        app.submit_remote_directory(b".", false).unwrap();
        assert_eq!(app.panes[0].auth_generation, generation);
        assert!(!app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().cancelled);
    }
    app.left_page = LeftPage::Sessions;
    app.sftp_tick();
    assert!(app.sftp.as_ref().unwrap().view.is_none());
    assert!(app.remote_transfer_status(ticket).is_some());
    let profile = app.panes[0].profile.clone();
    app.add(None);
    let index = app.index(app.selected).unwrap();
    app.panes[index].profile = profile;
    until(|| {
        app.tick();
        app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete
    });
    assert!(!app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().cancelled);
    assert!(status.snapshot().end.is_some());
    assert!(!app.cancel_remote_transfer(ticket));
    assert!(app.forget_remote_transfer(ticket));
    assert!(app.remote_transfer_status(ticket).is_none());
    close(&mut app);
    drop(app);
    assert!(parent.snapshot().kinds[Kind::Sftp as usize] > 0, "retained report keeps parent credit");
    drop(status);
    released(&parent);
}
#[test]
fn same_owner_authentication_is_bound_to_task_and_parked_view_rejects_old_input() {
    let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
    let _scope = Scope::enter(parent.clone());
    let mut app = workspace();
    app.start_remote_files(paths(), b".").unwrap();
    let first = app.submit_remote_transfer(upload(Path::new("/unused-source"), b"/unused-target-one")).unwrap();
    let second = app.submit_remote_transfer(upload(Path::new("/unused-source"), b"/unused-target-two")).unwrap();
    assert_eq!(first.source, second.source);
    let state = app.sftp.as_ref().unwrap();
    let file = state.files.iter().flatten().find(|f| f.ticket == second).unwrap();
    let identity = Identity::new(file.binding.profiles.clone(), file.binding.profile, state.service.budget()).unwrap();
    // 合成控制器只验证界面迁移；真实发布前端由下方回环用例覆盖。
    let (mut backend, front) =
        Backend::with_identity(file.binding.owner, state.service.budget(), Arc::new(|| {}), identity).unwrap();
    let challenge = backend.begin(PromptKind::Secret, b"fixture challenge", &Cancellation::default()).unwrap();
    app.open_authentication(front).unwrap();
    app.authentication.as_mut().unwrap().task = Some(second);
    let old = app.protected_context().unwrap();
    app.protected_input(old.utf8_input(InputKind::Commit, b"must be cleared").unwrap());
    app.finish_remote_authentication(first);
    assert_eq!(app.authentication.as_ref().unwrap().task, Some(second));
    app.submit_remote_directory(b".", false).unwrap();
    assert_eq!(app.authentication.as_ref().unwrap().task, Some(second));
    assert!(app.park_remote_authentication());
    assert!(app.authentication.is_none());
    assert_eq!(app.sftp.as_ref().unwrap().parked.iter().flatten().count(), 1);
    let mut state = app.sftp.take().unwrap();
    assert!(app.remote_transfer_authentication(&mut state));
    app.sftp = Some(state);
    let fresh = app.protected_context().unwrap();
    assert!(!old.same_source(&fresh));
    app.protected_input(old.utf8_input(InputKind::Commit, b"late must be rejected").unwrap());
    assert!(backend.take(challenge, &Cancellation::default()).unwrap().is_none());
    assert_eq!(app.authentication.as_ref().unwrap().task, Some(second));
    assert!(app.cancel_remote_transfer(second));
    assert!(app.authentication.is_none());
    assert!(backend.take(challenge, &Cancellation::default()).is_err());
    close(&mut app);
    drop((old, fresh, backend, app));
    released(&parent);
}
#[test]
fn fixed_completed_records_refuse_more_until_explicitly_forgotten_and_stale_profile_cancels() {
    let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
    let _scope = Scope::enter(parent.clone());
    let mut app = workspace();
    app.start_remote_files(paths(), b".").unwrap();
    app.left_page = LeftPage::Sessions;
    let mut first = None;
    for _ in 0..SLOTS {
        let ticket = app.submit_remote_transfer(upload(Path::new("/unused-source"), b"/unused-target")).unwrap();
        first.get_or_insert(ticket);
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == ticket).unwrap().complete
        });
    }
    let stats = app.sftp.as_ref().unwrap().service.stats();
    assert_eq!(
        app.submit_remote_transfer(upload(Path::new("/unused-source"), b"/unused-target")).err(),
        Some(Failure::Full)
    );
    let after = app.sftp.as_ref().unwrap().service.stats();
    assert_eq!((after.running, after.queued, after.results), (stats.running, stats.queued, stats.results));
    assert!(app.forget_remote_transfer(first.unwrap()));
    let next = app.submit_remote_transfer(upload(Path::new("/unused-source"), b"/unused-target")).unwrap();
    assert!(!app.forget_remote_transfer(next));
    app.panes[0].profile = Some((profiles(), 0));
    app.sftp_tick();
    let file = app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == next).unwrap();
    assert!(file.cancelled);
    assert!(app.remote_transfer_status(next).is_some());
    close(&mut app);
    drop(app);
    released(&parent);
}

#[test]
#[ignore = "隔离回环Workspace文件任务/前端暂存；无原生窗口和目标确认UI"]
fn isolated_workspace_file_task_survives_directory_refresh_and_background_switch() {
    use std::{
        fs::File,
        io::{Read, Write},
        os::unix::ffi::OsStrExt,
    };
    let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
    let helper = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_HELPER").unwrap());
    let configuration = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_CONFIG").unwrap());
    let scenario = std::env::var("CONGMINAL_AUTH_SCENARIO").unwrap();
    assert!(
        [
            "upload_background",
            "download_background",
            "background_cancel",
            "upload_replace_review",
            "download_replace_review",
            "upload_refuse_review",
            "download_refuse_review",
            "upload_stale_prepare",
            "upload_cancel_before_auth",
            "upload_retry_review",
            "download_retry_review",
            "download_lookup",
            "download_lookup_cancel",
            "download_lookup_form_cancel",
            "download_lookup_stale",
            "download_lookup_symlink"
        ]
        .contains(&scenario.as_str())
    );
    let folder = root.join(&scenario);
    std::fs::create_dir(&folder).unwrap();
    let length = 32 * 1024 * 1024u64;
    let source_path = folder.join("source ;$()-文");
    let target = folder.join("target ;$()-文");
    let retry = scenario.contains("retry");
    let refusing = scenario.contains("refuse") || retry;
    if scenario.contains("replace") || refusing {
        std::fs::write(&target, b"KEEP ORIGINAL").unwrap();
    }
    let buffer = [37u8; 65536];
    let mut source_file = File::create(&source_path).unwrap();
    for _ in 0..length / buffer.len() as u64 {
        source_file.write_all(&buffer).unwrap();
    }
    drop(source_file);
    if scenario == "download_lookup_symlink" {
        let original = folder.join("actual regular file");
        std::fs::rename(&source_path, &original).unwrap();
        std::os::unix::fs::symlink(&original, &source_path).unwrap();
    }
    let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
    let _scope = Scope::enter(parent.clone());
    let mut app = workspace();
    // 工具配置别名为fixture；仅本地cat PTY携带SSH配置引用，用于验证输入隔离。
    let (mut config, mut warnings) = Config::parse("[connections.fixture]\nkind='ssh'\nhost='fixture'\n");
    config.admit_connections(&mut warnings);
    assert!(warnings.is_empty());
    app.panes[0].profile = Some((config.connections, 0));
    assert!(app.enable());
    app.start_remote_files(
        Paths { configuration: service::Config::File(&configuration), helper: &helper, temporary: &root },
        folder.as_os_str().as_bytes(),
    )
    .unwrap();
    let start = Instant::now();
    let mut directory_requests = 0;
    loop {
        assert!(start.elapsed() < Duration::from_secs(18), "directory before path form deadline");
        super::review::tests::draw(&mut app);
        if app
            .authentication
            .as_ref()
            .is_some_and(|a| a.controller.phase() == Phase::Editing && !a.controller.pending())
        {
            send_fixture_answer(&mut app);
            directory_requests += 1;
        }
        if app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading) {
            break;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(directory_requests, 1);
    if scenario.starts_with("download") {
        let view = app.sftp.as_ref().unwrap().view.as_ref().unwrap();
        let selected = view
            .listing()
            .unwrap()
            .entries
            .iter()
            .position(|e| &*e.name == source_path.file_name().unwrap().as_bytes())
            .unwrap();
        if scenario.starts_with("download_lookup") {
            // 注入合法的缺失列表字段；实际LSTAT仍通过独立认证的真实OpenSSH/SFTP。
            let view = app.sftp.as_mut().unwrap().view.as_mut().unwrap();
            let Ok(service::Output::Directory(listing)) = &mut view.reply.as_mut().unwrap().data else { panic!() };
            let entry = &mut listing.entries[selected];
            entry.attrs.permissions = None;
            entry.attrs.times = None;
            entry.kind = FileKind::Unknown;
            if scenario == "download_lookup_symlink" {
                entry.attrs.size = None;
            }
            if scenario == "download_lookup_stale" {
                std::fs::OpenOptions::new().write(true).open(&source_path).unwrap().set_len(length - 1).unwrap();
            }
        }
        assert!(app.sftp_click(selected + 4, 0));
        assert!(app.sftp_click(3, 4));
        super::paths_form::tests::commit(&mut app, target.to_str().unwrap());
        if scenario == "download_lookup_form_cancel" {
            let ticket = super::paths_form::tests::lookup_ticket(&app);
            let retired = app.sftp.as_ref().unwrap().service.stats().cancelled_queued;
            let context = app.protected_context().unwrap();
            app.protected_input(context.command(crate::platform::protected_input::InputCommand::Cancel).unwrap());
            drop(context);
            assert!(!app.sftp.as_ref().unwrap().paths_active());
            let start = Instant::now();
            let cancel_mode = loop {
                assert!(start.elapsed() < Duration::from_secs(18), "cancelled lookup cleanup deadline");
                if let Some(c) = app.sftp.as_ref().unwrap().service.take() {
                    assert_eq!(c.ticket, ticket);
                    assert_eq!(c.cleanup, Cleanup::Confirmed);
                    match &c.result {
                        Err(service::TaskError::Cancelled) => {}
                        Ok(reply) => assert!(matches!(reply.data, Err(Failure::Cancelled))),
                        _ => panic!("cancelled read-only query returned a different outcome"),
                    }
                    break "running_result";
                }
                let stats = app.sftp.as_ref().unwrap().service.stats();
                // 回收线程直接析构被取消的等待项，不为它生成完成消息；最终仍须join。
                if stats.queued == 0
                    && stats.running == 0
                    && stats.results == 0
                    && stats.cancelled_queued == retired + 1
                {
                    break "queued_retired";
                }
                std::thread::sleep(Duration::from_millis(1));
            };
            eprintln!("SFTP_LOOKUP_RESULT scenario={scenario} requests=0 failure=Some(Cancelled) mode={cancel_mode}");
            end_lookup_case(app, &parent, &scenario, &target, 0);
            return;
        }
        if scenario.starts_with("download_lookup") {
            let initial = app.protected_context().unwrap();
            let mut challenges = 0;
            let start = Instant::now();
            loop {
                assert!(start.elapsed() < Duration::from_secs(18), "attribute lookup deadline");
                super::review::tests::draw(&mut app);
                if app
                    .authentication
                    .as_ref()
                    .is_some_and(|a| a.controller.phase() == Phase::Editing && !a.controller.pending())
                {
                    if scenario == "download_lookup_cancel" {
                        let context = app.protected_context().unwrap();
                        app.protected_input(
                            context.command(crate::platform::protected_input::InputCommand::Cancel).unwrap(),
                        );
                    } else {
                        send_fixture_answer(&mut app);
                    }
                    challenges += 1;
                }
                if !super::paths_form::tests::lookup_pending(&app)
                    || super::paths_form::tests::lookup_error(&app).is_some()
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            assert_eq!(challenges, 1);
            // 认证切换前的表单输入不能在恢复后继续应用。
            app.protected_input(initial.utf8_input(InputKind::Commit, b"OLD_LOOKUP_FORM").unwrap());
            drop(initial);
            let failure = super::paths_form::tests::lookup_error(&app);
            eprintln!("SFTP_LOOKUP_RESULT scenario={scenario} requests={challenges} failure={failure:?}");
            if scenario != "download_lookup" {
                assert!(failure.is_some());
                if scenario == "download_lookup_stale" {
                    assert_eq!(failure, Some(Failure::Changed));
                }
                if scenario == "download_lookup_symlink" {
                    assert_eq!(failure, Some(Failure::Invalid));
                }
                end_lookup_case(app, &parent, &scenario, &target, challenges);
                return;
            }
        }
        super::paths_form::tests::next(&mut app);
        assert!(app.sftp.as_ref().unwrap().review.is_some());
    } else {
        assert!(app.sftp_click(3, 3));
        super::paths_form::tests::commit(&mut app, source_path.to_str().unwrap());
        let context = app.protected_context().unwrap();
        app.protected_input(context.command(crate::platform::protected_input::InputCommand::NextField(false)).unwrap());
        let context = app.protected_context().unwrap();
        app.protected_input(context.command(crate::platform::protected_input::InputCommand::SelectAll).unwrap());
        super::paths_form::tests::commit(&mut app, target.file_name().unwrap().to_str().unwrap());
        super::paths_form::tests::next(&mut app);
        let deadline = Instant::now() + Duration::from_secs(5);
        while app.sftp.as_ref().unwrap().review.is_none() {
            assert!(app.tick());
            assert!(Instant::now() < deadline);
            std::thread::yield_now();
        }
    }
    super::review::tests::confirm(&mut app, scenario.contains("replace"));
    let ticket = app.sftp.as_ref().unwrap().files.iter().flatten().next().unwrap().ticket;
    let early_cancel = scenario == "upload_cancel_before_auth";
    if early_cancel {
        assert!(app.cancel_remote_transfer(ticket));
    }
    // 此夹具随后专测文件认证暂存/后台切换；隐藏目录，避免换窗格自动发起额外目录认证。
    app.left_page = LeftPage::Sessions;
    app.sftp_tick();
    let stale = scenario == "upload_stale_prepare";
    if stale {
        std::fs::rename(&source_path, folder.join("original inode retained")).unwrap();
        let mut replacement = File::create(&source_path).unwrap();
        for _ in 0..length / buffer.len() as u64 {
            replacement.write_all(&buffer).unwrap();
        }
        drop(replacement);
    }

    let status = app.remote_transfer_status(ticket).unwrap().clone();
    if early_cancel {
        let start = Instant::now();
        let mut max_tick = 0;
        loop {
            let tick = Instant::now();
            assert!(app.tick());
            max_tick = max_tick.max(tick.elapsed().as_nanos());
            if app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == ticket).unwrap().complete {
                break;
            }
            assert!(start.elapsed() < Duration::from_secs(18));
            std::thread::sleep(Duration::from_millis(1));
        }
        let snapshot = status.snapshot();
        assert_eq!(snapshot.bytes, 0);
        assert_eq!(snapshot.publication, service::TransferPublication::NotAttempted);
        assert!(matches!(snapshot.end, Some(service::TransferEnd::NotStarted | service::TransferEnd::Cancelled)));
        assert_eq!(snapshot.cleanup, Cleanup::Confirmed);
        assert!(!target.exists());
        assert!(status.partial().is_none());
        super::transfer_notifications::tests::assert_notice(
            &app,
            ticket,
            crate::notifications::TransferResult::Cancelled,
        );
        assert!(app.forget_remote_transfer(ticket));
        close(&mut app);
        let peak = app.sftp.as_ref().unwrap().service.budget().snapshot().peak;
        drop(app);
        drop(status);
        released(&parent);
        eprintln!("SFTP_NOTICE_RESULT scenario={scenario} count=1 result=Cancelled");
        eprintln!(
            "SFTP_WORKSPACE_RESULT scenario={scenario} requests=0 peak={peak} final=0 cleanup=ok max_tick_ns={max_tick}"
        );
        return;
    }
    let mut switched_auth = false;
    let mut switched_stream = false;
    let mut requests = 0;
    let mut other = None;
    let mut max_tick = 0;
    let start = Instant::now();
    loop {
        assert!(start.elapsed() < Duration::from_secs(18), "workspace transfer deadline");
        let tick_start = Instant::now();
        assert!(app.tick());
        max_tick = max_tick.max(tick_start.elapsed().as_nanos());
        if app.authentication.as_ref().is_some_and(|a| a.controller.phase() == Phase::Editing) {
            app.last_draw = Instant::now() - FRAME_INTERVAL;
            app.driver.last_draw = Instant::now() - FRAME_INTERVAL;
            let _ = app.draw();
            if app.authentication.as_ref().unwrap().controller.pending() {
                continue;
            }
            if !switched_auth {
                let old = app.protected_context().unwrap();
                app.protected_input(old.utf8_input(InputKind::Commit, b"clear this fixture text").unwrap());
                let profile = app.panes[0].profile.clone();
                app.add(None);
                other = Some(app.selected);
                let index = app.index(app.selected).unwrap();
                app.panes[index].profile = profile;
                app.sftp_tick();
                assert!(app.authentication.is_none());
                assert_eq!(app.sftp.as_ref().unwrap().parked.iter().flatten().count(), 1);
                assert!(app.select(1));
                app.sftp_tick();
                assert!(app.authentication.is_some());
                assert!(!old.same_source(&app.protected_context().unwrap()));
                app.protected_input(old.utf8_input(InputKind::Commit, b"STALE_INPUT").unwrap());
                switched_auth = true;
                continue;
            }
            send_fixture_answer(&mut app);
            requests += 1;
        }
        let snapshot = status.snapshot();
        if !switched_stream && snapshot.bytes > 0 && snapshot.bytes < length {
            let generation = app.panes[0].auth_generation;
            app.submit_remote_directory(root.join("remote").as_os_str().as_bytes(), false).unwrap();
            assert_eq!(app.panes[0].auth_generation, generation);
            app.left_page = LeftPage::Sessions;
            app.sftp_tick();
            assert!(app.select(other.unwrap()));
            app.sftp_tick();
            assert!(!app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().cancelled);
            if scenario == "background_cancel" {
                app.last_draw = Instant::now() - FRAME_INTERVAL;
                let _ = app.draw();
                let r = app.right_rect;
                let h = app.driver.renderer.cell_size().1;
                let click = |app: &mut Workspace, x: f64, y: f64| {
                    app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
                    app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
                };
                click(&mut app, (r.x + r.w / 2) as f64, (r.y + 4 * h + h / 2) as f64);
                app.last_draw = Instant::now() - FRAME_INTERVAL;
                let _ = app.draw();
                click(&mut app, (r.x + r.w / 6) as f64, (r.y + h + h / 2) as f64);
                assert!(app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().cancelled);
            }
            switched_stream = true;
        }
        if app.sftp.as_ref().unwrap().files[0].as_ref().unwrap().complete {
            break;
        }
        assert!(start.elapsed() < Duration::from_secs(18));
        std::thread::sleep(Duration::from_millis(1));
    }
    assert!(switched_auth && (switched_stream || stale));
    assert_eq!(requests, 1);
    let snapshot = status.snapshot();
    assert_eq!(snapshot.cleanup, Cleanup::Confirmed);
    if stale {
        assert_eq!(snapshot.end, Some(service::TransferEnd::Failed));
        assert_eq!(snapshot.failure, Some(service::TransferFailure::Changed));
        assert_eq!(snapshot.bytes, 0);
        assert_eq!(snapshot.publication, service::TransferPublication::NotAttempted);
        assert!(status.partial().is_none());
        assert!(!target.exists());
    } else if scenario == "background_cancel" {
        assert!(snapshot.bytes < length);
        assert!(!target.exists());
        assert_eq!(snapshot.end, Some(service::TransferEnd::Cancelled));
    } else if refusing {
        assert_eq!(snapshot.end, Some(service::TransferEnd::Failed));
        assert_eq!(std::fs::read(&target).unwrap(), b"KEEP ORIGINAL");
    } else {
        assert_eq!(snapshot.bytes, length);
        assert_eq!(snapshot.end, Some(service::TransferEnd::Completed));
        let mut file = File::open(&target).unwrap();
        let mut bytes = [0u8; 65536];
        let mut total = 0;
        loop {
            let n = file.read(&mut bytes).unwrap();
            if n == 0 {
                break;
            }
            assert!(bytes[..n].iter().all(|b| *b == 37));
            total += n as u64;
        }
        assert_eq!(total, length);
    }
    let notice = if stale || refusing {
        crate::notifications::TransferResult::Failed
    } else if scenario == "background_cancel" {
        crate::notifications::TransferResult::Cancelled
    } else {
        crate::notifications::TransferResult::Published
    };
    super::transfer_notifications::tests::assert_notice(&app, ticket, notice);
    eprintln!("SFTP_NOTICE_RESULT scenario={scenario} count=1 result={notice:?}");
    if retry {
        // 原请求已失败并保留目标；修改源后，重试必须取得新版本再明确确认覆盖。
        let mut source_file = File::create(&source_path).unwrap();
        let buffer = [41u8; 65536];
        for _ in 0..length / buffer.len() as u64 {
            source_file.write_all(&buffer).unwrap();
        }
        source_file.write_all(&buffer[..17]).unwrap();
        drop(source_file);
        assert!(app.select(1));
        app.show_transfer_list();
        super::review::tests::draw(&mut app);
        let r = app.right_rect;
        let h = app.driver.renderer.cell_size().1;
        let click = |app: &mut Workspace, x: f64, y: f64| {
            app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
            app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
        };
        click(&mut app, (r.x + r.w / 2) as f64, (r.y + 4 * h + h / 2) as f64);
        super::review::tests::draw(&mut app);
        click(&mut app, (r.x + 3 * r.w / 4) as f64, (r.y + 2 * h + h / 2) as f64);
        assert!(app.sftp.as_ref().unwrap().preparation.is_some(), "visible retry starts source inspection");
        assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(), 1);
        let mut retry_requests = 0;
        let start = Instant::now();
        while app.sftp.as_ref().unwrap().review.is_none() {
            assert!(start.elapsed() < Duration::from_secs(18), "retry source inspection deadline");
            super::review::tests::draw(&mut app);
            if app
                .authentication
                .as_ref()
                .is_some_and(|a| a.controller.phase() == Phase::Editing && !a.controller.pending())
            {
                send_fixture_answer(&mut app);
                retry_requests += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(std::fs::read(&target).unwrap(), b"KEEP ORIGINAL", "inspection must not write target");
        assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(), 1);
        assert!(super::review::tests::overwrite_is_off(&app));
        super::review::tests::confirm(&mut app, true);
        let new_ticket = app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket != ticket).unwrap().ticket;
        let new_status = app.remote_transfer_status(new_ticket).unwrap().clone();
        assert_eq!(new_status.snapshot().length, length + 17, "retry uses the new source version");
        while !app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == new_ticket).unwrap().complete {
            assert!(start.elapsed() < Duration::from_secs(30), "retry transfer deadline");
            super::review::tests::draw(&mut app);
            if app
                .authentication
                .as_ref()
                .is_some_and(|a| a.controller.phase() == Phase::Editing && !a.controller.pending())
            {
                send_fixture_answer(&mut app);
                retry_requests += 1;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(retry_requests, if scenario.starts_with("download") { 2 } else { 1 });
        let s = new_status.snapshot();
        assert_eq!(s.end, Some(service::TransferEnd::Completed));
        assert_eq!(s.cleanup, Cleanup::Confirmed);
        let mut file = File::open(&target).unwrap();
        let mut bytes = [0u8; 65536];
        let mut total = 0;
        loop {
            let n = file.read(&mut bytes).unwrap();
            if n == 0 {
                break;
            }
            assert!(bytes[..n].iter().all(|b| *b == 41));
            total += n as u64;
        }
        assert_eq!(total, length + 17);
        super::transfer_notifications::tests::assert_notice(
            &app,
            new_ticket,
            crate::notifications::TransferResult::Published,
        );
        assert!(
            !app.sftp.as_ref().unwrap().files.iter().flatten().find(|f| f.ticket == new_ticket).unwrap().retryable()
        );
        assert!(app.forget_remote_transfer(new_ticket));
        drop(new_status);
        eprintln!(
            "SFTP_RETRY_RESULT scenario={scenario} requests={retry_requests} version=fresh old_record=retained target=verified"
        );
    }
    assert!(app.forget_remote_transfer(ticket));
    close(&mut app);
    let peak = app.sftp.as_ref().unwrap().service.budget().snapshot().peak;
    drop(app);
    assert!(parent.snapshot().kinds[Kind::Sftp as usize] > 0);
    drop(status);
    released(&parent);
    eprintln!(
        "SFTP_WORKSPACE_RESULT scenario={scenario} requests={requests} peak={peak} final=0 cleanup=ok max_tick_ns={max_tick}"
    );
}

fn end_lookup_case(mut app: Workspace, parent: &Arc<Budget>, scenario: &str, target: &Path, requests: usize) {
    assert!(app.sftp.as_ref().unwrap().review.is_none());
    assert!(app.sftp.as_ref().unwrap().files.iter().all(Option::is_none));
    assert!(!target.exists());
    close(&mut app);
    let peak = app.sftp.as_ref().unwrap().service.budget().snapshot().peak;
    drop(app);
    released(parent);
    eprintln!(
        "SFTP_WORKSPACE_RESULT scenario={scenario} requests={requests} peak={peak} final=0 cleanup=ok max_tick_ns=0"
    );
}

pub(in crate::workspace) fn send_fixture_answer(app: &mut Workspace) {
    let input = app.protected_context().unwrap();
    app.protected_input(input.utf8_input(InputKind::Commit, b"fixture only").unwrap());
    app.last_draw = Instant::now() - FRAME_INTERVAL;
    let _ = app.draw();
    let area = app.authentication.as_ref().unwrap().controller.caret().unwrap();
    let (cw, ch) = app.driver.renderer.cell_size();
    let bounds = Workspace::authentication_bounds(&app.frame, &app.driver.renderer);
    let (x, y) = ((bounds[0] + 16 * cw) as f64, (area[1] + 2 * ch + ch / 2) as f64);
    app.event(Event::MouseDown { button: MouseButton::Left, x, y, mods: Mods::default() });
    app.event(Event::MouseUp { button: MouseButton::Left, x, y, mods: Mods::default() });
}
