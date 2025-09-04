use crate::client::{
    ConnectionPool, ProxyResult, handle_one_time_http_request, handle_websocket_connection,
};
use crate::common::{
    ConnectionType, HttpRequestParser, HttpResponseParser, ProxyConnectionState,
    is_connection_error, is_websocket_upgrade_response,
};
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

/// 서버로 부터 받은 데이터를 백엔드로 릴레이 하는 함수
pub async fn handle_backend_connection(
    mut server_stream: TcpStream,
    backend_port: u16,
    connection_timeout: u64,
    idle_timeout: u64,
    pool: Arc<Mutex<ConnectionPool>>,
    data_logging: String,
    active_sessions: Option<Arc<Mutex<std::collections::HashSet<String>>>>,
) -> ProxyResult<()> {
    // 연결 상태
    let mut connection_state = ProxyConnectionState::Initial;

    // Try to get a connection from the pool
    let (mut backend_stream, connection_id) = {
        let mut pool = pool.lock().await;
        if let Some(conn) = pool.get_connection(backend_port, connection_timeout).await {
            conn
        } else {
            error!("Failed to connect to backend:{}", backend_port);
            return Err("Failed to connect to backend".into());
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
            return Err(e.into());
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
            return Err(e.into());
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
                        return Err(e.into());
                    }

                    let mut response_parser = HttpResponseParser::new();
                    response_parser.extend(&response_buffer);

                    let is_valid_websocket = if response_parser.is_complete() {
                        if let Ok(response) = response_parser.parse() {
                            is_websocket_upgrade_response(response.status, &response.headers)
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
                    return Err("Failed to get backend response for WebSocket upgrade".into());
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
