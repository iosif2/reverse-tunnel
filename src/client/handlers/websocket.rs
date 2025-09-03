use crate::client::ProxyResult;
use crate::common::errors::is_connection_error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, info, warn};

// Handle WebSocket connection with special handling
pub async fn handle_websocket_connection(
    server_stream: TcpStream,
    backend_stream: TcpStream,
) -> ProxyResult<()> {
    debug!("Starting WebSocket proxy mode between server and backend");

    // Use the split function to get owned halves for async tasks
    let (sr, sw) = tokio::io::split(server_stream);
    let (br, bw) = tokio::io::split(backend_stream);

    // sr, sw, br, bw를 가변 참조로 변환
    let mut sr = sr;
    let mut sw = sw;
    let mut br = br;
    let mut bw = bw;

    // Forward data from server to backend without timeout (WebSockets can idle)
    let to_backend = tokio::spawn(async move {
        info!("WebSocket: Transferring data: server -> backend");
        let mut buffer = [0u8; 32768]; // 버퍼 크기 증가
        let mut total_bytes: u64 = 0;

        loop {
            match sr.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: server side closed the connection");
                    break;
                }
                Ok(n) => {
                    match bw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            // 데이터를 즉시 전송하기 위해 명시적으로 플러시 추가
                            if let Err(e) = bw.flush().await {
                                error!("WebSocket: Error flushing data to backend: {}", e);
                                break;
                            }

                            total_bytes += n as u64;
                            debug!("WebSocket: server -> backend: {} bytes transferred", n);
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: server -> backend: {} bytes transferred",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: server -> backend: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying server -> backend: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: server -> backend: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from server: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: server -> backend: {} total bytes transferred",
            total_bytes
        );
    });

    // Forward data from backend to server without timeout
    let to_server = tokio::spawn(async move {
        info!("WebSocket: Transferring data: backend -> server");
        let mut buffer = [0u8; 32768]; // 버퍼 크기 증가
        let mut total_bytes: u64 = 0;

        loop {
            match br.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: backend side closed the connection");
                    break;
                }
                Ok(n) => {
                    match sw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            // 데이터를 즉시 전송하기 위해 명시적으로 플러시 추가
                            if let Err(e) = sw.flush().await {
                                error!("WebSocket: Error flushing data to server: {}", e);
                                break;
                            }

                            total_bytes += n as u64;
                            debug!("WebSocket: backend -> server: {} bytes transferred", n);
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: backend -> server: {} bytes transferred",
                                    total_bytes
                                );
                            }

                            // 대용량 데이터 전송 시 경고 로그 추가
                            if total_bytes > 100000 && total_bytes % 100000 < 10240 {
                                warn!(
                                    "WebSocket: Large data transfer: backend -> server: {} bytes total",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: backend -> server: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying backend -> server: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: backend -> server: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from backend: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: backend -> server: {} total bytes transferred",
            total_bytes
        );
    });

    match tokio::try_join!(to_backend, to_server) {
        Ok(_) => info!("WebSocket proxy session closed normally"),
        Err(e) => warn!("WebSocket proxy session closed with error: {:?}", e),
    }

    Ok(())
}
