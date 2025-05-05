use bytes::{Buf, BytesMut};
use http::{HeaderMap, Method, Request, Response, StatusCode, Version};
use httparse;
use std::io;
use tracing::{debug, error, info, warn};

/// HTTP 요청을 파싱하는 구조체
pub struct HttpRequestParser {
    buf: BytesMut,
    headers: [httparse::Header<'static>; 64],
}

impl HttpRequestParser {
    /// 새로운 HTTP 요청 파서를 생성
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(8192),
            headers: [httparse::EMPTY_HEADER; 64],
        }
    }

    /// 버퍼에 데이터 추가
    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// HTTP 요청이 완전한지 확인
    pub fn is_complete(&self) -> bool {
        // 헤더의 끝을 찾음 (빈 줄로 표시됨)
        if let Some(headers_end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
            // 청크 인코딩 또는 Content-Length를 확인
            let headers_str = std::str::from_utf8(&self.buf[..headers_end])
                .unwrap_or_default()
                .to_lowercase();

            // Content-Length가 있는 경우
            if headers_str.contains("content-length:") {
                // 본문 길이 추출
                for line in headers_str.lines() {
                    if line.starts_with("content-length:") {
                        if let Ok(len) = line
                            .split(':')
                            .nth(1)
                            .unwrap_or("0")
                            .trim()
                            .parse::<usize>()
                        {
                            return self.buf.len() >= headers_end + 4 + len;
                        }
                    }
                }
            }

            // Transfer-Encoding: chunked인 경우 완전한 본문 확인은 복잡하므로
            // 여기서는 마지막 청크(0 크기)를 포함하는지만 검사
            if headers_str.contains("transfer-encoding: chunked") {
                return self.buf.windows(5).any(|w| w == b"0\r\n\r\n");
            }

            // Content-Length도 Transfer-Encoding도 없는 경우, 헤더만 있으면 완전함
            return true;
        }
        false
    }

    /// HTTP 요청 파싱
    pub fn parse(&self) -> Result<HttpRequest, io::Error> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut req = httparse::Request::new(&mut headers);

        match req.parse(&self.buf) {
            Ok(httparse::Status::Complete(headers_len)) => {
                // HTTP 메서드, 경로, 버전 추출
                let method = req.method.unwrap_or("GET").to_string();
                let path = req.path.unwrap_or("/").to_string();
                let version = match req.version.unwrap_or(1) {
                    0 => "HTTP/1.0",
                    _ => "HTTP/1.1",
                }
                .to_string();

                // 헤더 맵 구성
                let mut header_map = HeaderMap::new();
                for header in req.headers {
                    if let Ok(value) = std::str::from_utf8(header.value) {
                        if let Ok(name) =
                            http::header::HeaderName::from_bytes(header.name.as_bytes())
                        {
                            if let Ok(val) = http::header::HeaderValue::from_str(value) {
                                header_map.insert(name, val);
                            }
                        }
                    }
                }

                // 본문 추출 (있는 경우)
                let body = if self.buf.len() > headers_len {
                    Some(self.buf[headers_len..].to_vec())
                } else {
                    None
                };

                // 연결 타입 결정
                let connection_type = determine_connection_type(&header_map, &version);

                Ok(HttpRequest {
                    method,
                    path,
                    version,
                    headers: header_map,
                    body,
                    connection_type,
                })
            }
            Ok(httparse::Status::Partial) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "불완전한 HTTP 요청",
            )),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        }
    }

    /// 웹소켓 업그레이드 요청인지 확인
    pub fn is_websocket_upgrade(&self) -> bool {
        if let Ok(req) = self.parse() {
            return req.is_websocket_upgrade();
        }
        false
    }

    /// 버퍼 내용 가져오기
    pub fn get_buffer(&self) -> &[u8] {
        &self.buf
    }

    /// 버퍼 비우기
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// HTTP 응답을 파싱하는 구조체
pub struct HttpResponseParser {
    buf: BytesMut,
    headers: [httparse::Header<'static>; 64],
}

impl HttpResponseParser {
    /// 새로운 HTTP 응답 파서 생성
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(8192),
            headers: [httparse::EMPTY_HEADER; 64],
        }
    }

    /// 버퍼에 데이터 추가
    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// HTTP 응답이 완전한지 확인
    pub fn is_complete(&self) -> bool {
        // 헤더의 끝을 찾음
        if let Some(headers_end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
            // 청크 인코딩 또는 Content-Length를 확인
            let headers_str = std::str::from_utf8(&self.buf[..headers_end])
                .unwrap_or_default()
                .to_lowercase();

            // Content-Length가 있는 경우
            if headers_str.contains("content-length:") {
                for line in headers_str.lines() {
                    if line.starts_with("content-length:") {
                        if let Ok(len) = line
                            .split(':')
                            .nth(1)
                            .unwrap_or("0")
                            .trim()
                            .parse::<usize>()
                        {
                            return self.buf.len() >= headers_end + 4 + len;
                        }
                    }
                }
            }

            // Transfer-Encoding: chunked인 경우
            if headers_str.contains("transfer-encoding: chunked") {
                return self.buf.windows(5).any(|w| w == b"0\r\n\r\n");
            }

            // 1xx, 204, 304 상태 코드는 본문이 없음
            let mut resp = [httparse::EMPTY_HEADER; 64];
            let mut res = httparse::Response::new(&mut resp);
            if let Ok(httparse::Status::Complete(_)) = res.parse(&self.buf) {
                if let Some(status) = res.code {
                    if status < 200 || status == 204 || status == 304 {
                        return true;
                    }
                }
            }

            // HEAD 요청에 대한 응답은 본문이 없음
            // (클라이언트가 HEAD 요청을 보냈는지는 여기서 알 수 없으므로 처리하지 않음)

            // 그 외의 경우, 헤더와 일부 본문이 있어야 함
            return headers_end + 4 < self.buf.len();
        }
        false
    }

    /// HTTP 응답 파싱
    pub fn parse(&self) -> Result<HttpResponse, io::Error> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut res = httparse::Response::new(&mut headers);

        match res.parse(&self.buf) {
            Ok(httparse::Status::Complete(headers_len)) => {
                // 상태 코드, 이유 구문, 버전 추출
                let status = res.code.unwrap_or(200);
                let reason = res.reason.unwrap_or("OK").to_string();
                let version = match res.version.unwrap_or(1) {
                    0 => "HTTP/1.0",
                    _ => "HTTP/1.1",
                }
                .to_string();

                // 헤더 맵 구성
                let mut header_map = HeaderMap::new();
                for header in res.headers {
                    if let Ok(value) = std::str::from_utf8(header.value) {
                        if let Ok(name) =
                            http::header::HeaderName::from_bytes(header.name.as_bytes())
                        {
                            if let Ok(val) = http::header::HeaderValue::from_str(value) {
                                header_map.insert(name, val);
                            }
                        }
                    }
                }

                // 본문 추출 (있는 경우)
                let body = if self.buf.len() > headers_len {
                    Some(self.buf[headers_len..].to_vec())
                } else {
                    None
                };

                // 연결 타입 결정
                let connection_type = determine_connection_type(&header_map, &version);

                Ok(HttpResponse {
                    status,
                    reason,
                    version,
                    headers: header_map,
                    body,
                    connection_type,
                })
            }
            Ok(httparse::Status::Partial) => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "불완전한 HTTP 응답",
            )),
            Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e.to_string())),
        }
    }

    /// 웹소켓 업그레이드 응답인지 확인
    pub fn is_websocket_upgrade(&self) -> bool {
        if let Ok(res) = self.parse() {
            return res.is_websocket_upgrade();
        }
        false
    }

    /// 버퍼 내용 가져오기
    pub fn get_buffer(&self) -> &[u8] {
        &self.buf
    }

    /// 버퍼 비우기
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

