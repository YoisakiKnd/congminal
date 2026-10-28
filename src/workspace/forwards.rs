//! 连接页的转发规则页：查看、新增、删除当前 SSH 配置档的 `-L` 规则。
//!
//! 只改内存里的配置档（`config.connections[i].ssh.forwards`），不写回配置文件：
//! 规则对之后新建的连接生效，已有会话不受影响。每条规则仍按配置解析时的同一套
//! 校验（字符集、段数、长度）把关，非法输入只提示、不落库。
use std::time::{Duration, Instant};

use super::*;
use crate::config::profiles::Connection;
use crate::resources::Tracked;

/// 一条转发规则的长度上限，与配置解析一致。
pub(super) const SPEC_BYTES: usize = 128;
/// 输入框的预编辑（输入法组合）上限。
const PREEDIT_BYTES: usize = 64;
/// 删除要再点一次，窗口与其它二次确认一致。
const DELETE_CONFIRM: Duration = Duration::from_secs(10);
/// 按钮行：新建 / 删除 / 返回。
const LABELS: [&str; 3] = ["新建", "删除", "返回"];
const LABELS_NARROW: [&str; 3] = ["新", "删", "返"];

pub(super) struct Input {
    pub(super) text: Tracked<String>,
    pub(super) caret: usize,
    pub(super) preedit: String,
}

pub(super) struct Page {
    /// 正在编辑的配置档下标（`config.connections` 里的位置）。
    pub(super) profile: usize,
    /// 工作副本：配置档里的规则 + 本次运行临时加/删的结果。
    pub(super) specs: Vec<String>,
    pub(super) selected: Option<usize>,
    pub(super) offset: usize,
    pub(super) input: Option<Input>,
    pub(super) delete_pending: Option<(usize, Instant)>,
    pub(super) note: Option<String>,
}

impl Page {
    fn new(profile: usize, specs: Vec<String>) -> Self {
        Self { profile, specs, selected: None, offset: 0, input: None, delete_pending: None, note: None }
    }
}

impl Workspace {
    /// 打开转发页：只对“当前窗格用的是配置列表里的 SSH 配置档”开放。
    pub(super) fn open_forwards(&mut self) -> bool {
        let Some((profiles, index)) = self.panes[self.loaded].profile.clone() else {
            self.notice("当前会话没有 SSH 配置档");
            return false;
        };
        if !profiles.shares(&self.driver.config.connections) {
            self.notice("这个连接是按别名临时合成的，没有可编辑的配置档");
            return false;
        }
        if !matches!(self.driver.config.connections.get(index).map(|p| &p.connection), Some(Connection::Ssh(_))) {
            self.notice("当前会话不是 SSH 连接");
            return false;
        }
        let specs = match &self.driver.config.connections[index].connection {
            Connection::Ssh(ssh) => ssh.forwards.clone(),
            _ => Vec::new(),
        };
        self.forwards = Some(Page::new(index, specs));
        self.chrome_dirty = true;
        true
    }

    pub(super) fn close_forwards(&mut self) {
        self.sync_forwards_override();
        self.forwards = None;
        self.chrome_dirty = true;
    }

    /// 把工作副本记成“本次运行临时改过的转发规则”，供之后新建连接使用。
    /// 与配置档原值一致时清掉覆盖，避免多留一份副本。
    pub(super) fn sync_forwards_override(&mut self) {
        let Some(page) = &self.forwards else { return };
        let original = match self.driver.config.connections.get(page.profile).map(|p| &p.connection) {
            Some(Connection::Ssh(ssh)) => ssh.forwards.as_slice(),
            _ => &[],
        };
        self.forwards_override = (page.specs.as_slice() != original).then(|| (page.profile, page.specs.clone()));
    }

    /// 当前配置档的规则（只读快照，供绘制与测试）。
    pub(super) fn forward_specs(&self) -> &[String] {
        self.forwards.as_ref().map_or(&[], |page| page.specs.as_slice())
    }

    /// 打开输入框：先准入一段固定额度，关掉页面时随页面一起释放。
    fn open_forward_input(&mut self) {
        let Some(page) = &mut self.forwards else { return };
        if page.input.is_some() {
            return;
        }
        let limit = crate::config::profiles::FORWARD_LIMIT;
        if page.specs.len() >= limit {
            page.note = Some(format!("最多 {limit} 条转发规则，先删掉一条"));
            self.chrome_dirty = true;
            return;
        }
        let Ok(memory) = crate::resources::current().lease(crate::resources::Kind::Sftp, SPEC_BYTES + 256) else {
            page.note = Some("额度不足，先关掉一些面板".into());
            self.chrome_dirty = true;
            return;
        };
        page.input = Some(Input { text: Tracked::new(String::new(), memory), caret: 0, preedit: String::new() });
        page.note = Some("输入 [绑定地址:]端口:主机:端口，回车添加".into());
        self.chrome_dirty = true;
    }

