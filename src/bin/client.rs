use chrono;
use clap::Parser;
use reverse_tcp_proxy::{
    ConnectionType, HttpRequestParser, HttpResponseParser, ProxyConnectionState,
};
use socket2;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

/// Command-line arguments for the reverse TCP proxy client
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    // 명령어 인자 정의 구조체
    /// 서버 주소
    #[arg(long = "server", short = 's')]
    server: String,

    /// 백엔드 포트, 실제 로컬에서 서비스 되는 포트
    #[arg(long = "backend-port", short = 'b')]
    backend_port: u16,

    /// 실제 인터넷에서 접속할 포트
    #[arg(long = "public-port", short = 'p')]
    public_port: u16,

    /// 백엔드 서버에 연결시 타임아웃 시간
    #[arg(long = "connection-timeout", short = 't', default_value = "5")]
    connection_timeout: u64,

    /// 서버에 연결시 유후상태 타임아웃 시간
    #[arg(long = "idle-timeout", short = 'i', default_value = "0")]
    idle_timeout: u64,

    /// 재연결 시도 인터벌
    #[arg(long = "reconnect-interval", short = 'r', default_value = "5")]
    reconnect_interval: u64,

    /// 재연결 시도 횟수 제한
    #[arg(long = "max-reconnect", short = 'm', default_value = "0")]
    max_reconnect_attempts: u32,
}

// 웹소켓 업그레이드 응답인지 확인하는 함수
fn is_websocket_upgrade_response(data: &[u8]) -> bool {
    let mut parser = HttpResponseParser::new();
    parser.extend(data);
    parser.is_websocket_upgrade()
}

// 웹소켓 업그레이드 요청인지 확인하는 함수
fn is_websocket_request(data: &[u8]) -> bool {
    let mut parser = HttpRequestParser::new();
    parser.extend(data);
    parser.is_websocket_upgrade()
}

