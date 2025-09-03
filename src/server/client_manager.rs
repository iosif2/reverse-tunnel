use crate::server::types::ProxyClientInfo;
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::Mutex;
use tracing::{debug, info, warn};

pub fn update_client_activity(clients: &Arc<Mutex<Vec<ProxyClientInfo>>>, client_id: &str) {
    tokio::spawn({
        let clients = clients.clone();
        let client_id = client_id.to_string();
        async move {
            let mut clients_lock = clients.lock().await;
            for client in clients_lock.iter_mut() {
                if client.handshake_id == client_id {
                    client.last_activity = std::time::Instant::now();
                    debug!("Updated activity timestamp for client: {}", client_id);
                    break;
                }
            }
        }
    });
}

pub async fn validate_callback_stream(stream: &TcpStream, callback_id: &str) -> bool {
    if let Ok(peer_addr) = stream.peer_addr() {
        info!(
            "Validating callback stream from {} for ID {}",
            peer_addr, callback_id
        );
    } else {
        warn!(
            "Unable to get peer address for callback stream with ID {}",
            callback_id
        );
    }

    // 소켓 정보 로깅
    if let Ok(local_addr) = stream.local_addr() {
        info!("Callback stream local address: {}", local_addr);
    }

    // TCP 스트림 설정 확인
    let socket_ref = socket2::SockRef::from(stream);

    // 소켓 옵션 검사
    match socket_ref.keepalive() {
        Ok(status) => info!("Callback stream keepalive status: {}", status),
        Err(e) => warn!("Unable to get keepalive status: {}", e),
    }

    // 버퍼 크기 로깅
    if let Ok(size) = socket_ref.recv_buffer_size() {
        info!("Callback stream receive buffer size: {} bytes", size);
    }
    if let Ok(size) = socket_ref.send_buffer_size() {
        info!("Callback stream send buffer size: {} bytes", size);
    }

    true
}
