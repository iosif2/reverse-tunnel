use crate::common::errors::is_connection_error;
use crate::http_utils::HttpRequestParser;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

pub fn is_websocket_upgrade(data: &[u8]) -> bool {
    let mut parser = HttpRequestParser::new();
    parser.extend(data);
    parser.is_websocket_upgrade()
}
