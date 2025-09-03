use thiserror::Error;

#[derive(Debug, Error)]
pub enum ProxyError {
    #[error(transparent)]
    Io(#[from] tokio::io::Error),

    #[error("Network error: {0}")]
    Network(String),

    #[error("Parsing error: {0}")]
    Parse(String),

    #[error("Other: {0}")]
    Msg(String),
}

impl From<&str> for ProxyError {
    fn from(s: &str) -> Self {
        ProxyError::Msg(s.to_string())
    }
}

impl From<String> for ProxyError {
    fn from(s: String) -> Self {
        ProxyError::Msg(s)
    }
}

pub type ProxyResult<T> = std::result::Result<T, ProxyError>;