    /// 提交输入：校验通过才追加到配置档，失败只提示。
    pub(super) fn submit_forward(&mut self) -> bool {
        let Some(page) = &mut self.forwards else { return false };
        let Some(input) = page.input.as_ref() else { return false };
        let spec = input.text.data.trim().to_string();
        if spec.is_empty() {
            page.note = Some("规则不能为空".into());
            self.chrome_dirty = true;
            return true;
        }
        if !crate::config::profiles::forward_ok(&spec) {
            page.note = Some("规则须形如 [绑定地址:]端口:主机:端口，只能用字母数字与 .:_-[]".into());
            self.chrome_dirty = true;
            return true;
        }
        let limit = crate::config::profiles::FORWARD_LIMIT;
        if page.specs.len() >= limit {
            page.note = Some(format!("最多 {limit} 条转发规则，先删掉一条"));
        } else if page.specs.contains(&spec) {
            page.note = Some("这条规则已经有了".into());
        } else {
            page.specs.push(spec.clone());
            page.selected = Some(page.specs.len() - 1);
            page.note = Some(format!("已添加 {spec}（本次运行有效，不写回配置文件）"));
            page.input = None;
        }
        self.sync_forwards_override();
        self.chrome_dirty = true;
        true
    }

    /// 删除选中的规则：第一次只起确认，第二次才真正删。
    fn request_delete_forward(&mut self) {
        let Some(page) = &mut self.forwards else { return };
        let Some(index) = page.selected else {
            page.note = Some("先选一条规则".into());
            self.chrome_dirty = true;
            return;
        };
        let now = Instant::now();
        match page.delete_pending {
            Some((pending, at)) if pending == index && now.duration_since(at) < DELETE_CONFIRM => {
                page.delete_pending = None;
                if index < page.specs.len() {
                    let removed = page.specs.remove(index);
                    page.note = Some(format!("已删除 {removed}（本次运行有效）"));
                    page.selected = if page.specs.is_empty() { None } else { Some(index.min(page.specs.len() - 1)) };
                }
            }
            _ => {
                page.delete_pending = Some((index, now));
                page.note = Some("再点一次「删除」确认".into());
            }
        }
        self.sync_forwards_override();
        self.chrome_dirty = true;
    }

    /// 规则列表从第几行开始：按钮行、提示行，输入框开着时再多一行。
    fn rules_start(&self) -> usize {
        3 + usize::from(self.forwards.as_ref().is_some_and(|page| page.input.is_some()))
    }

    /// 转发页的行文本：提示行、可选的输入行，然后是规则列表。测试与绘制共用。
    pub(super) fn forward_rows(&self) -> Vec<String> {
        let Some(page) = &self.forwards else { return Vec::new() };
        let mut rows = Vec::new();
        rows.push(match &page.note {
            Some(note) => note.clone(),
            None => "回车或点「新建」添加规则".into(),
        });
        if let Some(input) = &page.input {
            rows.push(format!("新规则：{}", input.text.data));
        }
        for (index, spec) in self.forward_specs().iter().enumerate().skip(page.offset) {
            let mark = if page.selected == Some(index) { "●" } else { " " };
            let pending = matches!(page.delete_pending, Some((p, _)) if p == index);
            rows.push(format!("{mark} {spec}{}", if pending { " · 待确认删除" } else { "" }));
        }
        if self.forward_specs().is_empty() {
            rows.push("还没有转发规则".into());
        }
        rows
    }

