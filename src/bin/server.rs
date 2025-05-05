use chrono;
use clap::Parser;
use reverse_tcp_proxy::{
    ConnectionType, HttpRequestParser, HttpResponseParser, ProxyConnectionState,
};
use socket2;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info};

/// Command-line arguments for the reverse TCP proxy server
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Address to listen for client registrations (e.g., 0.0.0.0:7000)
    #[arg(long = "listen", short = 'l', default_value = "0.0.0.0:7000")]
    listen: String,

    /// Connection timeout in seconds for handling public connections
    #[arg(long = "connection-timeout", short = 't', default_value = "5")]
    connection_timeout: u64,

    /// Idle timeout in seconds for proxy sessions (0 for no timeout)
    #[arg(long = "idle-timeout", short = 'i', default_value = "0")]
    idle_timeout: u64,
}

// Structure to hold information about a registered client
struct ClientInfo {
    backend_port: u16, // Backend port on the client side
    public_port: u16,  // Public port on the server side
    handshake_channel: Option<tokio::sync::oneshot::Sender<TcpStream>>,
    handshake_id: String,
    last_activity: std::time::Instant, // Track last activity time
    reconnect_attempts: u32,           // Track reconnection attempts
}

// Helper to parse HTTP headers and connection info
fn parse_http_request(data: &[u8]) -> Option<(bool, bool, String)> {
    let mut parser = HttpRequestParser::new();
    parser.extend(data);

    if !parser.is_complete() {
        return None; // 완전한 HTTP 요청이 아님
    }

    match parser.parse() {
        Ok(request) => {
            // 경로 추출
            let path = request.path;

            // 연결 타입 확인
            match request.connection_type {
                ConnectionType::KeepAlive => Some((true, false, path)),
                ConnectionType::Close => Some((false, true, path)),
                ConnectionType::WebSocketUpgrade => Some((true, false, path)), // 웹소켓은 별도 처리
            }
        }
        Err(_) => None,
    }
}

// Check if a request is a WebSocket upgrade request
fn is_websocket_upgrade(data: &[u8]) -> bool {
    let mut parser = HttpRequestParser::new();
    parser.extend(data);
    parser.is_websocket_upgrade()
}

