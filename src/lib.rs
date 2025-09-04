pub mod client;
pub mod common;
pub mod server;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

pub const DEFAULT_CONNECTION_TIMEOUT: u64 = 5;

pub const DEFAULT_IDLE_TIMEOUT: u64 = 0;

pub use client::{
    Args, ConnectionPool, PooledConnection, ProxyError, ProxyResult, connect_and_serve,
    handle_backend_connection, handle_legacy_connection, handle_one_time_http_request,
    handle_websocket_connection,
};
pub use common::{
    ConnectionType, HttpRequest, HttpRequestParser, HttpResponse, HttpResponseParser,
    ProxyConnectionState, determine_connection_type, is_websocket_upgrade_request,
    is_websocket_upgrade_response, serialize_http_request_to_bytes, serialize_request_to_bytes,
};
pub use server::{
    ProxyClientInfo, handle_client, handle_public_connection, update_client_activity,
    validate_callback_stream,
};
pub mod prelude {
    pub use crate::common::http_conn::{
        ConnectionType, HttpRequest, HttpRequestParser, HttpResponse, HttpResponseParser,
        serialize_http_request_to_bytes, serialize_request_to_bytes,
    };

    pub use crate::client::{ProxyError, ProxyResult};
}
