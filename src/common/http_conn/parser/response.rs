use crate::common::http_conn::{
    HttpResponse, determine_connection_type, is_websocket_upgrade_response,
};
use bytes::BytesMut;
use http::HeaderMap;
use httparse;
use std::io;

pub struct HttpResponseParser {
    buf: BytesMut,
    headers: [httparse::Header<'static>; 64],
}

impl HttpResponseParser {
    pub fn new() -> Self {
        Self {
            buf: BytesMut::with_capacity(8192),
            headers: [httparse::EMPTY_HEADER; 64],
        }
    }

    pub fn extend(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    pub fn is_complete(&self) -> bool {
        if let Some(headers_end) = self.buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let headers_str = std::str::from_utf8(&self.buf[..headers_end])
                .unwrap_or_default()
                .to_lowercase();

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

            if headers_str.contains("transfer-encoding: chunked") {
                return self.buf.windows(5).any(|w| w == b"0\r\n\r\n");
            }

            let mut resp = [httparse::EMPTY_HEADER; 64];
            let mut res = httparse::Response::new(&mut resp);
            if let Ok(httparse::Status::Complete(_)) = res.parse(&self.buf) {
                if let Some(status) = res.code {
                    if status < 200 || status == 204 || status == 304 {
                        return true;
                    }
                }
            }

            return headers_end + 4 < self.buf.len();
        }
        false
    }

    pub fn parse(&self) -> Result<HttpResponse, io::Error> {
        let mut headers = [httparse::EMPTY_HEADER; 64];
        let mut res = httparse::Response::new(&mut headers);

        match res.parse(&self.buf) {
            Ok(httparse::Status::Complete(headers_len)) => {
                let status = res.code.unwrap_or(200);
                let reason = res.reason.unwrap_or("OK").to_string();
                let version = match res.version.unwrap_or(1) {
                    0 => "HTTP/1.0",
                    _ => "HTTP/1.1",
                }
                .to_string();

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

                let body = if self.buf.len() > headers_len {
                    Some(self.buf[headers_len..].to_vec())
                } else {
                    None
                };

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
            return is_websocket_upgrade_response(res.status, &res.headers);
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
