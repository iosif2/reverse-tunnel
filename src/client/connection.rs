use std::collections::HashMap;
use std::time::Instant;
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::debug;

// Add a struct to track connection pool information
pub struct PooledConnection {
    stream: TcpStream,
    last_used: Instant,
    session_id: String,
}

// Connection pool to manage and reuse backend connections
pub struct ConnectionPool {
    connections: HashMap<u16, Vec<PooledConnection>>,
    max_idle_time: Duration,
    max_pool_size: usize,
}

impl ConnectionPool {
    pub fn new(max_idle_seconds: u64, max_pool_size: usize) -> Self {
        Self {
            connections: HashMap::new(),
            max_idle_time: Duration::from_secs(max_idle_seconds),
            max_pool_size,
        }
    }

    // Get a connection from the pool or create a new one
    pub async fn get_connection(
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
    pub async fn return_connection(&mut self, port: u16, stream: TcpStream, session_id: String) {
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
    pub async fn cleanup(&mut self) {
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
