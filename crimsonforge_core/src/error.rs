use thiserror::Error;

#[derive(Debug, Error)]
pub enum ParseError {
    #[error("invalid magic at offset {offset:#x}: expected {expected:?}, found {found:?}")]
    InvalidMagic {
        expected: Vec<u8>,
        found: Vec<u8>,
        offset: usize,
    },

    #[error("unexpected EOF at offset {offset:#x}: needed {needed} bytes, have {have}")]
    UnexpectedEof {
        offset: usize,
        needed: usize,
        have: usize,
    },

    #[error("invalid {field} encoding at offset {offset:#x}")]
    InvalidEncoding { field: &'static str, offset: usize },

    #[error("checksum mismatch: computed {computed:#010x}, stored {stored:#010x}")]
    ChecksumMismatch { computed: u32, stored: u32 },

    #[error("unsupported version {version:#x}")]
    UnsupportedVersion { version: u32 },

    #[error("compression error: {0}")]
    Compression(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, ParseError>;

impl ParseError {
    pub fn eof(offset: usize, needed: usize, have: usize) -> Self {
        Self::UnexpectedEof { offset, needed, have }
    }

    pub fn magic(expected: &[u8], found: &[u8], offset: usize) -> Self {
        Self::InvalidMagic {
            expected: expected.to_vec(),
            found: found.to_vec(),
            offset,
        }
    }
}

pub(crate) fn read_u8(data: &[u8], offset: usize) -> Result<u8> {
    data.get(offset)
        .copied()
        .ok_or_else(|| ParseError::eof(offset, 1, data.len().saturating_sub(offset)))
}

pub(crate) fn read_u16_le(data: &[u8], offset: usize) -> Result<u16> {
    data.get(offset..offset + 2)
        .and_then(|b| b.try_into().ok())
        .map(u16::from_le_bytes)
        .ok_or_else(|| ParseError::eof(offset, 2, data.len().saturating_sub(offset)))
}

pub(crate) fn read_u32_le(data: &[u8], offset: usize) -> Result<u32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or_else(|| ParseError::eof(offset, 4, data.len().saturating_sub(offset)))
}

pub(crate) fn read_i32_le(data: &[u8], offset: usize) -> Result<i32> {
    data.get(offset..offset + 4)
        .and_then(|b| b.try_into().ok())
        .map(i32::from_le_bytes)
        .ok_or_else(|| ParseError::eof(offset, 4, data.len().saturating_sub(offset)))
}

pub(crate) fn read_f32_le(data: &[u8], offset: usize) -> Result<f32> {
    read_u32_le(data, offset).map(f32::from_bits)
}


