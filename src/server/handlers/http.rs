use crate::prelude::{ConnectionType, HttpRequestParser};

pub fn parse_http_request(data: &[u8]) -> Option<(bool, bool, String)> {
    let mut parser = HttpRequestParser::new();
    parser.extend(data);

    if !parser.is_complete() {
        return None;
    }

    match parser.parse() {
        Ok(request) => {
            let path = request.path;

            match request.connection_type {
                ConnectionType::KeepAlive => Some((true, false, path)),
                ConnectionType::Close => Some((false, true, path)),
                ConnectionType::WebSocketUpgrade => Some((true, false, path)),
            }
        }
        Err(_) => None,
    }
}
