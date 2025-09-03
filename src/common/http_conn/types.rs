use crate::common::http_conn::connection::ConnectionType;
use axum::body::Body;
use axum::extract::Request;
use http::{HeaderMap, Method, Version};

#[derive(Debug)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub version: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    pub connection_type: ConnectionType,
}

#[derive(Debug)]
pub struct HttpResponse {
    pub status: u16,
    pub reason: String,
    pub version: String,
    pub headers: HeaderMap,
    pub body: Option<Vec<u8>>,
    pub connection_type: ConnectionType,
}

impl HttpRequest {
    /// axum Request<Body>에서 HttpRequest로 변환
    pub async fn from_axum_request(
        req: Request<Body>,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (parts, body) = req.into_parts();

        // 메서드 변환
        let method = parts.method.to_string();

        // 경로 추출
        let path = parts.uri.path().to_string();

        // HTTP 버전 변환
        let version = match parts.version {
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_11 => "HTTP/1.1",
            Version::HTTP_2 => "HTTP/2.0",
            Version::HTTP_3 => "HTTP/3.0",
            _ => "HTTP/1.1",
        }
        .to_string();

        // 헤더 복사
        let headers = parts.headers;

        // 본문 읽기
        let body_bytes = if let Ok(body_bytes) = axum::body::to_bytes(body, usize::MAX).await {
            if body_bytes.is_empty() {
                None
            } else {
                Some(body_bytes.to_vec())
            }
        } else {
            None
        };

        // 연결 타입 결정
        let connection_type =
            crate::common::http_conn::connection::determine_connection_type(&headers, &version);

        Ok(HttpRequest {
            method,
            path,
            version,
            headers,
            body: body_bytes,
            connection_type,
        })
    }
}

impl From<Request<Body>> for HttpRequest {
    fn from(req: Request<Body>) -> Self {
        // 동기적 변환 (본문은 빈 상태로)
        let (parts, _body) = req.into_parts();

        let method = parts.method.to_string();
        let path = parts.uri.path().to_string();
        let version = match parts.version {
            Version::HTTP_10 => "HTTP/1.0",
            Version::HTTP_11 => "HTTP/1.1",
            Version::HTTP_2 => "HTTP/2.0",
            Version::HTTP_3 => "HTTP/3.0",
            _ => "HTTP/1.1",
        }
        .to_string();
        let headers = parts.headers;
        let connection_type =
            crate::common::http_conn::connection::determine_connection_type(&headers, &version);

        HttpRequest {
            method,
            path,
            version,
            headers,
            body: None, // 본문은 비워둠
            connection_type,
        }
    }
}

/// axum Request를 원시 HTTP 바이트로 직렬화
pub async fn serialize_request_to_bytes(
    req: Request<Body>,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let (parts, body) = req.into_parts();

    let mut request_bytes = Vec::new();

    // 1. HTTP 요청 라인 작성
    let method = parts.method.to_string();
    let path = parts.uri.path();
    let query = parts
        .uri
        .query()
        .map(|q| format!("?{}", q))
        .unwrap_or_default();
    let full_path = format!("{}{}", path, query);

    let version = match parts.version {
        Version::HTTP_10 => "HTTP/1.0",
        Version::HTTP_11 => "HTTP/1.1",
        Version::HTTP_2 => "HTTP/2.0",
        Version::HTTP_3 => "HTTP/3.0",
        _ => "HTTP/1.1",
    };

    let request_line = format!("{} {} {}\r\n", method, full_path, version);
    request_bytes.extend_from_slice(request_line.as_bytes());

    // 2. 헤더 작성
    for (name, value) in parts.headers.iter() {
        let header_line = format!("{}: {}\r\n", name, value.to_str().unwrap_or(""));
        request_bytes.extend_from_slice(header_line.as_bytes());
    }

    // 3. 빈 줄 (헤더와 본문 구분)
    request_bytes.extend_from_slice(b"\r\n");

    // 4. 본문 추가
    if let Ok(body_bytes) = axum::body::to_bytes(body, usize::MAX).await {
        if !body_bytes.is_empty() {
            request_bytes.extend_from_slice(&body_bytes);
        }
    }

    Ok(request_bytes)
}

/// HttpRequest를 원시 HTTP 바이트로 직렬화
pub fn serialize_http_request_to_bytes(
    req: &HttpRequest,
) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let mut request_bytes = Vec::new();

    // 1. HTTP 요청 라인
    let request_line = format!("{} {} {}\r\n", req.method, req.path, req.version);
    request_bytes.extend_from_slice(request_line.as_bytes());

    // 2. 헤더
    for (name, value) in req.headers.iter() {
        let header_line = format!("{}: {}\r\n", name, value.to_str().unwrap_or(""));
        request_bytes.extend_from_slice(header_line.as_bytes());
    }

    // 3. 빈 줄
    request_bytes.extend_from_slice(b"\r\n");

    // 4. 본문
    if let Some(ref body) = req.body {
        request_bytes.extend_from_slice(body);
    }

    Ok(request_bytes)
}
