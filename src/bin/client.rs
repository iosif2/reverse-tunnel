use clap::Parser;
use reverse_tunnel::client::{Args, connect_and_serve};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::time::Duration;
use tracing::{error, info};

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
