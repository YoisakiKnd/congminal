//! 显式启用的预览入口；未点击时没有 SFTP 池、工作器或连接。
use super::*;
#[derive(Default)]
pub(in crate::workspace) struct Entry {
    hit: Option<(Rect, u64, u64)>,
    error: Option<Failure>,
}
impl Workspace {
    pub(super) fn sftp_entry_paint(&mut self, r: Rect, h: usize) -> bool {
        self.sftp_entry.hit = None;
        let enabled = self.driver.config.sftp.enabled;
        for (i, text) in [
            "SFTP · 独立文件连接",
            if !enabled {
                "此配置未启用 SFTP 预览"
            } else if self.sftp_entry.error.is_some() {
                "未连接，请检查 helper/配置或额度"
            } else {
                "点击连接；认证在独立面板完成"
            },
        ]
        .into_iter()
        .enumerate()
        {
            if r.h >= (i + 2) * h {
                self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + (i + 1) * h, r.w, h], text, false);
            }
        }
        let rect = Rect { x: r.x, y: r.y + 3 * h, w: r.w, h };
        if enabled
            && r.h >= 4 * h
            && self.driver.renderer.auth_text(&mut self.frame, [rect.x, rect.y, rect.w, h], "[连接 SFTP]", true)
        {
            self.sftp_entry.hit = Some((r, self.selected, self.driver.renderer.generation()));
        }
        true
    }
    pub(super) fn sftp_entry_click(&mut self, row: usize) -> bool {
        if row == 3
            && self.window_focus
            && self.driver.config.sftp.enabled
            && self.sftp_entry.hit == Some((self.left_rect, self.selected, self.driver.renderer.generation()))
        {
            let result = self.connect_sftp_preview();
            self.sftp_entry.error = result.err();
            self.sftp_entry.hit = None;
            self.chrome_dirty = true;
        }
        true
    }
    pub(in crate::workspace) fn connect_sftp_preview(&mut self) -> Result<Ticket, Failure> {
        if !self.driver.config.sftp.enabled {
            return Err(Failure::Invalid);
        }
        // 覆盖 current_exe/path 的有界临时副本；子池建立后另由 Options 拥有。
        let _memory = crate::resources::current().lease(Kind::Sftp, 128 * 1024).map_err(|_| Failure::Budget)?;
        let helper = match &self.driver.config.sftp.helper {
            Some(path) => path.clone(),
            None => {
                let mut path = std::env::current_exe().map_err(|_| Failure::Invalid)?;
                path.set_file_name(if cfg!(windows) { "congminal-ssh-helper.exe" } else { "congminal-ssh-helper" });
                path
            }
        };
        let configuration = self.driver.config.sftp.ssh_config.clone();
        #[cfg(windows)]
        let temporary = crate::ssh::auth::ticket::windows::temporary().map_err(|_| Failure::Invalid)?;
        #[cfg(unix)]
        let temporary = PathBuf::from("/tmp");
        self.start_remote_files(
            Paths {
                configuration: configuration.as_deref().map_or(service::Config::System, service::Config::File),
                helper: &helper,
                temporary: &temporary,
            },
            b".",
        )
    }
}
#[cfg(all(test, unix))]
mod tests {
    use super::super::tests::{close, released, until, workspace};
    use super::*;
    use crate::resources::{Budget, Scope};
    fn show(app: &mut Workspace) {
        assert!(app.enable());
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Files;
        app.last_draw = Instant::now() - FRAME_INTERVAL;
        let _ = app.draw();
    }
    #[test]
    fn disabled_preview_starts_no_pool_and_ssh_files_never_fall_back_to_local_directory() {
        let mut app = workspace();
        show(&mut app);
        assert!(app.sftp_route());
        assert!(app.sftp_entry.hit.is_none());
        assert_eq!(app.connect_sftp_preview().err(), Some(Failure::Invalid));
        for _ in 0..20 {
            app.tick();
        }
        assert!(app.sftp.is_none());
        assert!(!app.local_files.loading && app.local_files.listing.is_none());
    }
    #[test]
    fn missing_helper_fails_after_explicit_entry_and_joined_close_releases_the_pool() {
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        app.driver.config.sftp.enabled = true;
        app.driver.config.sftp.helper = Some(PathBuf::from("/congminal-nonexistent-preview-helper"));
        show(&mut app);
        assert!(app.sftp.is_none());
        assert!(app.sftp_entry.hit.is_some());
        app.sftp_entry_click(3);
        assert!(app.sftp.is_some());
        until(|| {
            app.tick();
            app.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading)
        });
        assert!(app.sftp.as_ref().unwrap().view.as_ref().unwrap().error.is_some());
        assert!(app.authentication.is_none());
        close(&mut app);
        drop(app);
        released(&parent);
    }
    #[test]
    fn stale_owner_or_geometry_cannot_start_an_entry_that_was_not_drawn_for_it() {
        let mut app = workspace();
        app.driver.config.sftp.enabled = true;
        show(&mut app);
        assert!(app.sftp_entry.hit.is_some());
        let rect = app.left_rect;
        app.left_rect.w += 1;
        app.sftp_entry_click(3);
        assert!(app.sftp.is_none());
        app.left_rect = rect;
        let profile = app.panes[0].profile.clone();
        app.add(None);
        let index = app.index(app.selected).unwrap();
        app.panes[index].profile = profile;
        app.sftp_entry_click(3);
        assert!(app.sftp.is_none());
    }
    #[test]
    #[ignore = "隔离回环预览入口；软件绘制与显式焦点，不代表原生窗口"]
    fn isolated_preview_entry_lists_uploads_downloads_and_joins() {
        use super::super::{paths_form, review, transfer_tests};
        use crate::ssh::auth::controller::Phase;
        let root = PathBuf::from(std::env::var_os("CONGMINAL_AUTH_ROOT").unwrap());
        let helper = std::env::var("CONGMINAL_AUTH_HELPER").unwrap();
        let ssh_config = std::env::var("CONGMINAL_AUTH_CONFIG").unwrap();
        let parent = Budget::new(256 * 1024 * 1024, 1024 * 1024 * 1024);
        let _scope = Scope::enter(parent.clone());
        let mut app = workspace();
        let (config, warnings) =
            Config::parse(&format!("[sftp]\nenabled=true\nhelper='{helper}'\nssh-config='{ssh_config}'\n"));
        assert!(warnings.is_empty());
        app.driver.config.sftp = config.sftp;
        show(&mut app);
        assert!(app.sftp.is_none());
        app.sftp_entry_click(3);
        assert!(app.sftp.is_some());
        let mut requests = 0;
        let mut run = |app: &mut Workspace, done: &dyn Fn(&Workspace) -> bool| {
            let start = Instant::now();
            loop {
                assert!(start.elapsed() < Duration::from_secs(15));
                review::tests::draw(app);
                if app
                    .authentication
                    .as_ref()
                    .is_some_and(|a| a.controller.phase() == Phase::Editing && !a.controller.pending())
                {
                    transfer_tests::send_fixture_answer(app);
                    requests += 1;
                }
                if done(app) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        };
        run(&mut app, &|a| a.sftp.as_ref().unwrap().view.as_ref().is_some_and(|v| !v.loading));
        let view = app.sftp.as_ref().unwrap().view.as_ref().unwrap();
        assert_eq!(view.target.bytes(), root.join("remote").as_os_str().as_encoded_bytes());
        assert!(view.error.is_none());
        assert!(app.local_files.listing.is_none());
        let source = root.join("source-文.txt");
        let target = root.join("remote/entry-upload.txt");
        std::fs::write(&source, b"preview entry upload\n").unwrap();
        app.open_transfer_paths(true).unwrap();
        paths_form::local_picker::tests::choose_from(&mut app, &root, source.file_name().unwrap());
        let context = app.protected_context().unwrap();
        app.protected_input(context.command(crate::platform::protected_input::InputCommand::NextField(false)).unwrap());
        drop(context);
        let context = app.protected_context().unwrap();
        app.protected_input(context.command(crate::platform::protected_input::InputCommand::SelectAll).unwrap());
        drop(context);
        paths_form::tests::commit(&mut app, "entry-upload.txt");
        paths_form::tests::next(&mut app);
        run(&mut app, &|a| a.sftp.as_ref().unwrap().review.is_some());
        review::tests::confirm(&mut app, false);
        run(&mut app, &|a| a.sftp.as_ref().unwrap().files.iter().flatten().all(|f| f.complete));
        assert_eq!(std::fs::read(&target).unwrap(), b"preview entry upload\n");
        let view = app.sftp.as_mut().unwrap().view.as_mut().unwrap();
        view.selected = Some(view.listing().unwrap().entries.iter().position(|e| e.name == b"fixture.txt").unwrap());
        app.open_transfer_paths(false).unwrap();
        let download = root.join("entry-download.txt");
        paths_form::tests::commit(&mut app, download.to_str().unwrap());
        paths_form::tests::next(&mut app);
        assert!(app.sftp.as_ref().unwrap().review.is_some());
        review::tests::confirm(&mut app, false);
        run(&mut app, &|a| a.sftp.as_ref().unwrap().files.iter().flatten().all(|f| f.complete));
        assert_eq!(std::fs::read(&download).unwrap(), b"generated fixture only");
        assert_eq!(requests, 3);
        assert_eq!(app.sftp.as_ref().unwrap().files.iter().flatten().count(), 2);
        close(&mut app);
        let peak = app.sftp.as_ref().unwrap().service.budget().snapshot().peak;
        drop(app);
        released(&parent);
        eprintln!("SFTP_ENTRY_RESULT scenario=entry requests={requests} peak={peak} final=0 cleanup=ok");
    }
}