    /// 键盘：输入框开着时只喂输入框，否则处理 Esc 与上下选择。
    pub(super) fn forwards_capture_key(&mut self, key: Key, mods: Mods) -> bool {
        let Some(page) = &mut self.forwards else { return false };
        if page.input.is_some() {
            if page.input.as_ref().is_some_and(|input| !input.preedit.is_empty()) {
                if key == Key::Escape
                    && let Some(input) = &mut page.input
                {
                    input.preedit.clear();
                }
                self.chrome_dirty = true;
                return true;
            }
            match key {
                Key::Escape => {
                    page.input = None;
                    page.note = None;
                }
                Key::Enter => return self.submit_forward(),
                Key::Backspace => {
                    if let Some(input) = &mut page.input
                        && input.caret > 0
                    {
                        let previous = input.text.data[..input.caret].char_indices().next_back().map_or(0, |(i, _)| i);
                        input.text.data.replace_range(previous..input.caret, "");
                        input.caret = previous;
                    }
                }
                Key::Left => {
                    if let Some(input) = &mut page.input {
                        input.caret = input.text.data[..input.caret].char_indices().next_back().map_or(0, |(i, _)| i);
                    }
                }
                Key::Right => {
                    if let Some(input) = &mut page.input {
                        input.caret = input.text.data[input.caret..]
                            .chars()
                            .next()
                            .map_or(input.caret, |c| input.caret + c.len_utf8());
                    }
                }
                Key::Char(_) | Key::Keypad(_) if !mods.ctrl && !mods.alt && !mods.logo => {
                    if let Some(c) = key.text().filter(|c| !c.is_control()) {
                        let mut buffer = [0u8; 4];
                        self.insert_forward(c.encode_utf8(&mut buffer));
                    }
                }
                _ => {}
            }
            self.chrome_dirty = true;
            return true;
        }
        match key {
            Key::Escape => {
                self.close_forwards();
                true
            }
            Key::Up => {
                page.selected = Some(page.selected.map_or(0, |index| index.saturating_sub(1)));
                self.chrome_dirty = true;
                true
            }
            Key::Down => {
                let last = page.specs.len().saturating_sub(1);
                page.selected = Some(page.selected.map_or(0, |index| (index + 1).min(last)));
                self.chrome_dirty = true;
                true
            }
            _ => false,
        }
    }

    pub(super) fn forwards_capture_text(&mut self, text: &str) -> bool {
        if self.forwards.as_ref().is_none_or(|page| page.input.is_none()) {
            return false;
        }
        self.insert_forward(text);
        self.chrome_dirty = true;
        true
    }

    pub(super) fn forwards_capture_preedit(&mut self, text: &str) -> bool {
        let Some(input) = self.forwards.as_mut().and_then(|page| page.input.as_mut()) else { return false };
        input.preedit.clear();
        for c in text.chars().take(PREEDIT_BYTES) {
            input.preedit.push(c);
        }
        self.chrome_dirty = true;
        true
    }

    /// 只在额度内插入；超出上限的部分丢弃，不扩容。
    fn insert_forward(&mut self, text: &str) {
        let Some(input) = self.forwards.as_mut().and_then(|page| page.input.as_mut()) else { return };
        let room = SPEC_BYTES.saturating_sub(input.text.data.len());
        if room == 0 {
            return;
        }
        let mut take = text;
        if take.len() > room {
            let mut end = room;
            while !take.is_char_boundary(end) {
                end -= 1;
            }
            take = &take[..end];
        }
        if take.is_empty() || input.text.data.try_reserve(take.len()).is_err() {
            return;
        }
        input.text.data.insert_str(input.caret, take);
        input.caret += take.len();
        input.preedit.clear();
    }

    /// 点击：第 1 行是按钮（新建/删除/返回），之后是规则行。
    pub(super) fn forwards_click(&mut self, row: usize, third: usize) -> bool {
        if self.forwards.is_none() {
            return false;
        }
        if row == 1 {
            match third {
                0 => self.open_forward_input(),
                1 => self.request_delete_forward(),
                _ => self.close_forwards(),
            }
            return true;
        }
        let Some(mut index) = row.checked_sub(self.rules_start()) else { return true };
        let Some(page) = &mut self.forwards else { return true };
        index += page.offset;
        if index < page.specs.len() {
            page.selected = Some(index);
            page.delete_pending = None;
            self.chrome_dirty = true;
        }
        true
    }

