use clap::Parser;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tokio::time::Duration;
use tracing::{debug, error, info, warn};

use reverse_tunnel::server::client_manager::validate_callback_stream;
use reverse_tunnel::server::config::Args;
use reverse_tunnel::server::handlers::client::handle_client;
use reverse_tunnel::server::types::ProxyClientInfo;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    info!(
        "Server started with data logging: {}, buffer size: {}KB",
        args.data_logging, args.buffer_size
    );

    let buffer_size = if args.buffer_size < 32 {
        warn!("Buffer size too small, setting to minimum 32KB");
        32 * 1024
    } else if args.buffer_size > 1024 {
        warn!("Buffer size too large, setting to maximum 1024KB");
        1024 * 1024
    } else {
        args.buffer_size as usize * 1024
    };

    let clients: Arc<Mutex<Vec<ProxyClientInfo>>> = Arc::new(Mutex::new(Vec::new()));

    let clients_cleanup = clients.clone();

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;

            let mut clients_lock = clients_cleanup.lock().await;
            let before_count = clients_lock.len();
            let now = std::time::Instant::now();

            clients_lock.retain(|c| now.duration_since(c.last_activity).as_secs() < 300);

            let removed = before_count - clients_lock.len();
            if removed > 0 {
                info!(
                    "Periodic cleanup: removed {} stale client sessions",
                    removed
                );
            }
        }
    });
    let listener = TcpListener::bind((args.host.clone(), args.port))
        .await
        .unwrap();
    info!("Server listening on {}:{}", &args.host, args.port);

    loop {
        match listener.accept().await {
            Ok((stream, addr)) => {
                if let Err(e) = stream.set_nodelay(true) {
                    debug!("Failed to set TCP_NODELAY on new connection: {}", e);
                }

                let handshake_id = {
                    let mut callback_buf = [0; 2048];
                    let mut callback_handshake = None;

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

                    let mut stream = stream;

                    validate_callback_stream(&stream, &id).await;

                    let mut buf = [0u8; 1024];
                    let mut callback_line = String::new();

                    match stream.peek(&mut buf).await {
                        Ok(n) => {
                            if let Ok(data) = std::str::from_utf8(&buf[..n]) {
                                callback_line = data.to_string();
                                info!(
                                    "Peeked callback data: '{}' ({} bytes)",
                                    data.trim().chars().take(100).collect::<String>(),
                                    n
                                );
                            }
                        }
                        Err(e) => {
                            error!("Error peeking callback data: {}", e);
                        }
                    }

                    match tokio::io::AsyncReadExt::read(&mut stream, &mut buf).await {
                        Ok(n) => {
                            info!("Read and discarded {} bytes from callback", n);
                        }
                        Err(e) => {
                            error!("Error reading callback data: {}", e);
                        }
                    }

                    let mut sender = None;
                    {
                        let mut clients_lock = clients.lock().await;
                        for client in clients_lock.iter_mut() {
                            if client.handshake_id == id {
                                sender = client.handshake_channel.take();
                                client.last_activity = std::time::Instant::now();
                                break;
                            }
                        }
                    }

                    if let Some(tx) = sender {
                        match tx.send(stream) {
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
