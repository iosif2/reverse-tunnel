/// HTTP 프록시 연결 상태
#[derive(Debug, Clone, PartialEq)]
pub enum ProxyConnectionState {
    /// 초기 상태
    Initial,
    /// 첫 요청/응답 처리 중
    FirstExchange,
    /// 활성 상태 (요청/응답 교환 중)
    Active,
    /// 웹소켓 모드
    WebSocket,
    /// 닫는 중
    Closing,
    /// 닫힘
    Closed,
    /// 오류 발생
    Error,
}
