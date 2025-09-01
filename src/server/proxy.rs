use crate::common::errors::is_connection_error;
use crate::http_utils::{ConnectionType, HttpResponseParser};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info, warn};

pub async fn handle_one_time_proxy(public_stream: &mut TcpStream, client_stream: &mut TcpStream) {
    debug!("Handling one-time HTTP exchange");

    let mut response_parser = HttpResponseParser::new();

    let mut response_buffer = vec![0u8; 32768]; // 초기 버퍼 크기 32KB로 변경

    match timeout(
        Duration::from_secs(30),
        client_stream.read(&mut response_buffer),
    )
    .await
    {
        Ok(Ok(n)) => {
            if n > 0 {
                // 실제 읽은 크기로 버퍼 조정
                response_buffer.truncate(n);

                // 응답 파서에 데이터 추가
                response_parser.extend(&response_buffer);

                // Forward response to public connection
                if let Err(e) = public_stream.write_all(&response_buffer).await {
                    error!("Error sending response to public: {}", e);
                    return;
                }

                // 명시적 플러시 추가
                if let Err(e) = public_stream.flush().await {
                    error!("Error flushing response to public: {}", e);
                    return;
                }

                // 응답 데이터 로깅
                if let Ok(text) = std::str::from_utf8(&response_buffer) {
                    let first_line = text.lines().next().unwrap_or("");
                    info!("Forwarded response to public: {}", first_line);
                }

                // 응답 파싱
                let mut is_keep_alive = false;
                let mut has_chunked_encoding = false;
                let mut content_length = None;
                let mut headers_end = 0;
                let mut is_complete = response_parser.is_complete();

                if let Ok(response) = response_parser.parse() {
                    // 연결 타입 확인
                    is_keep_alive = response.connection_type == ConnectionType::KeepAlive;

                    // 청크 인코딩 확인
                    if let Some(te) = response.headers.get("transfer-encoding") {
                        if let Ok(value) = te.to_str() {
                            has_chunked_encoding = value.to_lowercase().contains("chunked");
                        }
                    }

                    // Content-Length 추출
                    if let Some(cl) = response.headers.get("content-length") {
                        if let Ok(value) = cl.to_str() {
                            if let Ok(len) = value.parse::<usize>() {
                                content_length = Some(len);
                            }
                        }
                    }

                    // 헤더 끝 위치 추정
                    if let Some(body) = &response.body {
                        headers_end = response_buffer.len() - body.len();
                    } else if let Ok(text) = std::str::from_utf8(&response_buffer) {
                        if let Some(pos) = text.find("\r\n\r\n") {
                            headers_end = pos + 4;
                        }
                    }

                    // 디버그 정보 출력
                    debug!(
                        "HTTP response: status={}, version={}, connection-type={:?}, content-length={:?}, chunked={}",
                        response.status,
                        response.version,
                        response.connection_type,
                        content_length,
                        has_chunked_encoding
                    );
                }

                // Content-Length를 기반으로 응답 완전성 확인
                let mut need_more = false;

                if let Some(len) = content_length {
                    if headers_end > 0 {
                        let body_received = response_buffer.len() - headers_end;
                        need_more = body_received < len;

                        if need_more {
                            debug!(
                                "Partial HTTP response: {}/{} bytes of body",
                                body_received, len
                            );
                        }
                    }
                } else if has_chunked_encoding {
                    // 청크 인코딩의 경우 마지막 청크(0 크기)가 있는지 확인
                    let has_final_chunk = response_buffer.windows(5).any(|w| w == b"0\r\n\r\n");
                    need_more = !has_final_chunk;

                    if need_more {
                        debug!("Chunked encoding detected but response is incomplete");
                    }
                }

                // If we need more data or response indicates keep-alive, continue reading
                if need_more || is_keep_alive {
                    // 필요하면 버퍼 확장
                    if response_buffer.len() > response_buffer.capacity() / 2 {
                        response_buffer.reserve(response_buffer.capacity());
                    }

                    // Read remaining data in a loop
                    loop {
                        // 버퍼 크기 조정 (필요시 두 배 확장)
                        if response_buffer.len() > response_buffer.capacity() / 2 {
                            let new_capacity = response_buffer.capacity() * 2;
                            let mut new_buffer = Vec::with_capacity(new_capacity);
                            new_buffer.extend_from_slice(&response_buffer);
                            response_buffer = new_buffer;
                        }

                        let buf_pos = response_buffer.len();
                        response_buffer.resize(response_buffer.capacity(), 0);

                        match timeout(
                            Duration::from_secs(5),
                            client_stream.read(&mut response_buffer[buf_pos..]),
                        )
                        .await
                        {
                            Ok(Ok(0)) => {
                                // EOF
                                response_buffer.truncate(buf_pos);
                                break;
                            }
                            Ok(Ok(n)) => {
                                // 실제 읽은 크기로 버퍼 조정
                                response_buffer.truncate(buf_pos + n);

                                // 응답 파서에 새 데이터 추가
                                response_parser.extend(&response_buffer[buf_pos..]);

                                // Forward the additional data
                                if let Err(e) =
                                    public_stream.write_all(&response_buffer[buf_pos..]).await
                                {
                                    error!(
                                        "Error sending additional response data to public: {}",
                                        e
                                    );
                                    break;
                                }

                                // 명시적 플러시 추가
                                if let Err(e) = public_stream.flush().await {
                                    error!(
                                        "Error flushing additional response data to public: {}",
                                        e
                                    );
                                    break;
                                }

                                debug!("Forwarded {} more bytes to public", n);

                                // 응답 완전성 다시 확인
                                if let Some(len) = content_length {
                                    // Content-Length 기반 확인
                                    if headers_end > 0 {
                                        let body_received = response_buffer.len() - headers_end;
                                        if body_received >= len {
                                            debug!(
                                                "Received complete HTTP response: {} bytes of body",
                                                body_received
                                            );
                                            break;
                                        }
                                    }
                                } else if has_chunked_encoding {
                                    // 청크 인코딩의 경우 마지막 청크 확인
                                    if response_buffer[buf_pos..]
                                        .windows(5)
                                        .any(|w| w == b"0\r\n\r\n")
                                        || response_buffer.windows(5).any(|w| w == b"0\r\n\r\n")
                                    {
                                        debug!("Received complete chunked response");
                                        break;
                                    }
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Error reading additional data from client: {}", e);
                                break;
                            }
                            Err(_) => {
                                debug!(
                                    "Timeout reading additional data from client, assuming response complete"
                                );
                                break;
                            }
                        }
                    }
                }

                info!(
                    "One-time HTTP exchange completed, total response size: {} bytes",
                    response_buffer.len()
                );
            } else {
                error!("Client closed connection without sending response");
            }
        }
        Ok(Err(e)) => {
            error!("Error reading response from client: {}", e);
        }
        Err(_) => {
            error!("Timeout waiting for response from client");
        }
    }

    // Close connections explicitly
    info!("Closing one-time HTTP connection");
    let _ = public_stream.shutdown().await;
    let _ = client_stream.shutdown().await;
}

pub async fn handle_websocket_proxy(public_stream: TcpStream, client_stream: TcpStream) {
    debug!("Starting WebSocket proxy mode");

    let (mut pr, mut pw) = tokio::io::split(public_stream);
    let (mut cr, mut cw) = tokio::io::split(client_stream);

    let to_client = tokio::spawn(async move {
        info!("WebSocket: Transferring data: public -> client");
        let mut buffer = [0u8; 32768];
        let mut total_bytes: u64 = 0;

        loop {
            match pr.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: public side closed the connection");
                    break;
                }
                Ok(n) => match cw.write_all(&buffer[..n]).await {
                    Ok(_) => {
                        if let Err(e) = cw.flush().await {
                            error!("WebSocket: Error flushing data to client: {}", e);
                            break;
                        }

                        total_bytes += n as u64;
                        debug!(
                            "WebSocket: public -> client: {} bytes, data: {:?}",
                            n,
                            &buffer[..std::cmp::min(n, 50)]
                        );
                        if total_bytes % 10240 == 0 {
                            debug!(
                                "WebSocket: public -> client: {} bytes transferred",
                                total_bytes
                            );
                        }
                    }
                    Err(e) => {
                        if is_connection_error(&e) {
                            info!("WebSocket: public -> client: Connection closed: {}", e);
                        } else {
                            error!("WebSocket: Error copying public -> client: {}", e);
                        }
                        break;
                    }
                },
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: public -> client: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from public: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: public -> client: {} total bytes transferred",
            total_bytes
        );
    });

    let to_public = tokio::spawn(async move {
        info!("WebSocket: Transferring data: client -> public");
        let mut buffer = [0u8; 32768];
        let mut total_bytes: u64 = 0;

        loop {
            match cr.read(&mut buffer).await {
                Ok(0) => {
                    info!("WebSocket: client side closed the connection");
                    break;
                }
                Ok(n) => {
                    debug!(
                        "WebSocket: Received from client: {} bytes, data: {:?}",
                        n,
                        &buffer[..std::cmp::min(n, 50)]
                    );

                    match pw.write_all(&buffer[..n]).await {
                        Ok(_) => {
                            // 데이터를 즉시 전송하기 위해 명시적으로 플러시 추가
                            if let Err(e) = pw.flush().await {
                                error!("WebSocket: Error flushing data to public: {}", e);
                                break;
                            }

                            total_bytes += n as u64;
                            if total_bytes % 10240 == 0 {
                                // Log every ~10KB
                                debug!(
                                    "WebSocket: client -> public: {} bytes transferred",
                                    total_bytes
                                );
                            }

                            // 대용량 데이터 전송 시 경고 로그 추가
                            if total_bytes > 100000 && total_bytes % 100000 < 10240 {
                                warn!(
                                    "WebSocket: Large data transfer: client -> public: {} bytes total",
                                    total_bytes
                                );
                            }
                        }
                        Err(e) => {
                            if is_connection_error(&e) {
                                info!("WebSocket: client -> public: Connection closed: {}", e);
                            } else {
                                error!("WebSocket: Error copying client -> public: {}", e);
                            }
                            break;
                        }
                    }
                }
                Err(e) => {
                    if is_connection_error(&e) {
                        info!("WebSocket: client -> public: Connection closed: {}", e);
                    } else {
                        error!("WebSocket: Error reading from client: {}", e);
                    }
                    break;
                }
            }
        }

        info!(
            "WebSocket: client -> public: {} total bytes transferred",
            total_bytes
        );
    });

    match tokio::try_join!(to_client, to_public) {
        Ok(_) => info!("WebSocket proxy session closed normally"),
        Err(e) => warn!("WebSocket proxy session closed with error: {:?}", e),
    }
}
