use http::HeaderMap;
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionType {
    /// Keep-Alive 연결: 여러 요청/응답에 재사용
    KeepAlive,
    /// Close 연결: 요청/응답 이후 연결 닫기
    Close,
    /// 웹소켓 업그레이드: 프로토콜 업그레이드
    WebSocketUpgrade,
}
pub fn determine_connection_type(headers: &HeaderMap, version: &str) -> ConnectionType {
    if let Some(upgrade) = headers.get("upgrade") {
        if upgrade
            .to_str()
            .unwrap_or("")
            .to_lowercase()
            .contains("websocket")
        {
            if let Some(conn) = headers.get("connection") {
                if conn
                    .to_str()
                    .unwrap_or("")
                    .to_lowercase()
                    .contains("upgrade")
                {
                    if headers.contains_key("sec-websocket-key")
                        || headers.contains_key("sec-websocket-accept")
                    {
                        return ConnectionType::WebSocketUpgrade;
                    }
                }
            }
        }
    }

    // Connection 헤더 확인
    if let Some(connection) = headers.get("connection") {
        let connection_value = connection.to_str().unwrap_or("").to_lowercase();
        if connection_value.contains("close") {
            return ConnectionType::Close;
        } else if connection_value.contains("keep-alive") {
            return ConnectionType::KeepAlive;
        }
    }

    // HTTP 버전에 따른 기본값
    if version == "HTTP/1.0" {
        ConnectionType::Close
    } else {
        // HTTP/1.1부터는 기본이 Keep-Alive
        ConnectionType::KeepAlive
    }
}
