use crate::common::{
    ConnectionType, HttpRequestParser, ProxyConnectionState, is_connection_error, log_data_sample,
    log_transfer_summary,
};
use crate::server::{handle_one_time_proxy, handle_websocket_proxy};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

pub async fn handle_public_connection(
    mut public_stream: TcpStream,
    mut client_stream: TcpStream,
    initial_data: Vec<u8>,
    idle_timeout: u64,
    data_logging: String,
) {
    info!("Proxy session established: public <-> client");

    let session_id = format!("{}", chrono::Utc::now().timestamp_millis());
    info!("Starting proxy session {}", session_id);

    let mut connection_state = ProxyConnectionState::Initial;

    if let Err(e) = public_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on public stream: {}", e);
    }
    if let Err(e) = client_stream.set_nodelay(true) {
        debug!("Failed to set TCP_NODELAY on client stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&public_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on public stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&client_stream);
    if let Err(e) = socket_ref.set_keepalive(true) {
        debug!("Failed to set SO_KEEPALIVE on client stream: {}", e);
    }

    let socket_ref = socket2::SockRef::from(&public_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(1048576) {
        debug!("Failed to set receive buffer size on public stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(1048576) {
        debug!("Failed to set send buffer size on public stream: {}", e);
    }

    if let Ok(local_addr) = public_stream.local_addr() {
        if let Ok(peer_addr) = public_stream.peer_addr() {
            info!(
                "Public stream details: local={}, peer={}",
                local_addr, peer_addr
            );
        }
    }

    let socket_ref = socket2::SockRef::from(&client_stream);
    if let Err(e) = socket_ref.set_recv_buffer_size(1048576) {
        debug!("Failed to set receive buffer size on client stream: {}", e);
    }
    if let Err(e) = socket_ref.set_send_buffer_size(1048576) {
        debug!("Failed to set send buffer size on client stream: {}", e);
    }

    if let Ok(local_addr) = client_stream.local_addr() {
        if let Ok(peer_addr) = client_stream.peer_addr() {
            info!(
                "Client stream details: local={}, peer={}",
                local_addr, peer_addr
            );
        }
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
    let to_client = tokio::spawn({
        let session_id = session_id.clone();
        async move {
            info!("Transferring data: public -> client");
            let start_time = std::time::Instant::now();

            let result = if let Some(timeout_duration) = idle_timeout_duration {
                // With timeout - will abort if no data transfer happens within the timeout
                let mut buffer = [0u8; 32768]; // 버퍼 크기 32KB로 증가
                let mut total_bytes: u64 = 0;
                let start_time = std::time::Instant::now();

                loop {
                    match timeout(timeout_duration, pr.read(&mut buffer)).await {
                        Ok(Ok(0)) => break, // EOF
                        Ok(Ok(n)) => match cw.write_all(&buffer[..n]).await {
                            Ok(_) => {
                                // 데이터 즉시 플러시 추가
                                if let Err(e) = cw.flush().await {
                                    error!("Error flushing data to client: {}", e);
                                    break;
                                }

                                total_bytes += n as u64;
                                debug!("public -> client: {} bytes transferred", n);
                                log_data_sample("public -> client", &buffer[..n], n, &data_logging);

                                // 주기적으로 전송량 로그 출력 (1MB마다)
                                if total_bytes % (1024 * 1024) < n as u64 {
                                    let elapsed = start_time.elapsed();
                                    let rate = if elapsed.as_secs() > 0 {
                                        total_bytes as f64 / elapsed.as_secs() as f64
                                    } else {
                                        total_bytes as f64
                                    };

                                    info!(
                                        "Session {}: public -> client: {} bytes ({:.2} KB/s)",
                                        session_id,
                                        total_bytes,
                                        rate / 1024.0
                                    );
                                }

                                // 첫 100바이트 데이터 로깅 (트러블슈팅용)
                                if n > 10 && total_bytes < 10000 {
                                    info!(
                                        "Data sample (public -> client): {:?}",
                                        &buffer[..std::cmp::min(n, 100)]
                                    );
                                }

                                if total_bytes > 100_000_000 {
                                    // 100MB 이상 전송시 경고
                                    warn!(
                                        "Large data transfer: public -> client: {} bytes total",
                                        total_bytes
                                    );
                                }

                                // 대용량 패킷은 처리 시간을 확보하기 위해 스레드 양보
                                if n > 100000 {
                                    tokio::task::yield_now().await;
                                }
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
                // No timeout - use custom copy with logging
                let mut buffer = [0u8; 32768];
                let mut total_bytes: u64 = 0;
                let start_time = std::time::Instant::now();

                loop {
                    match pr.read(&mut buffer).await {
                        Ok(0) => break, // EOF
                        Ok(n) => match cw.write_all(&buffer[..n]).await {
                            Ok(_) => {
                                // 데이터 즉시 플러시 추가
                                if let Err(e) = cw.flush().await {
                                    error!("Error flushing data to client: {}", e);
                                    break;
                                }

                                total_bytes += n as u64;

                                // 주기적으로 전송량 로그 출력 (1MB마다)
                                if total_bytes % (1024 * 1024) < n as u64 {
                                    let elapsed = start_time.elapsed();
                                    let rate = if elapsed.as_secs() > 0 {
                                        total_bytes as f64 / elapsed.as_secs() as f64
                                    } else {
                                        total_bytes as f64
                                    };

                                    info!(
                                        "Session {}: public -> client: {} bytes ({:.2} KB/s)",
                                        session_id,
                                        total_bytes,
                                        rate / 1024.0
                                    );
                                }
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
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("public -> client: Connection closed: {}", e);
                            } else {
                                error!("Error reading from public: {}", e);
                            }
                            break;
                        }
                    }
                }

                Ok(total_bytes)
            };

            match result {
                Ok(bytes) => {
                    let elapsed = start_time.elapsed();
                    let rate = if elapsed.as_secs() > 0 {
                        bytes as f64 / elapsed.as_secs() as f64
                    } else {
                        bytes as f64
                    };

                    info!(
                        "Session {}: public -> client completed: {} bytes transferred in {:.2}s ({:.2} KB/s)",
                        session_id,
                        bytes,
                        elapsed.as_secs_f64(),
                        rate / 1024.0
                    );
                    log_transfer_summary(&session_id, "public -> client", bytes);
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("public -> client: Connection closed: {}", e);
                    } else {
                        error!("Error copying public -> client: {}", e);
                    }
                }
            }
        }
    });

    // Forward data from client to public with optional timeout
    let to_public = tokio::spawn({
        let session_id = session_id.clone();
        async move {
            info!("Transferring data: client -> public");
            let start_time = std::time::Instant::now();

            let result = if let Some(timeout_duration) = idle_timeout_duration {
                // With timeout - will abort if no data transfer happens within the timeout
                let mut buffer = [0u8; 32768]; // 버퍼 크기 32KB로 증가
                let mut total_bytes: u64 = 0;
                let start_time = std::time::Instant::now();

                loop {
                    match timeout(timeout_duration, cr.read(&mut buffer)).await {
                        Ok(Ok(0)) => break, // EOF
                        Ok(Ok(n)) => match pw.write_all(&buffer[..n]).await {
                            Ok(_) => {
                                // 데이터 즉시 플러시 추가
                                if let Err(e) = pw.flush().await {
                                    error!("Error flushing data to public: {}", e);
                                    break;
                                }

                                total_bytes += n as u64;
                                debug!(
                                    "client -> public: {} bytes transferred, first bytes: {:?}",
                                    n,
                                    &buffer[..std::cmp::min(n, 20)]
                                );

                                // 주기적으로 전송량 로그 출력 (1MB마다)
                                if total_bytes % (1024 * 1024) < n as u64 {
                                    let elapsed = start_time.elapsed();
                                    let rate = if elapsed.as_secs() > 0 {
                                        total_bytes as f64 / elapsed.as_secs() as f64
                                    } else {
                                        total_bytes as f64
                                    };

                                    info!(
                                        "Session {}: client -> public: {} bytes ({:.2} KB/s)",
                                        session_id,
                                        total_bytes,
                                        rate / 1024.0
                                    );
                                }
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
                Ok(bytes) => {
                    let elapsed = start_time.elapsed();
                    let rate = if elapsed.as_secs() > 0 {
                        bytes as f64 / elapsed.as_secs() as f64
                    } else {
                        bytes as f64
                    };

                    info!(
                        "Session {}: client -> public completed: {} bytes transferred in {:.2}s ({:.2} KB/s)",
                        session_id,
                        bytes,
                        elapsed.as_secs_f64(),
                        rate / 1024.0
                    );
                    log_transfer_summary(&session_id, "client -> public", bytes);
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("client -> public: Connection closed: {}", e);
                    } else {
                        error!("Error copying client -> public: {}", e);
                    }
                }
            }
        }
    });

    let _ = tokio::try_join!(to_client, to_public);
    info!("Proxy session closed");
}
