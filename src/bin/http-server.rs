use axum::{Json, Router, http::StatusCode, routing::get};
use serde::Serialize;
use std::net::SocketAddr;

// API 응답을 위한 구조체
#[derive(Serialize)]
struct HealthCheckResponse {
    status: String,
    message: String,
}

#[tokio::main]
async fn main() {
    // 헬스 체크 핸들러 함수를 라우터에 연결
    let app = Router::new().route("/health", get(health_check_handler));

    // 서버 주소 설정
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    println!("Server running on {}", addr);

    // 서버 시작
    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .await
        .unwrap();
}

// 헬스 체크 핸들러 함수
async fn health_check_handler() -> (StatusCode, Json<HealthCheckResponse>) {
    let response = HealthCheckResponse {
        status: "ok".to_string(),
        message: "Server is healthy".to_string(),
    };
    (StatusCode::OK, Json(response))
}