// Proxies data between the public stream and the client stream asynchronously
async fn handle_public_connection(
    mut public_stream: TcpStream,
    mut client_stream: TcpStream,
    initial_data: Vec<u8>,
    idle_timeout: u64,
) {
    info!("Proxy session established: public <-> client");

    // 연결 상태 추적
    let mut connection_state = ProxyConnectionState::Initial;

    // Configure socket options for better performance and error detection
    if let Err(e) = public_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on public stream: {}", e);
    }
    if let Err(e) = client_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on client stream: {}", e);
    }

    // Set SO_KEEPALIVE if supported by platform
    let socket_ref = socket2::SockRef::from(&public_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on public stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&client_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on client stream: {}", e);
    }

    // Try to set receive and send buffer sizes
    let socket_ref = socket2::SockRef::from(&public_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(262144) {
        // 256KB
        debug!("Failed to set receive buffer size on public stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(262144) {
        // 256KB
        debug!("Failed to set send buffer size on public stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&client_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(262144) {
        // 256KB
        debug!("Failed to set receive buffer size on client stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(262144) {
        // 256KB
        debug!("Failed to set send buffer size on client stream: {}", e);
    }

    // HTTP 요청 파싱 및 연결 타입 결정
    let mut http_parser = HttpRequestParser::new();
    http_parser.extend(&initial_data);

    let mut is_http = false;
    let mut is_websocket = false;
    let mut is_keep_alive = true;
    let mut is_connection_close = false;
    let mut request_path = String::new();

    if http_parser.is_complete() {
        match http_parser.parse() {
            Ok(request) => {
                is_http = true;
                request_path = request.path;

                // 연결 타입에 따른 처리 결정
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
                        is_websocket = true;
                        connection_state = ProxyConnectionState::WebSocket;
                    }
                }

                info!(
                    "Detected HTTP request for path: {}, connection-type: {:?}",
                    request_path, request.connection_type
                );
            }
            Err(e) => {
                // HTTP 파싱 실패했지만 계속 진행 (비 HTTP 트래픽일 수 있음)
                debug!("Failed to parse HTTP request: {}", e);
            }
        }
    } else {
        // 완전한 HTTP 요청이 아님 - 단순 TCP 트래픽으로 처리
        debug!("Not a complete HTTP request - handling as general TCP traffic");
    }

    // 웹소켓 업그레이드 요청 확인 (기존 코드와의 호환성을 위해 별도 체크)
    if http_parser.is_websocket_upgrade() {
        is_websocket = true;
        connection_state = ProxyConnectionState::WebSocket;
        info!(
            "Detected WebSocket upgrade request for path: {}",
            request_path
        );
    }

    // Write the initial data to the client stream first
    if !initial_data.is_empty() {
        if let Err(e) = client_stream.write_all(&initial_data).await {
            error!("Error sending initial data to client: {}", e);
            return;
        }
    }

    // For WebSocket connections, we need to handle them specially
    if is_websocket {
        info!(
            "WebSocket upgrade detected - using bidirectional streaming mode with longer timeouts"
        );
        handle_websocket_proxy(public_stream, client_stream).await;
        return;
    }

    // For HTTP requests with Connection: close, handle them specially
    if is_http && is_connection_close {
        info!("HTTP Connection: close - using one-time request handler");
        handle_one_time_proxy(&mut public_stream, &mut client_stream).await;
        return;
    }

    // Standard processing for persistent connections
    // Use the split function to get owned halves
    let (mut pr, mut pw) = tokio::io::split(public_stream);
    let (mut cr, mut cw) = tokio::io::split(client_stream);

    // Configure idle timeout if specified
    let idle_timeout_duration = if idle_timeout > 0 {
        Some(Duration::from_secs(idle_timeout))
    } else {
        None
    };

    // Forward data from public to client with optional timeout
    let to_client = tokio::spawn(async move {
        info!("Transferring data: public -> client");

        let result = if let Some(timeout_duration) = idle_timeout_duration {
            // With timeout - will abort if no data transfer happens within the timeout
            let mut buffer = [0u8; 16384]; // Larger buffer for HTTP responses
            let mut total_bytes: u64 = 0;

            loop {
                match timeout(timeout_duration, pr.read(&mut buffer)).await {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => match cw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            debug!("public -> client: {} bytes transferred", n);
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("public -> client: Connection closed: {}", e);
                            } else {
                                error!("Error copying public -> client: {}", e);
                            }
                            break;
                        }
                    },
                    Ok(Err(e)) => {
                        if is_connection_error(&e) {
                            info!("public -> client: Connection closed: {}", e);
                        } else {
                            error!("Error reading from public: {}", e);
                        }
                        break;
                    }
                    Err(_) => {
                        info!(
                            "public -> client: Idle timeout reached after {} seconds",
                            timeout_duration.as_secs()
                        );
                        break;
                    }
                }
            }
            Ok(total_bytes)
        } else {
            // No timeout - use standard copy function
            tokio::io::copy(&mut pr, &mut cw).await
        };

        match result {
            Ok(bytes) => info!("public -> client: {} bytes transferred", bytes),
            Err(e) => {
                if is_connection_error(&e) {
                    info!("public -> client: Connection closed: {}", e);
                } else {
                    error!("Error copying public -> client: {}", e);
                }
            }
        }
    });

    // Forward data from client to public with optional timeout
    let to_public = tokio::spawn(async move {
        info!("Transferring data: client -> public");

        let result = if let Some(timeout_duration) = idle_timeout_duration {
            // With timeout - will abort if no data transfer happens within the timeout
            let mut buffer = [0u8; 16384]; // Larger buffer for HTTP requests
            let mut total_bytes: u64 = 0;

            loop {
                match timeout(timeout_duration, cr.read(&mut buffer)).await {
                    Ok(Ok(0)) => break, // EOF
                    Ok(Ok(n)) => match pw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            debug!("client -> public: {} bytes transferred", n);
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("client -> public: Connection closed: {}", e);
                            } else {
                                error!("Error copying client -> public: {}", e);
                            }
                            break;
                        }
                    },
                    Ok(Err(e)) => {
                        if is_connection_error(&e) {
                            info!("client -> public: Connection closed: {}", e);
                        } else {
                            error!("Error reading from client: {}", e);
                        }
                        break;
                    }
                    Err(_) => {
                        info!(
                            "client -> public: Idle timeout reached after {} seconds",
                            timeout_duration.as_secs()
                        );
                        break;
                    }
                }
            }
            Ok(total_bytes)
        } else {
            // No timeout - use standard copy function
            tokio::io::copy(&mut cr, &mut pw).await
        };

        match result {
            Ok(bytes) => info!("client -> public: {} bytes transferred", bytes),
            Err(e) => {
                if is_connection_error(&e) {
                    info!("client -> public: Connection closed: {}", e);
                } else {
                    error!("Error copying client -> public: {}", e);
                }
            }
        }
    });

    let _ = tokio::try_join!(to_client, to_public);
    info!("Proxy session closed");
}

