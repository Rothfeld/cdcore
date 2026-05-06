//! Brute-force top-k cosine similarity over an `IndexReader`.
//!
//! All vectors are L2-normalized at index time, so cosine reduces to dot product.
//! simsimd dispatches to AVX2/AVX-512/NEON automatically.

use serde::Serialize;
use simsimd::SpatialSimilarity;

use crate::error::{CdmlError, Result};
use crate::index::IndexReader;

#[derive(Debug, Clone, Serialize)]
pub struct ScoredHit {
    pub index: usize,
    pub score: f32,
    pub path: String,
}

/// Return up to `k` highest-scoring rows (cosine similarity, descending).
///
/// `query` must already be L2-normalized.
pub fn topk(reader: &IndexReader, query: &[f32], k: usize) -> Result<Vec<ScoredHit>> {
    if query.len() != reader.dim {
        return Err(CdmlError::DimMismatch {
            index_dim: reader.dim,
            query_dim: query.len(),
        });
    }
    if reader.count == 0 || k == 0 {
        return Ok(vec![]);
    }

    let dim = reader.dim;
    let vecs = reader.vectors();

    // simsimd::cosine returns *distance* in [0, 2]. Convert to similarity = 1 - dist.
    let mut scored: Vec<(f32, usize)> = (0..reader.count)
        .map(|i| {
            let row = &vecs[i * dim..(i + 1) * dim];
            let dist = f32::cosine(query, row).expect("matched dims");
            (1.0 - dist as f32, i)
        })
        .collect();

    // Partial sort: only need the top k.
    let take = k.min(scored.len());
    scored.select_nth_unstable_by(take.saturating_sub(1), |a, b| {
        b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(take);
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

    Ok(scored
        .into_iter()
        .map(|(score, index)| ScoredHit {
            index,
            score,
            path: reader.paths[index].clone(),
        })
        .collect())
}
