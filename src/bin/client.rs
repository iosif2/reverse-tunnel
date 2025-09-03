use chrono;
use clap::Parser;
use reverse_tunnel::{
    ConnectionType, HttpRequestParser, HttpResponseParser, ProxyConnectionState,
    is_connection_error,
};
use socket2;
use std::collections::HashMap;
use std::error::Error;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

// Custom error type to work with Send trait
#[derive(Debug)]
struct ProxyError(String);

impl fmt::Display for ProxyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl Error for ProxyError {}

// Helper function to create error
fn proxy_err<T: ToString>(msg: T) -> Box<dyn Error + Send> {
    Box::new(ProxyError(msg.to_string()))
}

// Add a struct to track connection pool information
struct PooledConnection {
    stream: TcpStream,
    last_used: Instant,
    session_id: String,
}

// Connection pool to manage and reuse backend connections
struct ConnectionPool {
    connections: HashMap<u16, Vec<PooledConnection>>,
    max_idle_time: Duration,
    max_pool_size: usize,
}

impl ConnectionPool {
    fn new(max_idle_seconds: u64, max_pool_size: usize) -> Self {
        Self {
            connections: HashMap::new(),
            max_idle_time: Duration::from_secs(max_idle_seconds),
            max_pool_size,
        }
    }

    // Get a connection from the pool or create a new one
    async fn get_connection(
        &mut self,
        port: u16,
        connection_timeout: u64,
    ) -> Option<(TcpStream, String)> {
        // First try to get an existing connection from the pool
        if let Some(conns) = self.connections.get_mut(&port) {
            // Find a connection that might still be valid
            if let Some(idx) = conns.iter().position(|c| {
                // Use a simple heuristic - connections less than 30 seconds old are likely still valid
                // This avoids calling potentially blocking is_readable/is_writable methods that aren't available in tokio
                Instant::now().duration_since(c.last_used) < Duration::from_secs(30)
            }) {
                let conn = conns.remove(idx);
                debug!(
                    "Reusing pooled connection to backend:{} with session_id {}",
                    port, conn.session_id
                );
                return Some((conn.stream, conn.session_id));
            }
        }

        // If no existing connection is available, create a new one
        match timeout(
            Duration::from_secs(connection_timeout),
            TcpStream::connect(("127.0.0.1", port)),
        )
        .await
        {
            Ok(Ok(stream)) => {
                // Set TCP_NODELAY to improve latency
                if let Err(e) = stream.set_nodelay(true) {
                    debug!("Failed to set TCP_NODELAY on new backend stream: {}", e);
                }

                // Generate a new session ID
                let session_id = format!("{}-{}", chrono::Utc::now().timestamp_millis(), port);
                debug!(
                    "Created new connection to backend:{} with session_id {}",
                    port, session_id
                );
                Some((stream, session_id))
            }
            _ => None,
        }
    }

    // Return a connection to the pool for reuse
    async fn return_connection(&mut self, port: u16, stream: TcpStream, session_id: String) {
        // Check if this port already has a pool
        let conns = self.connections.entry(port).or_insert_with(Vec::new);

        // If the pool isn't full, add the connection
        if conns.len() < self.max_pool_size {
            debug!(
                "Returning connection with session_id {} to pool for backend:{}",
                session_id, port
            );
            conns.push(PooledConnection {
                stream,
                last_used: Instant::now(),
                session_id,
            });
        }
        // Otherwise, the connection will be dropped and closed
    }

    // Clean up idle connections
    async fn cleanup(&mut self) {
        let now = Instant::now();

        // For each port in the pool
        let ports_to_check: Vec<u16> = self.connections.keys().cloned().collect();
        for port in ports_to_check {
            if let Some(conns) = self.connections.get_mut(&port) {
                // Remove connections that have been idle for too long
                conns.retain(|conn| {
                    let should_keep = now.duration_since(conn.last_used) < self.max_idle_time;
                    if !should_keep {
                        debug!(
                            "Removing idle connection with session_id {} from pool",
                            conn.session_id
                        );
                    }
                    should_keep
                });

                // If the pool for this port is empty, remove it
                if conns.is_empty() {
                    self.connections.remove(&port);
                }
            }
        }
    }
}

/// Reverse-TCP-Proxy 명령줄 인자
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]

/// 명령어 인자 정의 구조체
struct Args {
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

    /// 데이터 로깅 레벨
    #[arg(long = "data-logging", short = 'd', default_value = "off")]
    data_logging: String,

    /// 연결 풀 최대 크기
    #[arg(long = "max-pool-size", default_value = "10")]
    max_pool_size: usize,

    /// 연결 풀 최대 유휴 시간 (초)
    #[arg(long = "pool-idle-timeout", default_value = "300")]
    pool_idle_timeout: u64,

    /// 연결 풀 활성화 여부
    #[arg(long = "use-connection-pool", default_value = "false")]
    use_connection_pool: bool,
}

