use thiserror::Error;

#[derive(Debug, Error)]
pub enum WmmoggError {
    #[error("wem parse error: {0}")]
    WemParse(String),

    #[error("ogg parse error: {0}")]
    OggParse(String),

    #[error("vorbis parse error: {0}")]
    VorbisParse(String),

    #[error("codebook not found in packed library (inline codebooks not yet supported)")]
    CodebookNotFound,

    #[error("unsupported wem variant: {0}")]
    UnsupportedVariant(String),

    #[error("forward conversion failed: {0}")]
    Forward(#[from] ww2ogg::WemError),

    #[error("i/o error: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, WmmoggError>;
