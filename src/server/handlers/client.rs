use crate::server::handlers::http::parse_http_request;
use crate::server::handlers::tcp::handle_public_connection;
use crate::server::types::ProxyClientInfo;
use std::sync::Arc;
use tokio::io::BufReader;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info};

pub async fn handle_client(
    control_stream: TcpStream,
    _clients: Arc<Mutex<Vec<ProxyClientInfo>>>,
    connection_timeout: u64,
    idle_timeout: u64,
) {
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

    match tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut line).await {
        Ok(0) => {
            error!("Client disconnected before sending any data");
            return;
        }
        Ok(_) => {}
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
                            info!("Detected HTTP request for path: {}, keep-alive: {}, connection-close: {}", request_path, is_keep_alive, is_connection_close);
                        }

                        // Debug: print the raw data received from the public connection
                        if n > 0 {
                            let data = &initial_data[..n];
                            match std::str::from_utf8(data) {
                                Ok(s) => {
                                    // Log the first line or up to 100 chars to avoid cluttering logs
                                    let preview = s.lines().next().unwrap_or("").chars().take(100).collect::<String>();
                                    debug!("Received from public: {}{}", preview, if preview.len() < s.len() { "..." } else { "" });
                                },
                                Err(_) => debug!("Received from public (hex): {}", data.iter().take(50).map(|b| format!("{:02x}", b)).collect::<Vec<_>>().join(" ") + "..."),
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

                            clients.push(ProxyClientInfo {
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
                                handle_public_connection(
                                    public_stream,
                                    client_stream,
                                    initial_data,
                                    idle_timeout,
                                    "verbose".to_string()
                                ).await;
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
            read_result = tokio::io::AsyncBufReadExt::read_line(&mut reader, &mut disconnect_buf) => {
                if read_result.unwrap_or(0) == 0 {
                    info!("Client disconnected, closing public port {}", public_port);
                    break;
                }
            }
        }
    }
}