// Handle a one-time HTTP request that should close after completion
async fn handle_one_time_proxy(public_stream: &mut TcpStream, client_stream: &mut TcpStream) {
    debug!("Handling one-time HTTP exchange");

    // HTTP 응답 파서 초기화
    let mut response_parser = HttpResponseParser::new();

    // Create a buffer for the client response
    let mut response_buffer = vec![0u8; 16384]; // 초기 버퍼 크기 줄임

    // Wait for response from client (with timeout)
    match timeout(
        Duration::from_secs(30),
        client_stream.read(&mut response_buffer),
    )
    .await
    {
        Ok(Ok(n)) => {
            if n > 0 {
                // 실제 읽은 크기로 버퍼 조정
                response_buffer.truncate(n);

                // 응답 파서에 데이터 추가
                response_parser.extend(&response_buffer);

                // Forward response to public connection
                if let Err(e) = public_stream.write_all(&response_buffer).await {
                    error!("Error sending response to public: {}", e);
                    return;
                }

                // 응답 데이터 로깅
                if let Ok(text) = std::str::from_utf8(&response_buffer) {
                    let first_line = text.lines().next().unwrap_or("");
                    info!("Forwarded response to public: {}", first_line);
                }

                // 응답 파싱
                let mut is_keep_alive = false;
                let mut has_chunked_encoding = false;
                let mut content_length = None;
                let mut headers_end = 0;
                let mut is_complete = response_parser.is_complete();

                if let Ok(response) = response_parser.parse() {
                    // 연결 타입 확인
                    is_keep_alive = response.connection_type == ConnectionType::KeepAlive;

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

                    // 헤더 끝 위치 추정
                    if let Some(body) = &response.body {
                        headers_end = response_buffer.len() - body.len();
                    } else if let Ok(text) = std::str::from_utf8(&response_buffer) {
                        if let Some(pos) = text.find("\r\n\r\n") {
                            headers_end = pos + 4;
                        }
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

                // Content-Length를 기반으로 응답 완전성 확인
                let mut need_more = false;

                if let Some(len) = content_length {
                    if headers_end > 0 {
                        let body_received = response_buffer.len() - headers_end;
                        need_more = body_received < len;

                        if need_more {
                            debug!(
                                "Partial HTTP response: {}/{} bytes of body",
                                body_received, len
                            );
                        }
                    }
                } else if has_chunked_encoding {
                    // 청크 인코딩의 경우 마지막 청크(0 크기)가 있는지 확인
                    let has_final_chunk = response_buffer.windows(5).any(|w| w == b"0\r\n\r\n");
                    need_more = !has_final_chunk;

                    if need_more {
                        debug!("Chunked encoding detected but response is incomplete");
                    }
                }

                // If we need more data or response indicates keep-alive, continue reading
                if need_more || is_keep_alive {
                    // 필요하면 버퍼 확장
                    if response_buffer.len() > response_buffer.capacity() / 2 {
                        response_buffer.reserve(response_buffer.capacity());
                    }

                    // Read remaining data in a loop
                    loop {
                        // 버퍼 크기 조정 (필요시 두 배 확장)
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
                            client_stream.read(&mut response_buffer[buf_pos..]),
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
                                    public_stream.write_all(&response_buffer[buf_pos..]).await
                                {
                                    error!(
                                        "Error sending additional response data to public: {}",
                                        e
                                    );
                                    break;
                                }

                                debug!("Forwarded {} more bytes to public", n);

                                // 응답 완전성 다시 확인
                                if let Some(len) = content_length {
                                    // Content-Length 기반 확인
                                    if headers_end > 0 {
                                        let body_received = response_buffer.len() - headers_end;
                                        if body_received >= len {
                                            debug!(
                                                "Received complete HTTP response: {} bytes of body",
                                                body_received
                                            );
                                            break;
                                        }
                                    }
                                } else if has_chunked_encoding {
                                    // 청크 인코딩의 경우 마지막 청크 확인
                                    if response_buffer[buf_pos..]
                                        .windows(5)
                                        .any(|w| w == b"0\r\n\r\n")
                                        || response_buffer.windows(5).any(|w| w == b"0\r\n\r\n")
                                    {
                                        debug!("Received complete chunked response");
                                        break;
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Error reading additional data from client: {}", e);
                                break;
                            }
                            Err(_) => {
                                debug!(
                                    "Timeout reading additional data from client, assuming response complete"
                                );
                                break;
                            }
                        }
                    }
                }

                info!(
                    "One-time HTTP exchange completed, total response size: {} bytes",
                    response_buffer.len()
                );
            } else {
                error!("Client closed connection without sending response");
            }
        }
        Ok(Err(e)) => {
            error!("Error reading response from client: {}", e);
        }
        Err(_) => {
            error!("Timeout waiting for response from client");
        }
    }

    // Close connections explicitly
    info!("Closing one-time HTTP connection");
    let _ = public_stream.shutdown().await;
    let _ = client_stream.shutdown().await;
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

// Handles a client registration and proxies connections
async fn handle_client(
    control_stream: TcpStream,
    _clients: Arc<Mutex<Vec<ClientInfo>>>,
    connection_timeout: u64,
    idle_timeout: u64,
) {
    // Store the peer address before splitting
    let peer_addr: Option<std::net::SocketAddr> = match control_stream.peer_addr() {
        Ok(addr) => Some(addr),
        Err(_) => {
            error!("Failed to get peer address");
            return;
        }
    };

    info!("New client connection from {:?}", peer_addr);

    let (read_half, mut write_half) = tokio::io::split(control_stream);
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    match reader.read_line(&mut line).await {
        Ok(0) => {
            error!("Client disconnected before sending any data");
            return;
        }
        Ok(_) => {
            // Continue processing
        }
        Err(e) => {
            error!("Error reading from client: {}", e);
            return;
        }
    }

    let parts: Vec<&str> = line.trim().split_whitespace().collect();
    if parts.len() != 3 || parts[0] != "REGISTER" {
        error!("Invalid command from client: {}", line.trim());
        let _ = write_half.write_all(b"ERROR: Invalid command\n").await;
        return;
    }

    let backend_port = match parts[1].parse::<u16>() {
        Ok(port) => port,
        Err(_) => {
            error!("Invalid backend port: {}", parts[1]);
            let _ = write_half.write_all(b"ERROR: Invalid backend port\n").await;
            return;
        }
    };

    let public_port = match parts[2].parse::<u16>() {
        Ok(port) => port,
        Err(_) => {
            error!("Invalid public port: {}", parts[2]);
            let _ = write_half.write_all(b"ERROR: Invalid public port\n").await;
            return;
        }
    };

    // Try to bind the public port. If it's in use, return an error to the client.
    let listener = match TcpListener::bind(("0.0.0.0", public_port)).await {
        Ok(l) => l,
        Err(e) => {
            error!("Failed to bind to public port {}: {}", public_port, e);
            let _ = write_half.write_all(b"ERROR: Public port in use\n").await;
            return;
        }
    };
    info!("Listening on public port {}", public_port);

    // Accept connections until the client disconnects
    loop {
        // Create a buffer for reading lines in the select! branch
        let mut disconnect_buf = String::new();
        tokio::select! {
            // Accept new public connections
            accept_result = listener.accept() => {
                match accept_result {
                    Ok((mut public_stream, addr)) => {
                        info!("Accepted new public connection from {:?}", addr);

                        // Wait for the first bytes from the public stream
                        let mut initial_data = vec![0u8; 4096];
                        info!("Waiting for initial data from public connection...");

                        // Set a longer read timeout for initial data to handle slow clients
                        let n = match timeout(Duration::from_secs(connection_timeout * 2), public_stream.read(&mut initial_data)).await {
                            Ok(Ok(n)) => n,
                            Ok(Err(e)) => {
                                error!("Error reading initial data from public: {}", e);
                                continue;
                            }
                            Err(_) => {
                                error!("Timeout waiting for data from public connection, closing");
                                continue;
                            }
                        };

                        // Check if data received
                        if n == 0 {
                            error!("No data received from public connection, closing");
                            continue;
                        }

                        info!("Received {} bytes of initial data from public connection", n);

                        // Truncate buffer to actual data size
                        initial_data.truncate(n);

                        // Check if this is an HTTP request
                        let http_info = parse_http_request(&initial_data);
                        let mut is_http = false;
                        let mut is_keep_alive = false;
                        let mut is_connection_close = false;
                        let mut request_path = String::new();

                        if let Some((keep_alive, connection_close, path)) = http_info {
                            is_http = true;
                            is_keep_alive = keep_alive;
                            is_connection_close = connection_close;
                            request_path = path;
                            info!("Detected HTTP request for path: {}, keep-alive: {}, connection-close: {}",
                                   request_path, is_keep_alive, is_connection_close);
                        }

                        // Debug: print the raw data received from the public connection
                        if n > 0 {
                            let data = &initial_data[..n];
                            match std::str::from_utf8(data) {
                                Ok(s) => {
                                    // Log the first line or up to 100 chars to avoid cluttering logs
                                    let preview = s.lines().next().unwrap_or("").chars().take(100).collect::<String>();
                                    debug!("Received from public: {}{}",
                                           preview,
                                           if preview.len() < s.len() { "..." } else { "" });
                                },
                                Err(_) => debug!("Received from public (hex): {}",
                                                 data.iter().take(50)
                                                     .map(|b| format!("{:02x}", b))
                                                     .collect::<Vec<_>>().join(" ") + "..."),
                            }
                        }

                        // Notify client that the public connection is ready
                        info!("Sending READY to client");
                        if let Err(e) = write_half.write_all(b"READY\n").await {
                            error!("Failed to send READY to client: {}", e);
                            continue;
                        }

                        // Wait for client to connect back for this session
                        info!("Waiting for client to connect back...");

                        // Instead of connecting to the client, we'll accept a connection from the client
                        // We create a channel to receive the new connection
                        let (tx, rx) = tokio::sync::oneshot::channel();

                        // Store connection details to track the handshake
                        let handshake_id = format!("{}-{}", chrono::Utc::now().timestamp_millis(), public_port);

                        {
                            // Store the channel in shared state
                            let mut clients = _clients.lock().await;

                            // First check if we need to clean up any stale handshakes
                            let now = std::time::Instant::now();
                            clients.retain(|c| {
                                let age = now.duration_since(c.last_activity);
                                // Remove handshakes older than 30 seconds
                                !(age.as_secs() > 30 && c.handshake_channel.is_some())
                            });

                            clients.push(ClientInfo {
                                backend_port,
                                public_port,
                                handshake_channel: Some(tx),
                                handshake_id: handshake_id.clone(),
                                last_activity: std::time::Instant::now(),
                                reconnect_attempts: 0,
                            });
                        }

                        // Tell client to connect back with handshake ID
                        let connect_message = format!("CONNECT {}\n", handshake_id);
                        if let Err(e) = write_half.write_all(connect_message.as_bytes()).await {
                            error!("Failed to send CONNECT with handshake ID to client: {}", e);
                            continue;
                        }

                        // Wait for the client to connect back (with timeout)
                        let session_stream = match timeout(Duration::from_secs(connection_timeout), rx).await {
                            Ok(Ok(stream)) => {
                                info!("Client connected back for handshake ID {}", handshake_id);
                                Some(stream)
                            },
                            Ok(Err(_)) => {
                                error!("Channel error while waiting for client callback");
                                None
                            },
                            Err(_) => {
                                error!("Timeout waiting for client to connect back");
                                None
                            }
                        };

                        // Clean up the entry regardless of success/failure
                        {
                            let mut clients = _clients.lock().await;
                            clients.retain(|c| c.handshake_id != handshake_id);
                        }

                        if let Some(client_stream) = session_stream {
                            info!("Spawning proxy handler for public connection");
                            tokio::spawn(async move {
                                handle_public_connection(public_stream, client_stream, initial_data, idle_timeout).await;
                            });
                        } else {
                            error!("Failed to receive client connection, dropping public connection");
                        }
                    }
                    Err(e) => {
                        error!("Failed to accept public connection: {}", e);
                    }
                }
            }
            // Detect client disconnect
            read_result = reader.read_line(&mut disconnect_buf) => {
                if read_result.unwrap_or(0) == 0 {
                    info!("Client disconnected, closing public port {}", public_port);
                    break;
                }
            }
        }
    }
    // When the function returns, the listener is dropped and the port is freed.
}

// Handle WebSocket proxy with bidirectional streaming
async fn handle_websocket_proxy(mut public_stream: TcpStream, mut client_stream: TcpStream) {
    debug!("Starting WebSocket proxy mode");

    // For WebSockets, we need higher timeouts and keeping the connection open
    // WebSockets might idle for extended periods but should remain open

    // Use the split function to get owned halves
    let (mut pr, mut pw) = tokio::io::split(public_stream);
    let (mut cr, mut cw) = tokio::io::split(client_stream);

    // Forward data from public to client without any timeouts
    let to_client = tokio::spawn(async move {
        info!("WebSocket: Transferring data: public -> client");
        let mut buffer = [0u8; 16384];
        let mut total_bytes: u64 = 0;

        loop {
            match pr.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: public side closed the connection");
                    break;
                }
                Ok(n) => {
                    match cw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: public -> client: {} bytes transferred",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: public -> client: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying public -> client: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: public -> client: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from public: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: public -> client: {} total bytes transferred",
            total_bytes
        );
    });

    // Forward data from client to public without any timeouts
    let to_public = tokio::spawn(async move {
        info!("WebSocket: Transferring data: client -> public");
        let mut buffer = [0u8; 16384];
        let mut total_bytes: u64 = 0;

        loop {
            match cr.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: client side closed the connection");
                    break;
                }
                Ok(n) => {
                    match pw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            total_bytes += n as u64;
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: client -> public: {} bytes transferred",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: client -> public: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying client -> public: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: client -> public: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from client: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: client -> public: {} total bytes transferred",
            total_bytes
        );
    });

    let _ = tokio::try_join!(to_client, to_public);
    info!("WebSocket proxy session closed");
}

#[tokio::main]
async fn main() {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Parse command-line arguments
    let args = Args::parse();

    // Shared list of clients
    let clients: Arc<Mutex<Vec<ClientInfo>>> = Arc::new(Mutex::new(Vec::new()));

    // Listen for client registrations on the specified address
    let listener = TcpListener::bind(&args.listen).await.unwrap();
    info!("Server listening on {}", &args.listen);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                // First check if this is a callback connection from an existing client
                let handshake_id = {
                    let mut callback_buf = [0; 1024];
                    let mut callback_handshake = None;

                    // Try to peek at the data without consuming it
                    if let Ok(n) = stream.peek(&mut callback_buf).await {
                        if n > 0 {
                            if let Ok(data) = std::str::from_utf8(&callback_buf[..n]) {
                                if data.starts_with("CALLBACK ") {
                                    if let Some(id) =
                                        data.strip_prefix("CALLBACK ").map(|s| s.trim().to_string())
                                    {
                                        callback_handshake = Some(id);
                                    }
                                }
                            }
                        }
                    }
                    callback_handshake
                };

                if let Some(id) = handshake_id {
                    info!("Received callback connection for handshake ID: {}", id);

                    // Read and discard the CALLBACK line
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    let _ = reader.read_line(&mut line).await;

                    let mut sender = None;
                    {
                        let mut clients_lock = clients.lock().await;
                        for client in clients_lock.iter_mut() {
                            if client.handshake_id == id {
                                sender = client.handshake_channel.take();
                                break;
                            }
                        }
                    }

                    if let Some(tx) = sender {
                        match tx.send(reader.into_inner()) {
                            Ok(_) => info!(
                                "Successfully forwarded callback connection for handshake ID: {}",
                                id
                            ),
                            Err(_) => error!(
                                "Failed to send stream through channel for handshake ID: {}",
                                id
                            ),
                        }
                    } else {
                        error!("No waiting handler found for handshake ID: {}", id);
                    }
                } else {
                    // This is a new client registration
                    info!("New client registration from {}", addr);
                    let clients = clients.clone();
                    let connection_timeout = args.connection_timeout;
                    let idle_timeout = args.idle_timeout;
                    tokio::spawn(async move {
                        handle_client(stream, clients, connection_timeout, idle_timeout).await;
                    });
                }
            }
            Err(e) => {
                error!("Failed to accept client: {}", e);
            }
        }
    }
}