/// 서버로 부터 받은 데이터를 백엔드로 릴레이 하는 함수
async fn handle_backend_connection(
    mut server_stream: TcpStream,
    backend_port: u16,
    connection_timeout: u64,
    idle_timeout: u64,
    pool: Arc<Mutex<ConnectionPool>>,
    data_logging: String,
    active_sessions: Option<Arc<Mutex<std::collections::HashSet<String>>>>,
) -> Result<(), Box<dyn std::error::Error + Send>> {
    // 연결 상태
    let mut connection_state = ProxyConnectionState::Initial;

    // Try to get a connection from the pool
    let (mut backend_stream, connection_id) = {
        let mut pool = pool.lock().await;
        if let Some(conn) = pool.get_connection(backend_port, connection_timeout).await {
            conn
        } else {
            error!("Failed to connect to backend:{}", backend_port);
            return Err(proxy_err("Failed to connect to backend"));
        }
    };

    info!(
        "Using connection {} to backend:{}",
        connection_id, backend_port
    );

    // Track if this connection is HTTP and keep-alive for potential reuse
    let mut is_http = false;
    let mut is_keep_alive = true;
    let mut is_connection_close = false;

    // Configure socket options for better performance and error detection
    if let Err(e) = server_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on server stream: {}", e);
    }

    // Set SO_KEEPALIVE if supported by platform
    let socket_ref = socket2::SockRef::from(&server_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on server stream: {}", e);
    }

    // Configure socket buffer sizes - 소켓 버퍼 크기 증가
    let socket_ref = socket2::SockRef::from(&server_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(1048576) {
        // 1MB
        debug!("Failed to set receive buffer size on server stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(1048576) {
        // 1MB
        debug!("Failed to set send buffer size on server stream: {}", e);
    }

    // Connection pool may have already set these, but let's ensure they're set
    let socket_ref = socket2::SockRef::from(&backend_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(1048576) {
        // 1MB
        debug!("Failed to set receive buffer size on backend stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(1048576) {
        // 1MB
        debug!("Failed to set send buffer size on backend stream: {}", e);
    }

    // 소켓 연결 상태 확인 로깅
    if let Ok(info) = server_stream.peer_addr() {
        debug!("Server stream connected to {}", info);
    }
    if let Ok(info) = backend_stream.peer_addr() {
        debug!("Backend stream connected to {}", info);
    }

    // Read any initial data from the server and forward it to the backend
    let mut initial_data = vec![0u8; 8192]; // Larger buffer for HTTP requests
    let n = match server_stream.read(&mut initial_data).await {
        Ok(n) => n,
        Err(e) => {
            error!("Error reading initial data from server: {}", e);
            return Err(proxy_err(e.to_string()));
        }
    };

    if n > 0 {
        initial_data.truncate(n);

        let mut request_parser = HttpRequestParser::new();
        request_parser.extend(&initial_data);

        let mut is_websocket_req = false;

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

        if !is_websocket_req && request_parser.is_websocket_upgrade() {
            is_websocket_req = true;
            connection_state = ProxyConnectionState::WebSocket;
            info!("Backup detection: WebSocket upgrade request detected");
        }

        match std::str::from_utf8(&initial_data) {
            Ok(s) => {
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

        if let Err(e) = backend_stream.write_all(&initial_data).await {
            error!("Error sending initial data to backend: {}", e);
            return Err(proxy_err(e.to_string()));
        }

        if is_websocket_req {
            info!("WebSocket upgrade request detected, waiting for backend response");

            let mut response_buffer = vec![0u8; 8192];
            match timeout(
                Duration::from_secs(connection_timeout),
                backend_stream.read(&mut response_buffer),
            )
            .await
            {
                Ok(Ok(n)) if n > 0 => {
                    response_buffer.truncate(n);

                    if let Err(e) = server_stream.write_all(&response_buffer).await {
                        error!("Error forwarding WebSocket response to server: {}", e);
                        return Err(proxy_err(e.to_string()));
                    }

                    let mut response_parser = HttpResponseParser::new();
                    response_parser.extend(&response_buffer);

                    let is_valid_websocket = if response_parser.is_complete() {
                        if let Ok(response) = response_parser.parse() {
                            response.is_websocket_upgrade()
                        } else {
                            false
                        }
                    } else {
                        response_parser.is_websocket_upgrade()
                    };

                    if is_valid_websocket {
                        info!("Backend confirmed WebSocket upgrade, switching to WebSocket mode");
                        connection_state = ProxyConnectionState::WebSocket;
                        handle_websocket_connection(server_stream, backend_stream).await?;
                        return Ok(());
                    } else {
                        info!(
                            "Backend response was not a valid WebSocket upgrade, continuing with normal proxy"
                        );
                    }
                }
                _ => {
                    error!("Failed to get backend response for WebSocket upgrade");
                    return Err(proxy_err(
                        "Failed to get backend response for WebSocket upgrade",
                    ));
                }
            }
        }

        if is_http && is_connection_close {
            info!(
                "HTTP request with Connection: close detected, will close connection after response"
            );

            handle_one_time_http_request(server_stream, backend_stream).await?;
            return Ok(());
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

    // Create channels for signaling completion
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel::<(bool, bool)>();
    let completion_tx = std::sync::Arc::new(std::sync::Mutex::new(Some(completion_tx)));

    // Clone for each task
    let completion_tx_client = completion_tx.clone();
    let completion_tx_server = completion_tx.clone();

    // Transfer statistics tracking
    let total_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let total_received = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let total_sent_clone = total_sent.clone();
    let total_received_clone = total_received.clone();

    // Setup connection state tracking
    let connection_id_clone = connection_id.clone();
    let pool_clone = pool.clone();
    let backend_port_clone = backend_port;

    // Store connection type flags for later use in reuse decision
    let is_http_clone = is_http;
    let is_keep_alive_clone = is_keep_alive;

    // Track error states to know if we can return to pool
    let server_error = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let backend_error = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let server_error_clone = server_error.clone();
    let backend_error_clone = backend_error.clone();

    // Clone them again for our main task
    let server_error_main = server_error.clone();
    let backend_error_main = backend_error.clone();

    // Forward data from server to backend with optional timeout
    let to_backend = tokio::spawn(async move {
        info!("Transferring data: server -> backend");

        let result = if let Some(timeout_duration) = idle_timeout_duration {
            // With timeout - will abort if no data transfer happens within the timeout
            let mut buffer = [0u8; 32768]; // 버퍼 크기 16384에서 32768로 증가
            let mut total_bytes: u64 = 0;

            loop {
                match timeout(timeout_duration, sr.read(&mut buffer)).await {
                    Ok(Ok(0)) => {
                        debug!("server -> backend: EOF received");
                        break;
                    }
                    Ok(Ok(n)) => match bw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            // 데이터 즉시 플러시 추가
                            if let Err(e) = bw.flush().await {
                                error!("Error flushing data to backend: {}", e);
                                server_error.store(true, std::sync::atomic::Ordering::SeqCst);
                                break;
                            }

                            total_bytes += n as u64;
                            total_sent_clone
                                .fetch_add(n as u64, std::sync::atomic::Ordering::SeqCst);
                            debug!("server -> backend: {} bytes transferred", n);
                            if total_bytes > 100_000_000 {
                                // 100MB 이상 전송 시 경고
                                warn!(
                                    "Large data transfer: server -> backend: {} bytes total",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("server -> backend: Connection closed: {}", e);
                            } else {
                                error!("Error copying server -> backend: {}", e);
                            }
                            backend_error.store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                    },
                    Ok(Err(e)) => {
                        if is_connection_error(&e) {
                            info!("server -> backend: Connection closed: {}", e);
                        } else {
                            error!("Error reading from server: {}", e);
                        }
                        server_error.store(true, std::sync::atomic::Ordering::SeqCst);
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
            // No timeout - use standard copy function with debug
            let result = tokio::io::copy(&mut sr, &mut bw).await;
            if let Ok(bytes) = result {
                if bytes > 100_000_000 {
                    // 100MB 이상 전송 시 경고
                    warn!(
                        "Large data transfer without timeout: server -> backend: {} bytes",
                        bytes
                    );
                }
            }
            result
        };

        match result {
            Ok(bytes) => info!("server -> backend: {} bytes transferred", bytes),
            Err(e) => {
                if is_connection_error(&e) {
                    info!("server -> backend: Connection closed: {}", e);
                } else {
                    error!("Error copying server -> backend: {}", e);
                    server_error.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }

        // Notify main task that this direction is done
        if let Some(tx) = completion_tx_server.lock().unwrap().take() {
            let _ = tx.send((true, false)); // server_done = true
        }
    });

    // Forward data from backend to server with optional timeout
    let to_server = tokio::spawn(async move {
        info!("Transferring data: backend -> server");

        let result = if let Some(timeout_duration) = idle_timeout_duration {
            // With timeout - will abort if no data transfer happens within the timeout
            let mut buffer = [0u8; 32768]; // 버퍼 크기 32KB로 증가
            let mut total_bytes: u64 = 0;

            loop {
                match timeout(timeout_duration, br.read(&mut buffer)).await {
                    Ok(read_result) => match read_result {
                        Ok(0) => {
                            debug!("backend -> server: EOF received");
                            break;
                        }
                        Ok(n) => match sw.write_all(&buffer[..n]).await {
                            Ok(_) => {
                                // 데이터 즉시 플러시 추가
                                if let Err(e) = sw.flush().await {
                                    error!("Error flushing data to server: {}", e);
                                    backend_error_clone
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                    break;
                                }

                                total_bytes += n as u64;
                                total_received_clone
                                    .fetch_add(n as u64, std::sync::atomic::Ordering::SeqCst);
                                debug!("backend -> server: {} bytes transferred", n);

                                // 첫 100바이트 데이터 로깅 (트러블슈팅용)
                                if n > 10 && total_bytes < 10000 {
                                    info!(
                                        "Data sample (backend -> server): {:?}",
                                        &buffer[..std::cmp::min(n, 100)]
                                    );
                                }

                                if total_bytes > 100_000_000 {
                                    warn!(
                                        "Large data transfer: backend -> server: {} bytes total",
                                        total_bytes
                                    );
                                }

                                // 대용량 패킷 전송 시 일시적 스레드 양보
                                if n > 100000 {
                                    tokio::task::yield_now().await;
                                }
                            }
                            Err(e) => {
                                if is_connection_error(&e) {
                                    info!("backend -> server: Connection closed: {}", e);
                                } else {
                                    error!("Error copying backend -> server: {}", e);
                                }
                                server_error_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                                break;
                            }
                        },
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("backend -> server: Connection closed: {}", e);
                            } else {
                                error!("Error reading from backend: {}", e);
                            }
                            backend_error_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                            break;
                        }
                    },
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
            // No timeout - use standard copy with logging
            let mut buffer = [0u8; 32768];
            let mut total_bytes: u64 = 0;

            loop {
                match br.read(&mut buffer).await {
                    Ok(0) => break, // EOF
                    Ok(n) => {
                        match sw.write_all(&buffer[..n]).await {
                            Ok(_) => {
                                // 플러시 명시적 호출
                                if let Err(e) = sw.flush().await {
                                    error!("Error flushing data to server: {}", e);
                                    server_error_clone
                                        .store(true, std::sync::atomic::Ordering::SeqCst);
                                    break;
                                }

                                total_bytes += n as u64;
                                total_received_clone
                                    .fetch_add(n as u64, std::sync::atomic::Ordering::SeqCst);
                                debug!("backend -> server: {} bytes transferred", n);

                                // 첫 100바이트 데이터 로깅 (트러블슈팅용)
                                if n > 10 && total_bytes < 10000 {
                                    info!(
                                        "Data sample (backend -> server): {:?}",
                                        &buffer[..std::cmp::min(n, 100)]
                                    );
                                }

                                if total_bytes > 100_000_000 {
                                    warn!(
                                        "Large data transfer: backend -> server: {} bytes total",
                                        total_bytes
                                    );
                                }

                                // 대용량 패킷 전송 시 일시적 스레드 양보
                                if n > 100000 {
                                    tokio::task::yield_now().await;
                                }
                            }
                            Err(e) => {
                                if is_connection_error(&e) {
                                    info!("backend -> server: Connection closed: {}", e);
                                } else {
                                    error!("Error copying backend -> server: {}", e);
                                }
                                server_error_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                                break;
                            }
                        }
                    }
                    Err(e) => {
                        if is_connection_error(&e) {
                            info!("backend -> server: Connection closed: {}", e);
                        } else {
                            error!("Error reading from backend: {}", e);
                        }
                        backend_error_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                        break;
                    }
                }
            }

            Ok(total_bytes)
        };

        match result {
            Ok(bytes) => info!("backend -> server: {} bytes transferred", bytes),
            Err(e) => {
                if is_connection_error(&e) {
                    info!("backend -> server: Connection closed: {}", e);
                } else {
                    error!("Error copying backend -> server: {}", e);
                    backend_error_clone.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }

        // Notify main task that this direction is done
        if let Some(tx) = completion_tx_client.lock().unwrap().take() {
            let _ = tx.send((false, true)); // client_done = true
        }
    });

    // Wait for completion from either or both directions
    // This helps us determine when it's safe to return the stream to the pool
    let (server_done, client_done) = match completion_rx.await {
        Ok((s, c)) => (s, c),
        Err(_) => {
            // Channel error, assume both sides done
            error!("Channel error while waiting for completion");
            (true, true)
        }
    };

    // Ensure both tasks complete before we try to reassemble the stream
    let _ = to_backend.await;
    let _ = to_server.await;

    // 전송 통계 기록
    let total_sent_final = total_sent.load(std::sync::atomic::Ordering::SeqCst);
    let total_received_final = total_received.load(std::sync::atomic::Ordering::SeqCst);

    info!(
        "Proxy session completed: sent {} bytes, received {} bytes",
        total_sent_final, total_received_final
    );

    // Now attempt to reassemble the backend stream for reuse
    // This is a tricky part - we need to reconnect the two halves
    let server_error_val = server_error_main.load(std::sync::atomic::Ordering::SeqCst);
    let backend_error_val = backend_error_main.load(std::sync::atomic::Ordering::SeqCst);

    // Determine if the connection can be reused
    let can_reuse = is_http_clone && is_keep_alive_clone && !server_error_val && !backend_error_val;

    if can_reuse {
        // Try to manually recreate the stream using AsyncRead + AsyncWrite traits
        // Unfortunately, in tokio there's no direct split_reunite API, so we need an alternative approach
        // Option 1: Create a new connection to replace the split one
        match TcpStream::connect(("127.0.0.1", backend_port_clone)).await {
            Ok(fresh_stream) => {
                info!(
                    "Created fresh connection to replace {} for backend:{}",
                    connection_id_clone, backend_port_clone
                );
                let mut pool = pool_clone.lock().await;
                // Generate a new session ID for the fresh connection
                let new_session_id = format!(
                    "{}-{}",
                    chrono::Utc::now().timestamp_millis(),
                    backend_port_clone
                );
                pool.return_connection(backend_port_clone, fresh_stream, new_session_id)
                    .await;
            }
            Err(e) => {
                info!(
                    "Could not create replacement connection for {}: {}",
                    connection_id_clone, e
                );
            }
        }
    } else {
        if server_error_val || backend_error_val {
            info!(
                "Not returning connection {} to pool due to errors",
                connection_id_clone
            );
        } else if !is_keep_alive_clone {
            info!(
                "Not returning connection {} to pool as it's not keep-alive",
                connection_id_clone
            );
        }
    }

    info!("Proxy session streams closed by task completion");

    Ok(())
}

async fn handle_one_time_http_request(
    mut server_stream: TcpStream,
    mut backend_stream: TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send>> {
    debug!("Handling one-time HTTP request with Connection: close");

    // HTTP 응답 파서 초기화
    let mut response_parser = HttpResponseParser::new();

    // Create a buffer for the backend response
    let mut response_buffer = vec![0u8; 32768]; // 초기 버퍼 크기 32KB로 증가

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
                    return Ok(());
                }

                // 명시적 플러시 추가
                if let Err(e) = server_stream.flush().await {
                    error!("Error flushing response to server: {}", e);
                    return Ok(());
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

                                // 명시적 플러시 추가
                                if let Err(e) = server_stream.flush().await {
                                    error!(
                                        "Error flushing additional response data to server: {}",
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

    Ok(())
}

// Handle WebSocket connection with special handling
async fn handle_websocket_connection(
    server_stream: TcpStream,
    backend_stream: TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send>> {
    debug!("Starting WebSocket proxy mode between server and backend");

    // Use the split function to get owned halves for async tasks
    let (sr, sw) = tokio::io::split(server_stream);
    let (br, bw) = tokio::io::split(backend_stream);

    // sr, sw, br, bw를 가변 참조로 변환
    let mut sr = sr;
    let mut sw = sw;
    let mut br = br;
    let mut bw = bw;

    // Forward data from server to backend without timeout (WebSockets can idle)
    let to_backend = tokio::spawn(async move {
        info!("WebSocket: Transferring data: server -> backend");
        let mut buffer = [0u8; 32768]; // 버퍼 크기 증가
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
                            // 데이터를 즉시 전송하기 위해 명시적으로 플러시 추가
                            if let Err(e) = bw.flush().await {
                                error!("WebSocket: Error flushing data to backend: {}", e);
                                break;
                            }

                            total_bytes += n as u64;
                            debug!("WebSocket: server -> backend: {} bytes transferred", n);
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
        let mut buffer = [0u8; 32768]; // 버퍼 크기 증가
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
                            // 데이터를 즉시 전송하기 위해 명시적으로 플러시 추가
                            if let Err(e) = sw.flush().await {
                                error!("WebSocket: Error flushing data to server: {}", e);
                                break;
                            }

                            total_bytes += n as u64;
                            debug!("WebSocket: backend -> server: {} bytes transferred", n);
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: backend -> server: {} bytes transferred",
                                    total_bytes
                                );
                            }

                            // 대용량 데이터 전송 시 경고 로그 추가
                            if total_bytes > 100000 && total_bytes % 100000 < 10240 {
                                warn!(
                                    "WebSocket: Large data transfer: backend -> server: {} bytes total",
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

    match tokio::try_join!(to_backend, to_server) {
        Ok(_) => info!("WebSocket proxy session closed normally"),
        Err(e) => warn!("WebSocket proxy session closed with error: {:?}", e),
    }

    Ok(())
}

#[tokio::main]
async fn main() {
    // Setup tracing subscriber for logs
    tracing_subscriber::fmt::init();

    // Parse command line arguments
    let args = Args::parse();

    // Log startup information
    info!("Starting reverse TCP proxy client");
    info!(
        "Server: {}, Backend Port: {}, Public Port: {}",
        args.server, args.backend_port, args.public_port
    );
    info!(
        "Timeouts - Connection: {}s, Idle: {}s",
        args.connection_timeout, args.idle_timeout
    );

    // Log connection pool settings
    info!(
        "Connection Pool - Max Size: {}, Idle Timeout: {}s",
        args.max_pool_size, args.pool_idle_timeout
    );

    // Log data logging level
    info!("Data logging level: {}", args.data_logging);

    // Setup signal handler for graceful shutdown
    let running = Arc::new(AtomicBool::new(true));
    let r = running.clone();

    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut sigint = signal(SignalKind::interrupt()).unwrap();
        let mut sigterm = signal(SignalKind::terminate()).unwrap();

        tokio::spawn(async move {
            tokio::select! {
                _ = sigint.recv() => {
                    info!("Received SIGINT, shutting down gracefully...");
                    r.store(false, Ordering::SeqCst);
                }
                _ = sigterm.recv() => {
                    info!("Received SIGTERM, shutting down gracefully...");
                    r.store(false, Ordering::SeqCst);
                }
            }
        });
    }

    // Main reconnection loop with backoff strategy
    let mut reconnect_count = 0;
    let mut backoff_time = args.reconnect_interval;

    // Periodic system health check
    let running_clone = running.clone();
    tokio::spawn(async move {
        loop {
            if !running_clone.load(Ordering::SeqCst) {
                break;
            }
            // Run every 30 seconds
            tokio::time::sleep(Duration::from_secs(30)).await;
            // Log status
            info!("Performing periodic connection health check");

            // Check system status on Linux
            #[cfg(target_os = "linux")]
            {
                use std::process::Command;
                if let Ok(output) = Command::new("sh")
                    .arg("-c")
                    .arg("cat /proc/self/status | grep -E 'VmRSS|FDSize'")
                    .output()
                {
                    if let Ok(status) = String::from_utf8(output.stdout) {
                        info!("System status: {}", status.trim().replace("\n", ", "));
                    }
                }
            }
        }
    });

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
        match connect_and_serve(&args, running.clone()).await {
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
async fn connect_and_serve(
    args: &Args,
    running: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send>> {
    info!(
        "Connecting to server {} for backend port {} (public port {})",
        args.server, args.backend_port, args.public_port
    );

    // Create connection pool if enabled
    let pool = Arc::new(Mutex::new(ConnectionPool::new(
        args.pool_idle_timeout,
        args.max_pool_size,
    )));

    // Start connection pool cleanup task if needed
    if args.use_connection_pool {
        info!(
            "Using connection pool with max size: {}",
            args.max_pool_size
        );
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                pool_clone.lock().await.cleanup().await;
            }
        });
    } else {
        info!("Connection pooling disabled");
    }

    // Connect to the server's control port using the provided address
    let connect_result = timeout(
        Duration::from_secs(args.connection_timeout),
        TcpStream::connect(&args.server),
    )
    .await;

    let mut control_stream = match connect_result {
        Ok(Ok(stream)) => stream,
        Ok(Err(e)) => return Err(proxy_err(format!("Failed to connect to server: {}", e))),
        Err(_) => return Err(proxy_err("Connection timeout while connecting to server")),
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
    if let Err(e) = control_stream.write_all(register_msg.as_bytes()).await {
        return Err(proxy_err(format!(
            "Failed to send registration message: {}",
            e
        )));
    }

    // 등록 메시지 플러시 강제화
    if let Err(e) = control_stream.flush().await {
        return Err(proxy_err(format!(
            "Failed to flush registration message: {}",
            e
        )));
    }

    info!(
        "Registered backend:{} as public:{}",
        args.backend_port, args.public_port
    );

    // 서버가 등록을 처리할 수 있도록 잠시 지연
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut reader = BufReader::new(&mut control_stream);

    // Track active sessions to avoid race conditions during quick refreshes
    let active_sessions_arc = Arc::new(tokio::sync::Mutex::new(std::collections::HashSet::new()));
    let active_sessions_timer = active_sessions_arc.clone();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;

            let mut sessions = active_sessions_timer.lock().await;
            let old_count = sessions.len();
            if old_count > 10 {
                info!("Periodic cleanup: Checking {} active sessions", old_count);
                // 오래된 세션 정리 - 실제 구현에서는 세션 생성 시간 기록이 필요
                sessions.clear();
                info!("Periodic cleanup: Cleared {} stale sessions", old_count);
            }
        }
    });

    let active_sessions = active_sessions_arc.clone();

    // Connection pool management to reduce connection setup time
    let mut last_connection_time = std::time::Instant::now();

    loop {
        let mut line = String::new();
        // Wait for the server to notify about a new connection, with timeout to check shutdown
        match timeout(Duration::from_secs(1), reader.read_line(&mut line)).await {
            Ok(Ok(0)) => {
                return Err(proxy_err("Server closed connection"));
            }
            Ok(Ok(_)) => {
                // Process the line
                let trimmed = line.trim();
                debug!("Received from server: '{}'", trimmed);

                if trimmed == "READY" {
                    info!("Received READY signal from server");

                    // Wait for CONNECT after READY
                    let mut connect_line = String::new();
                    match timeout(Duration::from_secs(1), reader.read_line(&mut connect_line)).await
                    {
                        Ok(Ok(0)) => {
                            return Err(proxy_err(
                                "Server closed connection while waiting for CONNECT",
                            ));
                        }
                        Ok(Ok(_)) => {
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
                                    let session_count = {
                                        let sessions = active_sessions.lock().await;
                                        sessions.len()
                                    };

                                    if session_count >= 100 {
                                        warn!(
                                            "Too many active sessions ({}), cleaning up old ones",
                                            session_count
                                        );
                                        {
                                            let mut sessions = active_sessions.lock().await;
                                            sessions.clear();
                                        }
                                    }

                                    // Add this session to active set
                                    {
                                        let mut sessions = active_sessions.lock().await;
                                        sessions.insert(handshake_id.to_string());
                                    }

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

                                            // 콜백 데이터를 서버에 플러시 추가
                                            if let Err(e) = server_stream.flush().await {
                                                error!(
                                                    "Failed to flush handshake ID to server: {}",
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

                                            // Clone the pool for use in spawned task
                                            let pool_for_task = pool.clone();
                                            let data_logging_clone = args.data_logging.clone();
                                            let use_connection_pool = args.use_connection_pool;

                                            tokio::spawn(async move {
                                                info!(
                                                    "Session {}: Proxy session established with backend:{}",
                                                    session_id, backend_port
                                                );

                                                if use_connection_pool {
                                                    // Use connection pool for backend connections
                                                    if let Err(e) = handle_backend_connection(
                                                        server_stream,
                                                        backend_port,
                                                        connection_timeout,
                                                        idle_timeout,
                                                        pool_for_task,
                                                        data_logging_clone,
                                                        Some(active_sessions_clone.clone()),
                                                    )
                                                    .await
                                                    {
                                                        error!("Error in proxy session: {}", e);
                                                    }
                                                } else {
                                                    // Legacy mode: Direct connection without pool
                                                    match timeout(
                                                        Duration::from_secs(connection_timeout),
                                                        TcpStream::connect((
                                                            "127.0.0.1",
                                                            backend_port,
                                                        )),
                                                    )
                                                    .await
                                                    {
                                                        Ok(Ok(mut backend_stream)) => {
                                                            // Configure socket options
                                                            if let Err(e) =
                                                                backend_stream.set_nodelay(true)
                                                            {
                                                                debug!(
                                                                    "Failed to set TCP_NODELAY on backend stream: {}",
                                                                    e
                                                                );
                                                            }

                                                            let socket_ref = socket2::SockRef::from(
                                                                &backend_stream,
                                                            );
                                                            if let Err(e) =
                                                                socket_ref.set_keepalive(true)
                                                            {
                                                                debug!(
                                                                    "Failed to set SO_KEEPALIVE on backend stream: {}",
                                                                    e
                                                                );
                                                            }

                                                            if let Err(e) = socket_ref
                                                                .set_recv_buffer_size(1048576)
                                                            {
                                                                // 1MB
                                                                debug!(
                                                                    "Failed to set receive buffer size on backend stream: {}",
                                                                    e
                                                                );
                                                            }
                                                            if let Err(e) = socket_ref
                                                                .set_send_buffer_size(1048576)
                                                            {
                                                                // 1MB
                                                                debug!(
                                                                    "Failed to set send buffer size on backend stream: {}",
                                                                    e
                                                                );
                                                            }

                                                            // Generate legacy-style connection ID
                                                            let connection_id = format!(
                                                                "{}-{}",
                                                                chrono::Utc::now()
                                                                    .timestamp_millis(),
                                                                backend_port
                                                            );

                                                            info!(
                                                                "Starting connection {} to backend:{}",
                                                                connection_id, backend_port
                                                            );

                                                            // Process session directly
                                                            handle_legacy_connection(
                                                                server_stream,
                                                                backend_stream,
                                                                connection_id,
                                                                data_logging_clone,
                                                                idle_timeout,
                                                            )
                                                            .await;
                                                        }
                                                        Ok(Err(e)) => {
                                                            error!(
                                                                "Failed to connect to backend: {}",
                                                                e
                                                            );
                                                        }
                                                        Err(_) => {
                                                            error!(
                                                                "Connection timeout while connecting to backend:{}",
                                                                backend_port
                                                            );
                                                        }
                                                    }
                                                }

                                                // Remove from active sessions when done
                                                {
                                                    let mut sessions =
                                                        active_sessions_clone.lock().await;
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
                                }
                            } else if connect_trimmed.starts_with("ERROR:") {
                                error!("Received error from server: {}", connect_trimmed);
                            } else {
                                error!("Unexpected message from server: '{}'", connect_trimmed);
                            }
                        }
                        Ok(Err(e)) => {
                            return Err(proxy_err(format!("Error reading from server: {}", e)));
                        }
                        Err(_) => {
                            // Timeout occurred, check for shutdown signal
                            if !running.load(Ordering::SeqCst) {
                                info!("Shutdown signal detected during read, exiting loop");
                                return Ok(());
                            }
                            continue;
                        }
                    }
                }
            }
            Ok(Err(e)) => {
                return Err(proxy_err(format!("Error reading from server: {}", e)));
            }
            Err(_) => {
                // Timeout occurred, check for shutdown signal
                if !running.load(Ordering::SeqCst) {
                    info!("Shutdown signal detected during read, exiting loop");
                    return Ok(());
                }
                continue;
            }
        }
    }
}

// Add a helper function to handle legacy connections:
async fn handle_legacy_connection(
    mut server_stream: TcpStream,
    mut backend_stream: TcpStream,
    connection_id: String,
    data_logging: String,
    idle_timeout: u64,
) {
    // Read any initial data from the server and forward it to the backend
    let mut initial_data = vec![0u8; 32 * 1024]; // 32KB buffer
    let n = match server_stream.read(&mut initial_data).await {
        Ok(n) => n,
        Err(e) => {
            error!("Error reading initial data from server: {}", e);
            return;
        }
    };

    // Process initial data if we received any
    if n > 0 {
        // Truncate to actual received data
        initial_data.truncate(n);

        // Log initial data based on logging level
        match data_logging.as_str() {
            "minimal" => {
                debug!("Server → Backend: {} bytes", n);
            }
            "verbose" => {
                debug!(
                    "Server → Backend [{}]: {} bytes\n{:?}",
                    connection_id,
                    n,
                    String::from_utf8_lossy(&initial_data[..std::cmp::min(n, 100)])
                );
            }
            _ => {}
        }

        // Forward initial data to backend
        if let Err(e) = backend_stream.write_all(&initial_data).await {
            error!("Error sending initial data to backend: {}", e);
            return;
        }

        // Flush the write
        if let Err(e) = backend_stream.flush().await {
            error!("Error flushing initial data to backend: {}", e);
            return;
        }
    }

    // Parse the HTTP request to determine connection type
    let mut connection_state = ProxyConnectionState::Initial;
    let mut is_http = false;
    let mut is_websocket = false;
    let mut is_keep_alive = true;
    let mut is_connection_close = false;

    // Check if it's an HTTP request and determine connection type
    let mut request_parser = HttpRequestParser::new();
    request_parser.extend(&initial_data);

    if request_parser.is_complete() {
        if let Ok(request) = request_parser.parse() {
            is_http = true;

            match request.connection_type {
                ConnectionType::KeepAlive => {
                    is_keep_alive = true;
                    is_connection_close = false;
                    connection_state = ProxyConnectionState::Active;
                    info!(
                        "Detected HTTP keep-alive request for path: {}",
                        request.path
                    );
                }
                ConnectionType::Close => {
                    is_keep_alive = false;
                    is_connection_close = true;
                    connection_state = ProxyConnectionState::FirstExchange;
                    info!(
                        "Detected HTTP connection: close request for path: {}",
                        request.path
                    );
                }
                ConnectionType::WebSocketUpgrade => {
                    is_websocket = true;
                    connection_state = ProxyConnectionState::WebSocket;
                    info!(
                        "Detected WebSocket upgrade request for path: {}",
                        request.path
                    );
                }
            }

            info!(
                "Detected HTTP request for path: {}, connection-type: {:?}",
                request.path, request.connection_type
            );
        }
    }

    // Handle based on connection type
    if is_websocket {
        info!("Handling WebSocket upgrade request");
        if let Err(e) = handle_websocket_connection(server_stream, backend_stream).await {
            error!("WebSocket handling error: {}", e);
        }
    } else if is_connection_close {
        info!("Handling one-time HTTP request");
        if let Err(e) = handle_one_time_http_request(server_stream, backend_stream).await {
            error!("One-time HTTP request handling error: {}", e);
        }
    } else {
        // Regular bidirectional proxy
        // Set up buffer sizes for the streams
        const BUFFER_SIZE: usize = 32 * 1024; // 32KB buffer

        let (mut server_read, mut server_write) = tokio::io::split(server_stream);
        let (mut backend_read, mut backend_write) = tokio::io::split(backend_stream);

        // Start a timer for idle timeout if applicable
        let start_time = Instant::now();
        let idle_duration = if idle_timeout > 0 {
            Some(Duration::from_secs(idle_timeout))
        } else {
            None
        };

        // Create tasks for bidirectional data transfer
        let mut server_to_backend_buffer = vec![0u8; BUFFER_SIZE];
        let mut backend_to_server_buffer = vec![0u8; BUFFER_SIZE];

        // For HTTP keep-alive connections, we need to monitor for complete requests/responses
        let is_keep_alive_http = is_http && is_keep_alive;
        let http_state = Arc::new(Mutex::new(ProxyConnectionState::Active));
        let http_state_clone = http_state.clone();

        // Create a channel for communication between tasks
        let (stop_tx, mut stop_rx) = mpsc::channel::<()>(1);
        let stop_tx_clone = stop_tx.clone();

        let data_logging_clone = data_logging.clone();
        let connection_id_for_client = connection_id.clone();
        let connection_id_for_summary = connection_id.clone();

        // Track transfer statistics in shared state that can be accessed by both tasks
        let total_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let total_received = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let total_sent_clone = total_sent.clone();
        let total_received_clone = total_received.clone();

        // Server to backend task
        let server_to_backend = tokio::spawn(async move {
            let mut last_log_time = Instant::now();
            let mut bytes_since_last_log = 0u64;

            // HTTP request state tracking
            let mut request_data = Vec::new(); // Buffer to collect partial HTTP requests
            let mut awaiting_request = is_keep_alive_http; // Start waiting for request for keep-alive HTTP

            loop {
                match server_read.read(&mut server_to_backend_buffer).await {
                    Ok(0) => {
                        debug!("Server closed connection");
                        break;
                    }
                    Ok(n) => {
                        // Process HTTP data if this is a keep-alive HTTP connection
                        if is_keep_alive_http {
                            // Append to our HTTP buffer
                            request_data.extend_from_slice(&server_to_backend_buffer[..n]);

                            // Try to parse as an HTTP request
                            let mut parser = HttpRequestParser::new();
                            parser.extend(&request_data);

                            if parser.is_complete() {
                                // We have a complete HTTP request
                                debug!("Complete HTTP request detected");
                                awaiting_request = false;

                                if let Ok(request) = parser.parse() {
                                    debug!(
                                        "HTTP request parsed: {} {}",
                                        request.method, request.path
                                    );

                                    // Update connection state based on this request
                                    if request.connection_type == ConnectionType::Close {
                                        let mut state = http_state.lock().await;
                                        *state = ProxyConnectionState::FirstExchange;
                                        debug!(
                                            "Connection: close detected, will terminate after response"
                                        );
                                    }
                                }

                                // Clear buffer since we've handled this request
                                request_data.clear();
                            }
                        }

                        // Update atomic counter instead of local variable
                        total_sent_clone.fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        bytes_since_last_log += n as u64;

                        // Log data based on logging level
                        match data_logging_clone.as_str() {
                            "minimal" => {
                                debug!("Server → Backend: {} bytes", n);
                            }
                            "verbose" => {
                                debug!(
                                    "Server → Backend [{}]: {} bytes\n{:?}",
                                    connection_id_for_client,
                                    n,
                                    String::from_utf8_lossy(
                                        &server_to_backend_buffer[..std::cmp::min(n, 100)]
                                    )
                                );
                            }
                            _ => {}
                        }

                        // Log periodically for large transfers
                        if bytes_since_last_log > 1024 * 1024
                            && last_log_time.elapsed() > Duration::from_secs(5)
                        {
                            let elapsed_secs = last_log_time.elapsed().as_secs_f64();
                            let rate = bytes_since_last_log as f64 / elapsed_secs / 1024.0;
                            info!(
                                "Server → Backend: {:.2} KB/s ({} bytes in {:.1}s)",
                                rate, bytes_since_last_log, elapsed_secs
                            );
                            bytes_since_last_log = 0;
                            last_log_time = Instant::now();
                        }

                        // Write to backend
                        if let Err(e) = backend_write
                            .write_all(&server_to_backend_buffer[..n])
                            .await
                        {
                            warn!("Error writing to backend: {}", e);
                            break;
                        }

                        // Flush explicitly
                        if let Err(e) = backend_write.flush().await {
                            warn!("Error flushing to backend: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        if is_connection_error(&e) {
                            debug!("Server connection error: {}", e);
                        } else {
                            error!("Error reading from server: {}", e);
                        }
                        break;
                    }
                }
            }
            let _ = stop_tx_clone.send(()).await;
        });

        // Backend to server task
        let connection_id_for_backend = connection_id.clone();
        let backend_to_server = tokio::spawn(async move {
            let mut last_log_time = Instant::now();
            let mut bytes_since_last_log = 0u64;

            // HTTP response state tracking
            let mut response_data = Vec::new(); // Buffer to collect partial HTTP responses
            let mut awaiting_response = is_keep_alive_http; // Start waiting for response for keep-alive HTTP

            loop {
                match backend_read.read(&mut backend_to_server_buffer).await {
                    Ok(0) => {
                        debug!("Backend closed connection");
                        break;
                    }
                    Ok(n) => {
                        // Process HTTP data if this is a keep-alive HTTP connection
                        if is_keep_alive_http {
                            // Append to our HTTP buffer
                            response_data.extend_from_slice(&backend_to_server_buffer[..n]);

                            // Try to parse as an HTTP response
                            let mut parser = HttpResponseParser::new();
                            parser.extend(&response_data);

                            if parser.is_complete() {
                                // We have a complete HTTP response
                                debug!("Complete HTTP response detected");
                                awaiting_response = true; // Now wait for the next request

                                if let Ok(response) = parser.parse() {
                                    debug!("HTTP response parsed: {}", response.status);

                                    // Check if this is a connection: close response
                                    if response.connection_type == ConnectionType::Close {
                                        let mut state = http_state_clone.lock().await;
                                        *state = ProxyConnectionState::FirstExchange;
                                        debug!(
                                            "Connection: close response detected, will terminate after this exchange"
                                        );
                                    }
                                }

                                // Clear buffer since we've handled this response
                                response_data.clear();
                            }
                        }

                        // Update atomic counter instead of local variable
                        total_received_clone
                            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
                        bytes_since_last_log += n as u64;

                        // Log data based on logging level
                        match data_logging.as_str() {
                            "minimal" => {
                                debug!("Backend → Server: {} bytes", n);
                            }
                            "verbose" => {
                                debug!(
                                    "Backend → Server [{}]: {} bytes\n{:?}",
                                    connection_id_for_backend,
                                    n,
                                    String::from_utf8_lossy(
                                        &backend_to_server_buffer[..std::cmp::min(n, 100)]
                                    )
                                );
                            }
                            _ => {}
                        }

                        // Log periodically for large transfers
                        if bytes_since_last_log > 1024 * 1024
                            && last_log_time.elapsed() > Duration::from_secs(5)
                        {
                            let elapsed_secs = last_log_time.elapsed().as_secs_f64();
                            let rate = bytes_since_last_log as f64 / elapsed_secs / 1024.0;
                            info!(
                                "Backend → Server: {:.2} KB/s ({} bytes in {:.1}s)",
                                rate, bytes_since_last_log, elapsed_secs
                            );
                            bytes_since_last_log = 0;
                            last_log_time = Instant::now();
                        }

                        // Write to server
                        if let Err(e) = server_write.write_all(&backend_to_server_buffer[..n]).await
                        {
                            warn!("Error writing to server: {}", e);
                            break;
                        }

                        // Flush explicitly
                        if let Err(e) = server_write.flush().await {
                            warn!("Error flushing to server: {}", e);
                            break;
                        }
                    }
                    Err(e) => {
                        if is_connection_error(&e) {
                            debug!("Backend connection error: {}", e);
                        } else {
                            error!("Error reading from backend: {}", e);
                        }
                        break;
                    }
                }
            }

            // Send stop signal after loop exits
            let _ = stop_tx.send(()).await;
        });

        // Wait for either direction to complete or idle timeout
        if let Some(idle_duration) = idle_duration {
            tokio::select! {
                _ = stop_rx.recv() => {
                    info!("One side of the connection closed");
                }
                _ = tokio::time::sleep(idle_duration) => {
                    info!("Idle timeout reached, closing connection");
                }
            }
        } else {
            // No timeout, just wait for completion
            let _ = stop_rx.recv().await;
            info!("Connection complete");
        };

        // Wait for tasks to complete before getting the summary (avoid reference errors)
        let _ = tokio::try_join!(server_to_backend, backend_to_server);

        // Get the final byte counts from the atomic counters
        let total_sent_final = total_sent.load(std::sync::atomic::Ordering::Relaxed);
        let total_received_final = total_received.load(std::sync::atomic::Ordering::Relaxed);

        // Summarize the session
        let session_duration = start_time.elapsed();
        info!(
            "Session {} completed: sent {} bytes, received {} bytes in {:.2} seconds",
            connection_id_for_summary,
            total_sent_final,
            total_received_final,
            session_duration.as_secs_f64()
        );

        // Calculate throughput if the session lasted at least 1 second
        if session_duration.as_secs() > 0 {
            let sent_rate = total_sent_final as f64 / session_duration.as_secs_f64() / 1024.0;
            let received_rate =
                total_received_final as f64 / session_duration.as_secs_f64() / 1024.0;
            info!(
                "Throughput: sent {:.2} KB/s, received {:.2} KB/s",
                sent_rate, received_rate
            );
        }
    }
}
