use thiserror::Error;

#[derive(Debug, Error)]
pub enum CdmlError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("hf-hub: {0}")]
    HfHub(#[from] hf_hub::api::sync::ApiError),

    #[error("candle: {0}")]
    Candle(#[from] candle_core::Error),

    #[error("tokenizer: {0}")]
    Tokenizer(String),

    #[error("image: {0}")]
    Image(#[from] image::ImageError),

    #[error("index format: {0}")]
    Index(String),

    #[error("dim mismatch: index has {index_dim}, query produced {query_dim}")]
    DimMismatch { index_dim: usize, query_dim: usize },
}

impl From<tokenizers::Error> for CdmlError {
    fn from(e: tokenizers::Error) -> Self {
        CdmlError::Tokenizer(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, CdmlError>;
