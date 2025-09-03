use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

use crate::client::handlers::handle_one_time_http_request;
use crate::client::handlers::handle_websocket_connection;
use crate::common::errors::is_connection_error;
use crate::common::http_conn::{
    ConnectionType, HttpRequestParser, HttpResponseParser, ProxyConnectionState,
};
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tokio::time::{Duration, Instant};

// Add a helper function to handle legacy connections:
pub async fn handle_legacy_connection(
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