/// HTTP 연결 타입
#[derive(Debug, Clone, PartialEq)]
pub enum ConnectionType {
    /// Keep-Alive 연결: 여러 요청/응답에 재사용
    KeepAlive,
    /// Close 연결: 요청/응답 이후 연결 닫기
    Close,
    /// 웹소켓 업그레이드: 프로토콜 업그레이드
    WebSocketUpgrade,
}

/// HTTP 요청 구조체
#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub version: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    pub connection_type: ConnectionType,
}

impl HttpRequest {
    /// 웹소켓 업그레이드 요청인지 확인
    pub fn is_websocket_upgrade(&self) -> bool {
        let has_upgrade = if let Some(val) = self.headers.get("upgrade") {
            val.to_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("websocket")
        } else {
            false
        };

        let has_connection_upgrade = if let Some(val) = self.headers.get("connection") {
            val.to_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("upgrade")
        } else {
            false
        };

        let has_sec_websocket_key = self.headers.contains_key("sec-websocket-key");

        has_upgrade && has_connection_upgrade && has_sec_websocket_key
    }
}

/// HTTP 응답 구조체
#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub version: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    pub connection_type: ConnectionType,
}

impl HttpResponse {
    /// 웹소켓 업그레이드 응답인지 확인
    pub fn is_websocket_upgrade(&self) -> bool {
        // 상태 코드가 101이어야 함
        if self.status != 101 {
            return false;
        }

        let has_upgrade = if let Some(val) = self.headers.get("upgrade") {
            val.to_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("websocket")
        } else {
            false
        };

        let has_connection_upgrade = if let Some(val) = self.headers.get("connection") {
            val.to_str()
                .unwrap_or("")
                .to_lowercase()
                .contains("upgrade")
        } else {
            false
        };

        let has_sec_websocket_accept = self.headers.contains_key("sec-websocket-accept");

        has_upgrade && has_connection_upgrade && has_sec_websocket_accept
    }
}

