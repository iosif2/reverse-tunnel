//! Reverse TCP Proxy Library
//!
//! 이 라이브러리는 리버스 TCP 프록시 서버를 구현하기 위한 모듈들을 제공합니다.
//! HTTP, WebSocket, 일반 TCP 연결을 지원하며, 클라이언트 등록 및 관리 기능을 포함합니다.

pub mod common;
pub mod http_utils;
pub mod server;

// 공통 타입들을 재내보내기
pub use common::errors::*;
pub use common::types::*;

// HTTP 유틸리티들을 재내보내기
pub use http_utils::*;

// 서버 모듈의 주요 구성 요소들을 재내보내기
// pub use server::client_manager::ClientManager;
// pub use server::config::ServerConfig;
// pub use server::proxy::ProxyServer;

// 핸들러들을 재내보내기
// pub use server::handlers::http::HttpHandler;
// pub use server::handlers::tcp::TcpHandler;
// pub use server::handlers::websocket::WebSocketHandler;

// 유틸리티들을 재내보내기
pub use server::utils::logging::*;
// pub use server::utils::network::*;

/// 라이브러리 버전 정보
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// 기본 버퍼 크기 (KB)
pub const DEFAULT_BUFFER_SIZE: usize = 32 * 1024;

/// 기본 연결 타임아웃 (초)
pub const DEFAULT_CONNECTION_TIMEOUT: u64 = 5;

/// 기본 유휴 타임아웃 (초, 0은 무제한)
pub const DEFAULT_IDLE_TIMEOUT: u64 = 0;
