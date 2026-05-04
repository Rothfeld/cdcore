pub mod archive;
pub mod compression;
pub mod crypto;
pub mod error;
pub mod formats;
pub mod repack;
pub mod vfs;

#[cfg(feature = "python")]
pub mod python;

pub use error::{ParseError, Result};

// Re-export key types at crate root for convenience
pub use vfs::VfsManager;
pub use crypto::{pa_checksum, decrypt, encrypt, is_encrypted};
pub use compression::{decompress, compress_lz4, COMP_LZ4, COMP_NONE, COMP_ZLIB};
pub use archive::{parse_papgt, parse_pamt, PapgtData, PamtData, PamtFileEntry};

// PyO3 entry point -- used by maturin to find the module init function
#[cfg(feature = "python")]
pub use python::cdcore;
