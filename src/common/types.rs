use tokio::net::TcpStream;

pub struct ProxyClientInfo {
    pub backend_port: u16,
    pub public_port: u16,
    pub handshake_channel: Option<tokio::sync::oneshot::Sender<TcpStream>>,
    pub handshake_id: String,
    pub last_activity: std::time::Instant,
    pub reconnect_attempts: u32,
}
