use http::HeaderMap;
pub fn is_websocket_upgrade_request(headers: &HeaderMap) -> bool {
    let has_upgrade = if let Some(val) = headers.get("upgrade") {
        val.to_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("websocket")
    } else {
        false
    };

    let has_connection_upgrade = if let Some(val) = headers.get("connection") {
        val.to_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("upgrade")
    } else {
        false
    };

    let has_sec_websocket_key = headers.contains_key("sec-websocket-key");

    has_upgrade && has_connection_upgrade && has_sec_websocket_key
}

pub fn is_websocket_upgrade_response(status: u16, headers: &HeaderMap) -> bool {
    if status != 101 {
        return false;
    }

    let has_upgrade = if let Some(val) = headers.get("upgrade") {
        val.to_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("websocket")
    } else {
        false
    };

    let has_connection_upgrade = if let Some(val) = headers.get("connection") {
        val.to_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("upgrade")
    } else {
        false
    };

    let has_sec_websocket_accept = headers.contains_key("sec-websocket-accept");

    has_upgrade && has_connection_upgrade && has_sec_websocket_accept
}
