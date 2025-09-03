use crate::common::http_conn::parser::request::HttpRequestParser;

pub fn is_websocket_upgrade(data: &[u8]) -> bool {
    let mut parser = HttpRequestParser::new();
    parser.extend(data);
    parser.is_websocket_upgrade()
}
