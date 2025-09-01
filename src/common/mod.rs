//! 공통 모듈
//! 
//! 프로젝트 전체에서 사용되는 공통 타입, 에러, 상수 등을 정의합니다.

pub mod types;
pub mod errors;

// 공통 타입들을 재내보내기
pub use types::*;
pub use errors::*;