// 서버로 부터 받은 데이터를 백엔드로 릴레이 하는 함수
async fn handle_backend_connection(
    mut server_stream: TcpStream,
    backend_port: u16,
    connection_timeout: u64,
    idle_timeout: u64,
) {
    // 연결 상태
    let mut connection_state = ProxyConnectionState::Initial;

    // 백엔드 서비스와 연결하는 tcp 스트림 생성, 타임아웃이 적용됨
    let mut backend_stream = match timeout(
        Duration::from_secs(connection_timeout),
        TcpStream::connect(("127.0.0.1", backend_port)),
    )
    .await // 예외 처리 블록
    {
        Ok(Ok(stream)) => stream, // 연결 성공 시 스트림 반환
        Ok(Err(e)) => {
            error!("Failed to connect to backend:{}: {}", backend_port, e); // 연결 실패 시 에러 로깅 후 반환
            return;
        }
        Err(_) => {
            error!(
                "Connection timeout while connecting to backend:{}", // 타임아웃 발생시 에러 로깅 후 반환
                backend_port
            );
            return;
        }
    };

    // 위 코드에서 연결 성공시 반환되지 않았으므로 연결 성공 로깅
    info!(
        "Proxy session established: server <-> backend:{}",
        backend_port
    );

    // Configure socket options for better performance and error detection
    if let Err(e) = server_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on server stream: {}", e);
    }
    if let Err(e) = backend_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on backend stream: {}", e);
    }

    // Set SO_KEEPALIVE if supported by platform
    let socket_ref = socket2::SockRef::from(&server_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on server stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&backend_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on backend stream: {}", e);
    }

    // Try to set receive and send buffer sizes
    let socket_ref = socket2::SockRef::from(&server_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(262144) {
        // 256KB
        debug!("Failed to set receive buffer size on server stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(262144) {
        // 256KB
        debug!("Failed to set send buffer size on server stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&backend_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(262144) {
        // 256KB
        debug!("Failed to set receive buffer size on backend stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(262144) {
        // 256KB
        debug!("Failed to set send buffer size on backend stream: {}", e);
    }

    // Read any initial data from the server and forward it to the backend
    let mut initial_data = vec![0u8; 8192]; // Larger buffer for HTTP requests
    let n = match server_stream.read(&mut initial_data).await {
        Ok(n) => n,
        Err(e) => {
            error!("Error reading initial data from server: {}", e);
            return;
        }
    };

    // Process initial data if we received any
    if n > 0 {
        // 초기 데이터를 잘라서 실제 받은 부분만 사용
        initial_data.truncate(n);

        // HTTP 요청 파싱 및 연결 유형 결정
        let mut request_parser = HttpRequestParser::new();
        request_parser.extend(&initial_data);

        let mut is_http = false;
        let mut is_keep_alive = true;
        let mut is_connection_close = false;
        let mut is_websocket_req = false;

        // HTTP 요청인지 확인하고 파싱
        if request_parser.is_complete() {
            if let Ok(request) = request_parser.parse() {
                is_http = true;

                match request.connection_type {
                    ConnectionType::KeepAlive => {
                        is_keep_alive = true;
                        is_connection_close = false;
                        connection_state = ProxyConnectionState::Active;
                    }
                    ConnectionType::Close => {
                        is_keep_alive = false;
                        is_connection_close = true;
                        connection_state = ProxyConnectionState::FirstExchange;
                    }
                    ConnectionType::WebSocketUpgrade => {
                        is_websocket_req = true;
                        connection_state = ProxyConnectionState::WebSocket;
                    }
                }

                if is_websocket_req {
                    info!("Detected WebSocket upgrade request from server");
                } else {
                    info!(
                        "Detected HTTP request from server, connection-type: {:?}",
                        request.connection_type
                    );
                }
            }
        }

        // 백업: 이전 메서드로도 웹소켓 확인
        if !is_websocket_req && is_websocket_request(&initial_data) {
            is_websocket_req = true;
            connection_state = ProxyConnectionState::WebSocket;
            info!("Backup detection: WebSocket upgrade request detected");
        }

        // 디버깅 정보 로깅
        match std::str::from_utf8(&initial_data) {
            Ok(s) => {
                // Log the first line or up to 100 chars to avoid cluttering logs
                let preview = s
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(100)
                    .collect::<String>();
                debug!(
                    "Received from server: {}{}",
                    preview,
                    if preview.len() < s.len() { "..." } else { "" }
                );
            }
            Err(_) => debug!(
                "Received from server (hex): {}",
                initial_data
                    .iter()
                    .take(50)
                    .map(|b| format!("{:02x}", b))
                    .collect::<Vec<_>>()
                    .join(" ")
                    + "..."
            ),
        }

        // Forward initial data to backend
        if let Err(e) = backend_stream.write_all(&initial_data).await {
            error!("Error sending initial data to backend: {}", e);
            return;
        }

        // Check if this might be a WebSocket upgrade request
        if is_websocket_req {
            info!("WebSocket upgrade request detected, waiting for backend response");

            // Read the backend's response to see if it's a valid WebSocket upgrade response
            let mut response_buffer = vec![0u8; 8192];
            match timeout(
                Duration::from_secs(connection_timeout),
                backend_stream.read(&mut response_buffer),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => {
                    // 실제 받은 데이터만 사용
                    response_buffer.truncate(n);

                    // Forward the response to the server first
                    if let Err(e) = server_stream.write_all(&response_buffer).await {
                        error!("Error forwarding WebSocket response to server: {}", e);
                        return;
                    }

                    // HTTP 응답 파싱 및 웹소켓 업그레이드 확인
                    let mut response_parser = HttpResponseParser::new();
                    response_parser.extend(&response_buffer);

                    let is_valid_websocket = if response_parser.is_complete() {
                        if let Ok(response) = response_parser.parse() {
                            response.is_websocket_upgrade()
                        } else {
                            false
                        }
                    } else {
                        // 파서로 확인 실패 시 기존 방식으로 확인
                        is_websocket_upgrade_response(&response_buffer)
                    };

                    // Check if it's a valid WebSocket response
                    if is_valid_websocket {
                        info!("Backend confirmed WebSocket upgrade, switching to WebSocket mode");
                        connection_state = ProxyConnectionState::WebSocket;
                        handle_websocket_connection(server_stream, backend_stream).await;
                        return;
                    } else {
                        info!(
                            "Backend response was not a valid WebSocket upgrade, continuing with normal proxy"
                        );
                    }
                }
                _ => {
                    error!("Failed to get backend response for WebSocket upgrade");
                    return;
                }
            }
        }

        // For HTTP requests that specifically request connection close, we should handle them differently
        if is_http && is_connection_close {
            info!(
                "HTTP request with Connection: close detected, will close connection after response"
            );
            // Use special handling for Connection: close requests
            handle_one_time_http_request(server_stream, backend_stream).await;
            return;
        }
    } else {
        // No initial data received, just a notification of connection
        debug!("No initial data received from server");
    }
    // Use tokio::io::split to get owned halves for async tasks
    let (mut sr, mut sw) = tokio::io::split(server_stream);
    let (mut br, mut bw) = tokio::io::split(backend_stream);

    // Configure idle timeout if specified
    let idle_timeout_duration = if idle_timeout > 0 {
        Some(Duration::from_secs(idle_timeout))
    } else {
        None
    };

    // Forward data from server to backend with optional timeout
    let to_backend = tokio::spawn(async move {
        info!("Transferring data: server -> backend");

        let result = if let Some(timeout_duration) = idle_timeout_duration {
            // With timeout - will abort if no data transfer happens within the timeout
            let mut buffer = [0u8; 16384]; // Larger buffer for better performance with HTTP
            let mut total_bytes: u64 = 0;

            loop {
                match timeout(timeout_duration, sr.read(&mut buffer)).await {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => match bw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            debug!("server -> backend: {} bytes transferred", n);
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("server -> backend: Connection closed: {}", e);
                            } else {
                                error!("Error copying server -> backend: {}", e);
                            }
                            break;
                        }
                    },
                    Ok(Err(e)) => {
                        if is_connection_error(&e) {
                            info!("server -> backend: Connection closed: {}", e);
                        } else {
                            error!("Error reading from server: {}", e);
                        }
                        break;
                    }
                    Err(_) => {
                        info!(
                            "server -> backend: Idle timeout reached after {} seconds",
                            timeout_duration.as_secs()
                        );
                        break;
                    }
                }
            }
            Ok(total_bytes)
        } else {
            // No timeout - use standard copy function
            tokio::io::copy(&mut sr, &mut bw).await
        };

        match result {
            Ok(bytes) => info!("server -> backend: {} bytes transferred", bytes),
            Err(e) => {
                if is_connection_error(&e) {
                    info!("server -> backend: Connection closed: {}", e);
                } else {
                    error!("Error copying server -> backend: {}", e);
                }
            }
        }
    });

    // Forward data from backend to server with optional timeout
    let to_server = tokio::spawn(async move {
        info!("Transferring data: backend -> server");

        let result = if let Some(timeout_duration) = idle_timeout_duration {
            // With timeout - will abort if no data transfer happens within the timeout
            let mut buffer = [0u8; 16384]; // Larger buffer for better performance with HTTP
            let mut total_bytes: u64 = 0;

            loop {
                match timeout(timeout_duration, br.read(&mut buffer)).await {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => match sw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            debug!("backend -> server: {} bytes transferred", n);
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("backend -> server: Connection closed: {}", e);
                            } else {
                                error!("Error copying backend -> server: {}", e);
                            }
                            break;
                        }
                    },
                    Ok(Err(e)) => {
                        if is_connection_error(&e) {
                            info!("backend -> server: Connection closed: {}", e);
                        } else {
                            error!("Error reading from backend: {}", e);
                        }
                        break;
                    }
                    Err(_) => {
                        info!(
                            "backend -> server: Idle timeout reached after {} seconds",
                            timeout_duration.as_secs()
                        );
                        break;
                    }
                }
            }
            Ok(total_bytes)
        } else {
            // No timeout - use standard copy function
            tokio::io::copy(&mut br, &mut sw).await
        };

        match result {
            Ok(bytes) => info!("backend -> server: {} bytes transferred", bytes),
            Err(e) => {
                if is_connection_error(&e) {
                    info!("backend -> server: Connection closed: {}", e);
                } else {
                    error!("Error copying backend -> server: {}", e);
                }
            }
        }
    });

    let _ = tokio::try_join!(to_backend, to_server);
    info!("Proxy session closed.");
}

