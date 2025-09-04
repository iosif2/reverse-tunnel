use axum::http::Request;
use axum::{Json, Router, body::Body, http::StatusCode, routing::{get, post}};
use log::info;
use reverse_tunnel::common::http_conn::parser::request::HttpRequestParser;
use reverse_tunnel::common::http_conn::types::{HttpRequest, serialize_http_request_to_bytes};
use serde::Serialize;
use std::net::SocketAddr;

#[derive(Serialize)]
struct HealthCheckResponse {
    status: String,
    message: String,
}

#[tokio::main]
async fn main() {
    log4rs::init_file("log4rs.yml", Default::default()).unwrap();
    // 헬스 체크 핸들러 함수를 라우터에 연결
    let app = Router::new().route("/", get(health_check_handler));
    let app = app.merge(Router::new().route("/", post(health_check_handler)));
    // 서버 주소 설정
    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    info!("Server running on {}", addr);

    // 서버 시작
    axum::serve(tokio::net::TcpListener::bind(addr).await.unwrap(), app)
        .await
        .unwrap();
}

// 헬스 체크 핸들러 함수
async fn health_check_handler(req: Request<Body>) -> (StatusCode, Json<HealthCheckResponse>) {
    let mut parser = HttpRequestParser::new();
    let http_req = HttpRequest::from_axum_request(req).await.unwrap();
    parser.extend(serialize_http_request_to_bytes(&http_req).unwrap().as_ref());
    let _ = parser.parse().unwrap();
    info!("Request is_complete: {:?}", parser.is_complete());
    info!(
        "Request is_websocket_upgrade: {:?}",
        parser.is_websocket_upgrade()
    );
    info!("Request method: {:?}", http_req.method);
    info!("Request path: {:?}", http_req.path);
    info!("Request headers: {:?}", http_req.headers);
    info!("Request body: {:?}", http_req.body);
    let response = HealthCheckResponse {
        status: "ok".to_string(),
        message: "Server is healthy".to_string(),
    };
    (StatusCode::OK, Json(response))
}
