pub mod common;
pub mod http_utils;
pub mod server;

pub use common::errors::*;
pub use common::types::*;

pub use http_utils::*;

pub use server::utils::logging::*;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

pub const DEFAULT_CONNECTION_TIMEOUT: u64 = 5;

pub const DEFAULT_IDLE_TIMEOUT: u64 = 0;
