//! 连接创建时捕获的只读身份；不访问 SSH 文件，也不把别名冒充解析后的主机。
use super::{Error, escape};
use crate::{
    config::profiles::{Connection, Profiles},
    resources::{Budget, Kind, Tracked},
};
use std::sync::Arc;
pub const DISPLAY_LIMIT: usize = 4096;
struct Data {
    profiles: Profiles,
    index: usize,
    display: String,
}

#[derive(Clone)]
pub struct Identity(Arc<Tracked<Data>>);
impl Identity {
    pub fn new(profiles: Profiles, index: usize, pool: &Arc<Budget>) -> Result<Self, Error> {
        let profile = profiles.get(index).ok_or(Error::Invalid)?;
        let Connection::Ssh(ssh) = &profile.connection else { return Err(Error::Invalid) };
        if !crate::ssh::valid(ssh)
            || profile.id.len() > 64
            || profile.name.len() > 128
            || profile.id.is_empty()
            || profile.name.is_empty()
        {
            return Err(Error::Invalid);
        }
        let capacity =
            (profile.id.len() + profile.name.len() + ssh.host.len() + ssh.user.as_ref().map_or(0, String::len)) * 4
                + 256;
        if capacity > DISPLAY_LIMIT {
            return Err(Error::Limit);
        }
        let mut memory =
            pool.lease(Kind::Sftp, capacity + std::mem::size_of::<Data>() + 128).map_err(|_| Error::Budget)?;
        let mut display = String::new();
        display.try_reserve_exact(capacity).map_err(|_| Error::Budget)?;
        display.push_str("Connection profile: ");
        escape(&mut display, profile.name.as_bytes(), false);
        display.push_str(" [");
        escape(&mut display, profile.id.as_bytes(), false);
        display.push_str("]\nRequested SSH target: ");
        if let Some(user) = &ssh.user {
            escape(&mut display, user.as_bytes(), false);
        } else {
            display.push_str("(user from SSH configuration)");
        }
        display.push('@');
        if ssh.host.contains(':') {
            display.push('[');
        }
        escape(&mut display, ssh.host.as_bytes(), false);
        if ssh.host.contains(':') {
            display.push(']');
        }
        display.push(':');
        if let Some(port) = ssh.port {
            use std::fmt::Write;
            write!(display, "{port}").unwrap();
        } else {
            display.push_str("(port from SSH configuration)");
        }
        display.push_str("\nSSH configuration may resolve this alias. Verify the complete prompt below.\n\n");
        // 字段预检查的四倍展开加256静态字节足够，不在构建后才补一份无界副本。
        assert!(display.len() <= capacity && display.capacity() <= DISPLAY_LIMIT);
        memory.resize(display.capacity() + std::mem::size_of::<Data>() + 128).map_err(|_| Error::Budget)?;
        Ok(Self(Arc::new(Tracked::new(Data { profiles, index, display }, memory))))
    }
    pub fn matches(&self, profiles: &Profiles, index: usize) -> bool {
        self.0.index == index && self.0.profiles.shares(profiles)
    }
    pub fn display(&self) -> &str {
        &self.0.display
    }
    pub(super) fn uses(&self, pool: &Arc<Budget>) -> bool {
        self.0.memory.belongs_to(pool)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::Config,
        ssh::auth::{
            Backend, Kind as PromptKind, Owner, PROMPT_LIMIT, controller::Controller, editor::Error as EditorError,
        },
        tasks::Cancellation,
    };
    fn profiles() -> Profiles {
        let (mut config, mut warnings) = Config::parse(
            "[connections.fixture]\nkind='ssh'\nname='身份\u{202e}核对'\nhost='alias.fixture'\nuser='fixture-user'\nport=2222\n",
        );
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty(), "{warnings:?}");
        config.connections
    }
    #[test]
    fn identity_is_escaped_bound_to_immutable_profile_snapshot_and_charged_to_the_same_pool() {
        let pool = Budget::new(2 * 1024 * 1024, 0);
        let profiles = profiles();
        let identity = Identity::new(profiles.clone(), 0, &pool).unwrap();
        assert!(identity.matches(&profiles, 0));
        assert!(!identity.matches(&profiles, 1));
        assert!(!identity.matches(&self::profiles(), 0));
        assert!(identity.display().contains("fixture-user@alias.fixture:2222"));
        assert!(!identity.display().contains('\u{202e}'));
        assert!(identity.display().contains("\\u{202e}"));
        let owner = Owner { session: 42, generation: 3 };
        let cancel = Cancellation::default();
        let other = Budget::new(2 * 1024 * 1024, 0);
        assert!(matches!(
            Backend::with_identity(owner, &other, Arc::new(|| {}), identity.clone()),
            Err(Error::WrongPool)
        ));
        assert_eq!(other.snapshot().used, 0);
        let (mut backend, front) = Backend::with_identity(owner, &pool, Arc::new(|| {}), identity.clone()).unwrap();
        assert_eq!(front.owner(), owner);
        assert!(front.identity().unwrap().matches(&profiles, 0));
        backend.begin(PromptKind::Confirm, &[27; PROMPT_LIMIT], &cancel).unwrap();
        let snapshot = front.snapshot();
        let prompt = snapshot.input.unwrap();
        assert!(prompt.display().starts_with(identity.display()));
        assert_eq!(prompt.display().len(), identity.display().len() + 4 * PROMPT_LIMIT);
        let mut controller = Controller::new(front, owner, true).unwrap();
        assert!(controller.identity().unwrap().matches(&profiles, 0));
        assert!(controller.pending());
        let token = controller.token().unwrap();
        assert_eq!(controller.decide(token, true), Err(EditorError::NotReviewed));
        drop((controller, backend, prompt, identity));
        assert_eq!(pool.snapshot().used, 0);
    }
    #[test]
    fn omitted_user_and_port_stay_explicitly_unresolved_and_low_budget_does_not_publish_partial_identity() {
        let (mut config, mut warnings) = Config::parse("[connections.fixture]\nkind='ssh'\nhost='alias.fixture'\n");
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        let pool = Budget::new(1, 0);
        assert!(matches!(Identity::new(config.connections.clone(), 0, &pool), Err(Error::Budget)));
        assert_eq!(pool.snapshot().used, 0);
        let pool = Budget::new(8192, 0);
        let identity = Identity::new(config.connections, 0, &pool).unwrap();
        assert!(identity.display().contains("user from SSH configuration"));
        assert!(identity.display().contains("port from SSH configuration"));
        assert!(!identity.display().contains(":22\n"));
        drop(identity);
        assert_eq!(pool.snapshot().used, 0);
        let (mut config, mut warnings) = Config::parse("[connections.fixture]\nkind='ssh'\nhost='::1'\nport=2222\n");
        config.admit_connections(&mut warnings);
        assert!(warnings.is_empty());
        let identity = Identity::new(config.connections, 0, &pool).unwrap();
        assert!(identity.display().contains("@[::1]:2222"));
        drop(identity);
        assert_eq!(pool.snapshot().used, 0);
    }
}