/// 헤더와 HTTP 버전에 따라 연결 타입 결정
fn determine_connection_type(headers: &HeaderMap, version: &str) -> ConnectionType {
    // 웹소켓 업그레이드 확인
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

/// HTTP 프록시 연결 상태
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyConnectionState {
    /// 초기 상태
    Initial,
    /// 첫 요청/응답 처리 중
    FirstExchange,
    /// 활성 상태 (요청/응답 교환 중)
    Active,
    /// 웹소켓 모드
    WebSocket,
    /// 닫는 중
    Closing,
    /// 닫힘
    Closed,
    /// 오류 발생
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_request_parser() {
        let request =
            b"GET /index.html HTTP/1.1\r\nHost: example.com\r\nConnection: keep-alive\r\n\r\n";
        let mut parser = HttpRequestParser::new();
        parser.extend(request);

        assert!(parser.is_complete());
        let parsed = parser.parse().unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/index.html");
        assert_eq!(parsed.version, "HTTP/1.1");
        assert_eq!(parsed.connection_type, ConnectionType::KeepAlive);
    }

    #[test]
    fn test_http_response_parser() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nContent-Type: text/plain\r\n\r\nHello, World!";
        let mut parser = HttpResponseParser::new();
        parser.extend(response);

        assert!(parser.is_complete());
        let parsed = parser.parse().unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.reason, "OK");
        assert_eq!(parsed.version, "HTTP/1.1");
        assert!(parsed.body.is_some());
        assert_eq!(parsed.connection_type, ConnectionType::KeepAlive);
    }

    #[test]
    fn test_websocket_upgrade() {
        let request = b"GET /chat HTTP/1.1\r\nHost: example.com\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n";
        let mut parser = HttpRequestParser::new();
        parser.extend(request);

        assert!(parser.is_complete());
        assert!(parser.is_websocket_upgrade());

        let parsed = parser.parse().unwrap();
        assert_eq!(parsed.connection_type, ConnectionType::WebSocketUpgrade);
    }
}
