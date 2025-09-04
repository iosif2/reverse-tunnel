use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

use crate::client::{
    Args, ConnectionPool, ProxyResult, handle_backend_connection, handle_legacy_connection,
};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

// The main connection and serving logic extracted from main
pub async fn connect_and_serve(args: &Args, running: Arc<AtomicBool>) -> ProxyResult<()> {
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
        Ok(Err(e)) => return Err(format!("Failed to connect to server: {}", e).into()),
        Err(_) => {
            return Err("Connection timeout while connecting to server".into());
        }
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
        return Err(format!("Failed to send registration message: {}", e).into());
    }

    // 등록 메시지 플러시 강제화
    if let Err(e) = control_stream.flush().await {
        return Err(format!("Failed to flush registration message: {}", e).into());
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
                return Err("Server closed connection".to_string().into());
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
                            return Err("Server closed connection while waiting for CONNECT"
                                .to_string()
                                .into());
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
                                                        Ok(Ok(backend_stream)) => {
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
                            return Err(format!("Error reading from server: {}", e).into());
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
                return Err(format!("Error reading from server: {}", e).into());
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
