//! 两端直接编译相同的协议及秘密容器源码，不维护第二份编解码逻辑。
#[path = "auth_types.rs"]
mod types;
pub use types::{Error, Kind, PROMPT_LIMIT};
#[path = "auth_secret.rs"]
mod secret;
pub use secret::{ANSWER_LIMIT, Secret};
#[path = "auth_ticket.rs"]
pub mod ticket;
#[path = "auth_wire.rs"]
pub mod wire;
