pub mod connection;
pub mod parser;
pub mod proxy_state;
pub mod types;
pub mod upgrade;

pub use connection::{ConnectionType, determine_connection_type};
pub use parser::{HttpRequestParser, HttpResponseParser};
pub use proxy_state::ProxyConnectionState;
pub use types::{
    HttpRequest, HttpResponse, serialize_http_request_to_bytes, serialize_request_to_bytes,
};
pub use upgrade::{is_websocket_upgrade_request, is_websocket_upgrade_response};