// Handle a one-time HTTP request that should close after completion
async fn handle_one_time_http_request(mut server_stream: TcpStream, mut backend_stream: TcpStream) {
    debug!("Handling one-time HTTP request with Connection: close");

    // HTTP 응답 파서 초기화
    let mut response_parser = HttpResponseParser::new();

    // Create a buffer for the backend response
    let mut response_buffer = vec![0u8; 16384]; // 초기 버퍼 크기 줄임

    // Wait for response from backend (with a generous timeout)
    match timeout(
        Duration::from_secs(30),
        backend_stream.read(&mut response_buffer),
    )
    .await
    {
        Ok(Ok(n)) => {
            if n > 0 {
                // 실제 받은 데이터만 사용
                response_buffer.truncate(n);

                // 응답 파서에 데이터 추가
                response_parser.extend(&response_buffer);

                // Log response summary
                if let Ok(text) = std::str::from_utf8(&response_buffer) {
                    let first_line = text.lines().next().unwrap_or("");
                    info!("Received backend response: {}", first_line);
                }

                // Forward response to server
                if let Err(e) = server_stream.write_all(&response_buffer).await {
                    error!("Error sending response to server: {}", e);
                    return;
                }

                // 응답이 완전한지 확인
                let mut is_complete = response_parser.is_complete();
                let mut has_chunked_encoding = false;
                let mut content_length = None;
                let mut headers_end = 0;

                // 응답 파싱이 가능하면 상세 정보 추출
                if let Ok(response) = response_parser.parse() {
                    // 청크 인코딩 확인
                    if let Some(te) = response.headers.get("transfer-encoding") {
                        if let Ok(value) = te.to_str() {
                            has_chunked_encoding = value.to_lowercase().contains("chunked");
                        }
                    }

                    // Content-Length 추출
                    if let Some(cl) = response.headers.get("content-length") {
                        if let Ok(value) = cl.to_str() {
                            if let Ok(len) = value.parse::<usize>() {
                                content_length = Some(len);
                            }
                        }
                    }

                    // HTTP 버전 확인 (HTTP/1.0은 기본적으로 Connection: close)
                    let is_http10 = response.version == "HTTP/1.0";

                    // Connection: close 헤더 확인
                    let has_connection_close = response.connection_type == ConnectionType::Close;

                    // 응답 본문 시작점 찾기
                    if let Some(body) = &response.body {
                        headers_end = response_buffer.len() - body.len();
                    }

                    // 디버그 정보 출력
                    debug!(
                        "HTTP response: status={}, version={}, connection-type={:?}, content-length={:?}, chunked={}",
                        response.status,
                        response.version,
                        response.connection_type,
                        content_length,
                        has_chunked_encoding
                    );
                }

                // Content-Length 기반으로 응답 완전성 확인
                if let Some(len) = content_length {
                    if let Ok(text) = std::str::from_utf8(&response_buffer) {
                        if let Some(pos) = text.find("\r\n\r\n") {
                            headers_end = pos + 4;
                            let body_received = response_buffer.len() - headers_end;
                            is_complete = body_received >= len;

                            if !is_complete {
                                debug!(
                                    "Partial HTTP response: {}/{} bytes of body",
                                    body_received, len
                                );
                            }
                        }
                    }
                } else if has_chunked_encoding {
                    // 청크 인코딩의 경우 마지막 청크(0 크기)가 있는지 확인
                    is_complete = response_buffer.windows(5).any(|w| w == b"0\r\n\r\n");

                    if !is_complete {
                        debug!("Chunked encoding detected but response is incomplete");
                    }
                }

                // 응답이 완전하지 않으면 계속 읽기
                if !is_complete {
                    debug!("Response is incomplete, continuing to read");

                    // 필요하면 버퍼 확장
                    if response_buffer.len() > response_buffer.capacity() / 2 {
                        response_buffer.reserve(response_buffer.capacity());
                    }

                    // Read remaining data in a loop
                    loop {
                        // 버퍼 크기 조정 (필요한 경우 두 배로 확장)
                        if response_buffer.len() > response_buffer.capacity() / 2 {
                            let new_capacity = response_buffer.capacity() * 2;
                            let mut new_buffer = Vec::with_capacity(new_capacity);
                            new_buffer.extend_from_slice(&response_buffer);
                            response_buffer = new_buffer;
                        }

                        let buf_pos = response_buffer.len();
                        response_buffer.resize(response_buffer.capacity(), 0);

                        match timeout(
                            Duration::from_secs(5),
                            backend_stream.read(&mut response_buffer[buf_pos..]),
                        )
                        .await
                        {
                            Ok(Ok(0)) => {
                                // EOF
                                response_buffer.truncate(buf_pos);
                                break;
                            }
                            Ok(Ok(n)) => {
                                // 실제 읽은 크기로 버퍼 조정
                                response_buffer.truncate(buf_pos + n);

                                // 응답 파서에 새 데이터 추가
                                response_parser.extend(&response_buffer[buf_pos..]);

                                // Forward the additional data
                                if let Err(e) =
                                    server_stream.write_all(&response_buffer[buf_pos..]).await
                                {
                                    error!(
                                        "Error sending additional response data to server: {}",
                                        e
                                    );
                                    break;
                                }

                                debug!("Received {} more bytes from backend", n);

                                // 응답 완전성 다시 확인
                                is_complete = if let Some(len) = content_length {
                                    // Content-Length 기반 확인
                                    if headers_end > 0 {
                                        response_buffer.len() - headers_end >= len
                                    } else {
                                        false
                                    }
                                } else if has_chunked_encoding {
                                    // 청크 인코딩 확인
                                    let chunk_end =
                                        response_buffer.windows(5).any(|w| w == b"0\r\n\r\n");
                                    if chunk_end {
                                        debug!("Found end of chunked encoding");
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    // 명확한 종료 조건이 없으면 계속 읽기
                                    false
                                };

                                if is_complete {
                                    debug!("Response is now complete, stopping read loop");
                                    break;
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Error reading additional data from backend: {}", e);
                                break;
                            }
                            Err(_) => {
                                debug!(
                                    "Timeout reading additional data from backend, assuming response complete"
                                );
                                break;
                            }
                        }
                    }
                }

                info!(
                    "One-time HTTP request completed, total response size: {} bytes",
                    response_buffer.len()
                );
            } else {
                error!("Backend closed connection without sending response");
            }
        }
        Ok(Err(e)) => {
            error!("Error reading response from backend: {}", e);
        }
        Err(_) => {
            error!("Timeout waiting for response from backend");
        }
    }

    // Close connections (explicitly, though they would be closed when dropped anyway)
    debug!("Shutting down one-time HTTP connection streams");
    let _ = server_stream.shutdown().await;
    let _ = backend_stream.shutdown().await;
}

// Extract Content-Length from HTTP headers
fn extract_content_length(http_text: &str) -> Option<usize> {
    for line in http_text.lines() {
        if line.to_lowercase().starts_with("content-length:") {
            if let Some(len_str) = line.split(':').nth(1) {
                if let Ok(len) = len_str.trim().parse::<usize>() {
                    return Some(len);
                }
            }
        }
    }
    None
}

// Handle WebSocket connection with special handling
async fn handle_websocket_connection(mut server_stream: TcpStream, mut backend_stream: TcpStream) {
    debug!("Starting WebSocket proxy mode between server and backend");

    // Use the split function to get owned halves for async tasks
    let (mut sr, mut sw) = tokio::io::split(server_stream);
    let (mut br, mut bw) = tokio::io::split(backend_stream);

    // Forward data from server to backend without timeout (WebSockets can idle)
    let to_backend = tokio::spawn(async move {
        info!("WebSocket: Transferring data: server -> backend");
        let mut buffer = [0u8; 16384];
        let mut total_bytes: u64 = 0;

        loop {
            match sr.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: server side closed the connection");
                    break;
                }
                Ok(n) => {
                    match bw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: server -> backend: {} bytes transferred",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: server -> backend: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying server -> backend: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: server -> backend: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from server: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: server -> backend: {} total bytes transferred",
            total_bytes
        );
    });

    // Forward data from backend to server without timeout
    let to_server = tokio::spawn(async move {
        info!("WebSocket: Transferring data: backend -> server");
        let mut buffer = [0u8; 16384];
        let mut total_bytes: u64 = 0;

        loop {
            match br.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: backend side closed the connection");
                    break;
                }
                Ok(n) => {
                    match sw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: backend -> server: {} bytes transferred",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: backend -> server: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying backend -> server: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: backend -> server: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from backend: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: backend -> server: {} total bytes transferred",
            total_bytes
        );
    });

    let _ = tokio::try_join!(to_backend, to_server);
    info!("WebSocket proxy session closed");
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Parse command-line arguments
    let args = Args::parse();

    // Create a flag to track graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    // Set up signal handler for graceful shutdown
    #[cfg(unix)]
    {
        let sig_int = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())
            .expect("Failed to set up SIGINT handler");
        let sig_term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("Failed to set up SIGTERM handler");
        let r = running.clone();

        tokio::spawn(async move {
            use tokio::select;
            use tokio::signal::unix::SignalKind;

            let mut sig_int = sig_int;
            let mut sig_term = sig_term;

            select! {
                _ = sig_int.recv() => {
                    info!("Received SIGINT, shutting down gracefully...");
                },
                _ = sig_term.recv() => {
                    info!("Received SIGTERM, shutting down gracefully...");
                }
            }

            r.store(false, Ordering::SeqCst);
        });
    }

    // Main reconnection loop with backoff strategy
    let mut reconnect_count = 0;
    let mut backoff_time = args.reconnect_interval;

    while running.load(Ordering::SeqCst) {
        // Check max reconnect limit
        if args.max_reconnect_attempts > 0 && reconnect_count >= args.max_reconnect_attempts {
            error!(
                "Reached maximum reconnection attempts ({}), exiting",
                args.max_reconnect_attempts
            );
            break;
        }

        if reconnect_count > 0 {
            info!(
                "Reconnection attempt {}/{} after {} seconds...",
                reconnect_count,
                if args.max_reconnect_attempts > 0 {
                    args.max_reconnect_attempts.to_string()
                } else {
                    "unlimited".to_string()
                },
                backoff_time
            );
        } else {
            info!("Connecting to server at {}...", &args.server);
        }

        // Attempt to connect to the server
        match connect_and_serve(&args).await {
            Ok(_) => {
                info!("Connection closed normally");
                // Reset backoff on successful connection
                backoff_time = args.reconnect_interval;
                reconnect_count = 0;
            }
            Err(e) => {
                error!("Connection error: {}", e);
                reconnect_count += 1;

                // Implement exponential backoff with a cap
                if reconnect_count > 1 {
                    backoff_time = std::cmp::min(backoff_time * 2, 300); // Cap at 5 minutes
                }
            }
        }

        // Check if we should exit
        if !running.load(Ordering::SeqCst) {
            info!("Shutdown signal received, exiting reconnection loop");
            break;
        }

        // Check if reconnection is enabled
        if args.reconnect_interval == 0 {
            error!("Reconnection disabled, exiting");
            break;
        }

        info!("Reconnecting in {} seconds...", backoff_time);
        for _ in 0..backoff_time as usize {
            if !running.load(Ordering::SeqCst) {
                break;
            }
            tokio::time::sleep(Duration::from_secs(1)).await;
        }

        if !running.load(Ordering::SeqCst) {
            info!("Shutdown signal received during wait, exiting reconnection loop");
            break;
        }
    }

    info!("Client shutting down...");
}

// The main connection and serving logic extracted from main
async fn connect_and_serve(args: &Args) -> Result<(), Box<dyn std::error::Error>> {
    // Connect to the server's control port using the provided address
    let connect_result = timeout(
        Duration::from_secs(args.connection_timeout),
        TcpStream::connect(&args.server),
    )
    .await;

    let mut control_stream = match connect_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(format!("Failed to connect to server: {}", e).into()),
        Err(_) => return Err(format!("Connection timeout while connecting to server").into()),
    };

    info!("Connected to server at {}", &args.server);

    // Configure socket options for better performance
    if let Err(e) = control_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on control stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&control_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on control stream: {}", e);
    }

    // Register with the server using the provided backend and public ports
    let register_msg = format!("REGISTER {} {}\n", args.backend_port, args.public_port);
    control_stream.write_all(register_msg.as_bytes()).await?;
    info!(
        "Registered backend:{} as public:{}",
        args.backend_port, args.public_port
    );

    let mut reader = BufReader::new(&mut control_stream);

    // Track active sessions to avoid race conditions during quick refreshes
    let mut active_sessions = std::collections::HashSet::new();

    // Connection pool management to reduce connection setup time
    let mut last_connection_time = std::time::Instant::now();

    loop {
        let mut line = String::new();
        // Wait for the server to notify about a new connection
        match reader.read_line(&mut line).await {
            Ok(0) => {
                return Err("Server closed connection".into());
            }
            Ok(_) => {
                // Process the line
                let trimmed = line.trim();
                debug!("Received from server: '{}'", trimmed);

                if trimmed == "READY" {
                    info!("Received READY signal from server");

                    // Wait for CONNECT after READY
                    let mut connect_line = String::new();
                    match reader.read_line(&mut connect_line).await {
                        Ok(0) => {
                            return Err("Server closed connection while waiting for CONNECT".into());
                        }
                        Ok(_) => {
                            let connect_trimmed = connect_line.trim();
                            debug!("Received from server: '{}'", connect_trimmed);

                            if connect_trimmed.starts_with("CONNECT") {
                                // Extract handshake ID from CONNECT message
                                let parts: Vec<&str> = connect_trimmed.split_whitespace().collect();

                                if parts.len() >= 2 {
                                    let handshake_id = parts[1];
                                    info!(
                                        "Received CONNECT signal with handshake ID: {}",
                                        handshake_id
                                    );
                                    let session_id =
                                        chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0);

                                    // Check if we have too many active sessions (prevent resource exhaustion)
                                    if active_sessions.len() >= 100 {
                                        warn!(
                                            "Too many active sessions ({}), cleaning up old ones",
                                            active_sessions.len()
                                        );
                                        active_sessions.clear();
                                    }

                                    // Add this session to active set
                                    active_sessions.insert(handshake_id.to_string());

                                    // Check if there was a recent connection (for quick page refreshes)
                                    let connection_interval = std::time::Instant::now()
                                        .duration_since(last_connection_time);
                                    if connection_interval.as_millis() < 500 {
                                        // For very quick refreshes, add a small delay to prevent race conditions
                                        debug!(
                                            "Quick refresh detected ({} ms since last connection), adding small delay",
                                            connection_interval.as_millis()
                                        );
                                        tokio::time::sleep(Duration::from_millis(50)).await;
                                    }

                                    last_connection_time = std::time::Instant::now();

                                    info!(
                                        "Session {}: Connecting back to server with handshake ID {}...",
                                        session_id, handshake_id
                                    );

                                    // Use connection timeout for connecting back
                                    let connect_attempt = timeout(
                                        Duration::from_secs(args.connection_timeout),
                                        TcpStream::connect(&args.server),
                                    )
                                    .await;

                                    match connect_attempt {
                                        Ok(Ok(mut server_stream)) => {
                                            info!("Connected back to server, sending handshake ID");

                                            // Set TCP_NODELAY on this connection too
                                            if let Err(e) = server_stream.set_nodelay(true) {
                                                debug!(
                                                    "Failed to set TCP_NODELAY on server callback stream: {}",
                                                    e
                                                );
                                            }

                                            // Send the handshake ID first
                                            let callback_msg =
                                                format!("CALLBACK {}\n", handshake_id);
                                            if let Err(e) = server_stream
                                                .write_all(callback_msg.as_bytes())
                                                .await
                                            {
                                                error!(
                                                    "Failed to send handshake ID to server: {}",
                                                    e
                                                );
                                                continue;
                                            }

                                            info!("Successfully sent handshake ID to server");
                                            let backend_port = args.backend_port;
                                            let connection_timeout = args.connection_timeout;
                                            let idle_timeout = args.idle_timeout;
                                            let handshake_id_clone = handshake_id.to_string();
                                            let active_sessions_clone = active_sessions.clone();

                                            tokio::spawn(async move {
                                                info!(
                                                    "Session {}: Proxy session established with backend:{}",
                                                    session_id, backend_port
                                                );
                                                handle_backend_connection(
                                                    server_stream,
                                                    backend_port,
                                                    connection_timeout,
                                                    idle_timeout,
                                                )
                                                .await;

                                                // Remove from active sessions when done
                                                if let Ok(mut sessions) =
                                                    tokio::sync::Mutex::new(active_sessions_clone)
                                                        .try_lock()
                                                {
                                                    sessions.remove(&handshake_id_clone);
                                                    debug!(
                                                        "Removed session {} from active sessions, {} remain",
                                                        handshake_id_clone,
                                                        sessions.len()
                                                    );
                                                }
                                            });
                                        }
                                        Ok(Err(e)) => {
                                            error!(
                                                "Session {}: Failed to connect back to server: {}",
                                                session_id, e
                                            );
                                        }
                                        Err(_) => {
                                            error!(
                                                "Session {}: Connection timeout while connecting back to server",
                                                session_id
                                            );
                                        }
                                    }
                                } else {
                                    error!(
                                        "CONNECT message missing handshake ID: {}",
                                        connect_trimmed
                                    );
                                }
                            } else {
                                error!("Expected CONNECT but received: '{}'", connect_trimmed);
                            }
                        }
                        Err(e) => {
                            return Err(format!("Error reading from server: {}", e).into());
                        }
                    }
                } else if trimmed.starts_with("ERROR:") {
                    error!("Received error from server: {}", trimmed);
                } else {
                    error!("Unexpected message from server: '{}'", trimmed);
                }
            }
            Err(e) => {
                return Err(format!("Error reading from server: {}", e).into());
            }
        }
    }
}

// Helper function to determine if an error is a common connection closure
fn is_connection_error(e: &std::io::Error) -> bool {
    match e.kind() {
        std::io::ErrorKind::BrokenPipe
        | std::io::ErrorKind::ConnectionReset
        | std::io::ErrorKind::ConnectionAborted
        | std::io::ErrorKind::ConnectionRefused
        | std::io::ErrorKind::TimedOut => true,
        _ => false,
    }
}