    /// 绘制：按钮行 + 提示/输入 + 规则列表。
    pub(super) fn forwards_paint(&mut self, r: Rect, h: usize) -> bool {
        if self.forwards.is_none() {
            return false;
        }
        let narrow = r.w / 3 < 4 * self.driver.renderer.cell_size().0;
        let labels = if narrow { LABELS_NARROW } else { LABELS };
        let slot = (r.w / labels.len()).max(1);
        for (index, label) in labels.iter().enumerate() {
            let selected = matches!((index, &self.forwards), (1, Some(page)) if page.delete_pending.is_some());
            self.driver.renderer.chrome(&mut self.frame, [r.x + index * slot, r.y + h, slot, h], label, selected);
        }
        let rows = self.forward_rows();
        let rules = self.rules_start().saturating_sub(2);
        let visible = (r.h / h).saturating_sub(3);
        for (index, text) in rows.iter().take(visible).enumerate() {
            let selected = index >= rules
                && self.forwards.as_ref().is_some_and(|page| page.selected == Some(index - rules + page.offset));
            self.driver.renderer.chrome(&mut self.frame, [r.x, r.y + (index + 2) * h, r.w, h], text, selected);
        }
        true
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::config::Config;

    fn ssh_workspace(program: &std::path::Path) -> Workspace {
        let (mut configured, mut warnings) = Config::parse(&format!(
            "[connections.tunnel]\nkind='ssh'\nhost='fixture'\nprogram={program:?}\n\
             forward=['8080:localhost:80']\n"
        ));
        configured.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        let mut app = super::super::tests::workspace();
        app.driver.config.connections = configured.connections;
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        app.add_with_profile(None, Some(0));
        // SSH 窗格是异步准备的：等到新窗格挂上配置档再编辑转发。
        crate::workspace::tests::wait(&mut app, |a| a.panes[a.loaded].profile.is_some());
        app
    }

    fn fake_ssh(dir: &std::path::Path) -> std::path::PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let path = dir.join("fake-ssh-forward");
        std::fs::write(&path, "#!/bin/sh\ncase \"$1\" in -G) exit 0;; esac\nprintf 'REMOTE_READY'\nexec /bin/cat\n")
            .unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        path
    }

    #[test]
    fn the_page_lists_adds_and_removes_rules_without_touching_the_config_file() {
        let dir = crate::workspace::layout::TestDir::new();
        let program = fake_ssh(&dir.0);
        let mut app = ssh_workspace(&program);
        assert!(app.open_forwards(), "当前窗格是 SSH 配置档，可以编辑转发");
        // 打开时带上配置里的规则。
        assert_eq!(app.forward_specs(), ["8080:localhost:80"]);
        assert!(app.forward_rows()[1].contains("8080:localhost:80"), "{:?}", app.forward_rows());
        // 导出一帧供人工核对排版（按钮行 + 提示 + 规则）。
        if let Some(path) = std::env::var_os("CONGMINAL_FORWARDS_PREVIEW_PATH") {
            app.forwards_click(3, 0);
            crate::workspace::tests::force_draw(&mut app);
            let mut ppm = format!("P6\n{} {}\n255\n", app.frame.width, app.frame.height).into_bytes();
            for &pixel in &app.frame.pixels {
                ppm.extend_from_slice(&[(pixel >> 16) as u8, (pixel >> 8) as u8, pixel as u8]);
            }
            std::fs::write(path, ppm).unwrap();
        }
        // 新建：非法规则只提示，不落库。
        app.open_forward_input();
        app.forwards_capture_text("not-a-rule");
        app.submit_forward();
        assert_eq!(app.forward_specs(), ["8080:localhost:80"], "非法规则不能进列表");
        assert!(app.forward_rows()[0].contains("须形如"), "{:?}", app.forward_rows());
        // 合法规则追加到工作副本。
        app.forwards_capture_key(Key::Escape, Mods::default());
        app.open_forward_input();
        app.forwards_capture_text("9090:localhost:90");
        app.submit_forward();
        assert_eq!(app.forward_specs(), ["8080:localhost:80", "9090:localhost:90"]);
        // 重复规则被拒。
        app.open_forward_input();
        app.forwards_capture_text("9090:localhost:90");
        app.submit_forward();
        assert_eq!(app.forward_specs().len(), 2);
        assert!(app.forward_rows()[0].contains("已经有了"), "{:?}", app.forward_rows());
        // 删除要再点一次：先关掉输入框，规则行回到第 3 行。
        app.forwards_capture_key(Key::Escape, Mods::default());
        app.forwards_click(3, 0);
        assert_eq!(app.forwards.as_ref().unwrap().selected, Some(0), "{:?}", app.forward_rows());
        app.forwards_click(1, 1);
        assert_eq!(app.forward_specs().len(), 2, "第一次只起确认");
        assert!(app.forward_rows()[1].contains("待确认删除"), "{:?}", app.forward_rows());
        app.forwards_click(1, 1);
        assert_eq!(app.forward_specs(), ["9090:localhost:90"]);
        // 配置档本身没被改：只记在本次运行的覆盖里。
        let Connection::Ssh(ssh) = &app.driver.config.connections[0].connection else { panic!("应为 SSH") };
        assert_eq!(ssh.forwards, ["8080:localhost:80"], "配置文件里的规则不动");
        app.close_forwards();
        assert_eq!(
            app.forwards_override.as_ref().map(|(index, specs)| (*index, specs.clone())),
            Some((0, vec!["9090:localhost:90".to_string()]))
        );
    }

