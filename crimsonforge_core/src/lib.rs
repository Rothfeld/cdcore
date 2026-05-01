pub mod archive;
pub mod compression;
pub mod crypto;
pub mod error;
pub mod ffi;
pub mod formats;
pub mod python;
pub mod repack;
pub mod vfs;

pub use error::{ParseError, Result};

// Re-export key types at crate root for convenience
pub use vfs::VfsManager;
pub use crypto::{pa_checksum, decrypt, encrypt, is_encrypted};
pub use compression::{decompress, compress_lz4, COMP_LZ4, COMP_NONE, COMP_ZLIB};
pub use archive::{parse_papgt, parse_pamt, PapgtData, PamtData, PamtFileEntry};

// PyO3 entry point — used by maturin to find the module init function
pub use python::crimsonforge_core;
