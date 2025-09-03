use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::{Duration, timeout};
use tracing::{debug, error, info};

use crate::client::ProxyResult;
use crate::common::http_conn::{ConnectionType, HttpResponseParser};

pub async fn handle_one_time_http_request(
    mut server_stream: TcpStream,
    mut backend_stream: TcpStream,
) -> ProxyResult<()> {
    debug!("Handling one-time HTTP request with Connection: close");

    // HTTP 응답 파서 초기화
    let mut response_parser = HttpResponseParser::new();

    // Create a buffer for the backend response
    let mut response_buffer = vec![0u8; 32768]; // 초기 버퍼 크기 32KB로 증가

    // Wait for response from backend (with a generous timeout)
    match timeout(
        Duration::from_secs(30),
        backend_stream.read(&mut response_buffer),
    )
    .await
    {
        Ok(Ok(n)) => {
            if n > 0 {
                // 실제 받은 데이터만 사용
                response_buffer.truncate(n);

                // 응답 파서에 데이터 추가
                response_parser.extend(&response_buffer);

                // Log response summary
                if let Ok(text) = std::str::from_utf8(&response_buffer) {
                    let first_line = text.lines().next().unwrap_or("");
                    info!("Received backend response: {}", first_line);
                }

                // Forward response to server
                if let Err(e) = server_stream.write_all(&response_buffer).await {
                    error!("Error sending response to server: {}", e);
                    return Ok(());
                }

                // 명시적 플러시 추가
                if let Err(e) = server_stream.flush().await {
                    error!("Error flushing response to server: {}", e);
                    return Ok(());
                }

                // 응답이 완전한지 확인
                let mut is_complete = response_parser.is_complete();
                let mut has_chunked_encoding = false;
                let mut content_length = None;
                let mut headers_end = 0;

                // 응답 파싱이 가능하면 상세 정보 추출
                if let Ok(response) = response_parser.parse() {
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

                    // HTTP 버전 확인 (HTTP/1.0은 기본적으로 Connection: close)
                    let is_http10 = response.version == "HTTP/1.0";

                    // Connection: close 헤더 확인
                    let has_connection_close = response.connection_type == ConnectionType::Close;

                    // 응답 본문 시작점 찾기
                    if let Some(body) = &response.body {
                        headers_end = response_buffer.len() - body.len();
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

                // Content-Length 기반으로 응답 완전성 확인
                if let Some(len) = content_length {
                    if let Ok(text) = std::str::from_utf8(&response_buffer) {
                        if let Some(pos) = text.find("\r\n\r\n") {
                            headers_end = pos + 4;
                            let body_received = response_buffer.len() - headers_end;
                            is_complete = body_received >= len;

                            if !is_complete {
                                debug!(
                                    "Partial HTTP response: {}/{} bytes of body",
                                    body_received, len
                                );
                            }
                        }
                    }
                } else if has_chunked_encoding {
                    // 청크 인코딩의 경우 마지막 청크(0 크기)가 있는지 확인
                    is_complete = response_buffer.windows(5).any(|w| w == b"0\r\n\r\n");

                    if !is_complete {
                        debug!("Chunked encoding detected but response is incomplete");
                    }
                }

                // 응답이 완전하지 않으면 계속 읽기
                if !is_complete {
                    debug!("Response is incomplete, continuing to read");

                    // 필요하면 버퍼 확장
                    if response_buffer.len() > response_buffer.capacity() / 2 {
                        response_buffer.reserve(response_buffer.capacity());
                    }

                    // Read remaining data in a loop
                    loop {
                        // 버퍼 크기 조정 (필요한 경우 두 배로 확장)
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
                            backend_stream.read(&mut response_buffer[buf_pos..]),
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
                                    server_stream.write_all(&response_buffer[buf_pos..]).await
                                {
                                    error!(
                                        "Error sending additional response data to server: {}",
                                        e
                                    );
                                    break;
                                }

                                // 명시적 플러시 추가
                                if let Err(e) = server_stream.flush().await {
                                    error!(
                                        "Error flushing additional response data to server: {}",
                                        e
                                    );
                                    break;
                                }

                                debug!("Received {} more bytes from backend", n);

                                // 응답 완전성 다시 확인
                                is_complete = if let Some(len) = content_length {
                                    // Content-Length 기반 확인
                                    if headers_end > 0 {
                                        response_buffer.len() - headers_end >= len
                                    } else {
                                        false
                                    }
                                } else if has_chunked_encoding {
                                    // 청크 인코딩 확인
                                    let chunk_end =
                                        response_buffer.windows(5).any(|w| w == b"0\r\n\r\n");
                                    if chunk_end {
                                        debug!("Found end of chunked encoding");
                                        true
                                    } else {
                                        false
                                    }
                                } else {
                                    // 명확한 종료 조건이 없으면 계속 읽기
                                    false
                                };

                                if is_complete {
                                    debug!("Response is now complete, stopping read loop");
                                    break;
                                }
                            }
                            Ok(Err(e)) => {
                                error!("Error reading additional data from backend: {}", e);
                                break;
                            }
                            Err(_) => {
                                debug!(
                                    "Timeout reading additional data from backend, assuming response complete"
                                );
                                break;
                            }
                        }
                    }
                }

                info!(
                    "One-time HTTP request completed, total response size: {} bytes",
                    response_buffer.len()
                );
            } else {
                error!("Backend closed connection without sending response");
            }
        }
        Ok(Err(e)) => {
            error!("Error reading response from backend: {}", e);
        }
        Err(_) => {
            error!("Timeout waiting for response from backend");
        }
    }

    // Close connections (explicitly, though they would be closed when dropped anyway)
    debug!("Shutting down one-time HTTP connection streams");
    let _ = server_stream.shutdown().await;
    let _ = backend_stream.shutdown().await;

    Ok(())
}
