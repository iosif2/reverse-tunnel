use crate::common::http_conn::connection::determine_connection_type;
use crate::common::http_conn::types::HttpRequest;
use crate::common::http_conn::upgrade::is_websocket_upgrade_request;
use bytes::BytesMut;
use http::HeaderMap;
use httparse;
use std::io;

#[derive(Debug)]
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

    pub fn is_websocket_upgrade(&self) -> bool {
        if let Ok(req) = self.parse() {
            return is_websocket_upgrade_request(&req.headers);
        }
        false
    }

    pub fn get_buffer(&self) -> &[u8] {
        &self.buf
    }

    pub fn clear(&mut self) {
        self.buf.clear();
    }
}
