use clap::Parser;
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]

/// 명령어 인자 정의 구조체
pub struct Args {
    /// 서버 주소
    #[arg(long = "server", short = 's')]
    pub server: String,

    /// 백엔드 포트, 실제 로컬에서 서비스 되는 포트
    #[arg(long = "backend-port", short = 'b')]
    pub backend_port: u16,

    /// 실제 인터넷에서 접속할 포트
    #[arg(long = "public-port", short = 'p')]
    pub public_port: u16,

    /// 백엔드 서버에 연결시 타임아웃 시간
    #[arg(long = "connection-timeout", short = 't', default_value = "5")]
    pub connection_timeout: u64,

    /// 서버에 연결시 유후상태 타임아웃 시간
    #[arg(long = "idle-timeout", short = 'i', default_value = "0")]
    pub idle_timeout: u64,

    /// 재연결 시도 인터벌
    #[arg(long = "reconnect-interval", short = 'r', default_value = "5")]
    pub reconnect_interval: u64,

    /// 재연결 시도 횟수 제한
    #[arg(long = "max-reconnect", short = 'm', default_value = "0")]
    pub max_reconnect_attempts: u32,

    /// 데이터 로깅 레벨
    #[arg(long = "data-logging", short = 'd', default_value = "off")]
    pub data_logging: String,

    /// 연결 풀 최대 크기
    #[arg(long = "max-pool-size", default_value = "10")]
    pub max_pool_size: usize,

    /// 연결 풀 최대 유휴 시간 (초)
    #[arg(long = "pool-idle-timeout", default_value = "300")]
    pub pool_idle_timeout: u64,

    /// 연결 풀 활성화 여부
    #[arg(long = "use-connection-pool", default_value = "false")]
    pub use_connection_pool: bool,
}
