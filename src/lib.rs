pub mod client;
pub mod common;
pub mod server;
pub use common::errors::*;
pub use common::http_conn::*;
pub use common::logging::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

pub const DEFAULT_CONNECTION_TIMEOUT: u64 = 5;

pub const DEFAULT_IDLE_TIMEOUT: u64 = 0;