    #[test]
    fn edited_rules_reach_the_next_connection_but_not_the_config_file() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::workspace::layout::TestDir::new();
        // 假 ssh 把参数记到文件里，用来核对 -L 真的传下去了。
        let log = dir.0.join("argv.log");
        let program = dir.0.join("fake-ssh-argv");
        std::fs::write(
            &program,
            format!(
                "#!/bin/sh\ncase \"$1\" in -G) exit 0;; esac\nprintf '%s\\n' \"$@\" >> {log:?}\n\
                 printf 'REMOTE_READY'\nexec /bin/cat\n"
            ),
        )
        .unwrap();
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
        let mut app = ssh_workspace(&program);
        assert!(app.open_forwards());
        app.open_forward_input();
        app.forwards_capture_text("9090:localhost:90");
        app.submit_forward();
        // 再把原来的 8080 删掉：新连接只带剩下那条（提交成功后输入框已关，规则行在第 3 行）。
        app.forwards_click(3, 0);
        app.forwards_click(1, 1);
        app.forwards_click(1, 1);
        assert_eq!(app.forward_specs(), ["9090:localhost:90"]);
        app.close_forwards();
        assert!(app.forwards_override.is_some(), "改过的规则要记在本次运行的覆盖里");
        // 再从连接页点这个配置档：新连接带上改过的规则。
        // 第一次连接的 ssh 进程可能比窗格晚一点才写参数：等它写完再清空日志。
        let read_log = |path: &std::path::Path| std::fs::read_to_string(path).unwrap_or_default();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while !read_log(&log).contains("8080:localhost:80") {
            assert!(std::time::Instant::now() < deadline, "第一次连接没写参数：{:?}", read_log(&log));
            app.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        std::fs::remove_file(&log).unwrap();
        let before = app.panes.len();
        app.add_with_profile(None, Some(0));
        // 新窗格是异步准备的：等它真的出现并连上，别把旧窗格当成新连接。
        crate::workspace::tests::wait(&mut app, |a| a.panes.len() > before);
        crate::workspace::tests::wait(&mut app, |a| a.panes[before].remote.is_some());
        // 参数由 ssh 进程自己写，等它落盘再断言。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while read_log(&log).is_empty() {
            assert!(std::time::Instant::now() < deadline, "新连接没写参数");
            app.tick();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let argv = read_log(&log);
        assert!(argv.contains("9090:localhost:90"), "改过的规则要传给 ssh：{argv}");
        assert!(!argv.contains("8080:localhost:80"), "被删掉的规则不能再传：{argv}");
        // 配置文件里的原值不动。
        let Connection::Ssh(ssh) = &app.driver.config.connections[0].connection else { panic!("应为 SSH") };
        assert_eq!(ssh.forwards, ["8080:localhost:80"]);
    }

    #[test]
    fn the_page_is_closed_for_local_panes_and_escape_leaves_it() {
        let mut app = super::super::tests::workspace();
        app.action(Action::ToggleLeftSidebar);
        app.left_page = LeftPage::Connections;
        // 本地窗格没有 SSH 配置档。
        assert!(!app.open_forwards());
        assert!(app.forwards.is_none());
        // 打开后 Esc 关掉页面，键盘交回终端。
        let dir = crate::workspace::layout::TestDir::new();
        let program = fake_ssh(&dir.0);
        let mut app = ssh_workspace(&program);
        assert!(app.open_forwards());
        assert!(app.forwards_capture_key(Key::Escape, Mods::default()));
        assert!(app.forwards.is_none());
    }

    #[test]
    fn the_input_is_bounded_and_keypad_digits_are_text() {
        let dir = crate::workspace::layout::TestDir::new();
        let program = fake_ssh(&dir.0);
        let mut app = ssh_workspace(&program);
        assert!(app.open_forwards());
        app.open_forward_input();
        // 小键盘数字在数字锁定下也是文字。
        app.forwards_capture_key(Key::Keypad(crate::input::Keypad::Digit(8)), Mods::default());
        assert_eq!(app.forwards.as_ref().unwrap().input.as_ref().unwrap().text.data, "8");
        // 超长输入被截断在 128 字节，不扩容。
        app.forwards_capture_text(&"9".repeat(200));
        let text = &app.forwards.as_ref().unwrap().input.as_ref().unwrap().text.data;
        assert_eq!(text.len(), SPEC_BYTES);
        // 退格删一个字符。
        app.forwards_capture_key(Key::Backspace, Mods::default());
        assert_eq!(app.forwards.as_ref().unwrap().input.as_ref().unwrap().text.data.len(), SPEC_BYTES - 1);
    }
}
