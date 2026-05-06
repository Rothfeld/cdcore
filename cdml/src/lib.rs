//! cdml -- CLIP embedding + brute-force vector search.
//!
//! Pipeline:
//!   1. fetch openai/clip-vit-base-patch32 weights from HuggingFace
//!   2. encode images / text via candle-transformers::models::clip (fp32, CPU)
//!   3. persist as raw f32 little-endian "embeddings.bin" + "paths.txt"
//!   4. query: text -> embedding -> simsimd cosine vs full corpus -> top-k

pub mod error;
pub mod index;
pub mod model;
pub mod preprocess;
pub mod search;

pub use error::{CdmlError, Result};
pub use index::{IndexReader, IndexWriter};
pub use model::{Clip, ClipVariant};
pub use search::{topk, ScoredHit};
