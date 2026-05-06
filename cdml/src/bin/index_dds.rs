//! cdml-index-dds: walk every DDS in the Crimson Desert VFS, encode with CLIP, write a cdml index.
//!
//! Path stored in the index is the in-game VFS path (e.g.
//! `character/cha00100/texture/cha00100_d.dds`), not a host filesystem path.
//!
//! Example:
//!   cdml-index-dds --game /cd --out ./cdml_dds --batch 16

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use rayon::prelude::*;

use cdcore::formats::image::dds::decode_dds_to_rgba;
use cdcore::VfsManager;
use cdml::{Clip, ClipVariant, IndexWriter};

#[derive(Debug, Parser)]
#[command(about = "Build a cdml index over every DDS texture in the Crimson Desert VFS")]
struct Args {
    /// Path to the Crimson Desert packages directory (default: /cd).
    #[arg(long, default_value = "/cd")]
    game: PathBuf,
    /// Output dir for embeddings.bin + paths.txt.
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value = "base32")]
    variant: ClipVariant,
    /// Encode batch size (CLIP forward call). 256-512 is a sweet spot on a 96 GB GPU.
    #[arg(long, default_value_t = 256)]
    batch: usize,
    /// Stop after this many DDS entries (useful for sanity runs). 0 = no limit.
    #[arg(long, default_value_t = 0)]
    limit: usize,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    log::info!("opening VFS at {}", args.game.display());
    let vfs = VfsManager::new(args.game.to_str().context("non-utf8 game path")?)
        .context("VfsManager::new failed")?;
    vfs.load_all_groups().context("load_all_groups failed")?;

    log::info!("scanning for .dds entries");
    let mut entries: Vec<_> = vfs
        .search(".dds")
        .into_iter()
        .filter(|e| e.path.to_lowercase().ends_with(".dds"))
        .collect();
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    if args.limit > 0 && entries.len() > args.limit {
        entries.truncate(args.limit);
    }
    log::info!("{} DDS entries to encode", entries.len());
    assert!(!entries.is_empty(), "no DDS entries found in VFS");

    let device = Clip::best_device();
    log::info!("loading clip {:?} on {:?}", args.variant, device);
    let clip = Clip::load(args.variant, device).context("Clip::load failed")?;
    log::info!(
        "model ready: image_size={}, embed_dim={}",
        args.variant.image_size(),
        args.variant.embed_dim()
    );

    let mut writer = IndexWriter::create(&args.out, args.variant.embed_dim())?;
    let total = entries.len();
    let start = Instant::now();
    let mut done = 0usize;
    let mut decode_failures = 0usize;

    for chunk in entries.chunks(args.batch) {
        // Parallel CPU pipeline: VFS read (mmap + ChaCha20 + LZ4) + DDS decode.
        // Returns Some((rgba, w, h, path)) on success; None means decode failure
        // already logged. We then partition into the GPU batch on the main thread.
        let decoded: Vec<Option<(Vec<u8>, u32, u32, String)>> = chunk
            .par_iter()
            .map(|entry| {
                let bytes = match vfs.read_entry(entry) {
                    Ok(b) => b,
                    Err(e) => {
                        log::warn!("read_entry failed for {}: {}", entry.path, e);
                        return None;
                    }
                };
                match decode_dds_to_rgba(&bytes) {
                    Ok((w, h, rgba)) => Some((rgba, w, h, entry.path.clone())),
                    Err(e) => {
                        log::warn!("decode_dds_to_rgba failed for {}: {}", entry.path, e);
                        None
                    }
                }
            })
            .collect();

        let mut items: Vec<(Vec<u8>, u32, u32)> = Vec::with_capacity(chunk.len());
        let mut paths: Vec<String> = Vec::with_capacity(chunk.len());
        for d in decoded {
            match d {
                Some((rgba, w, h, p)) => {
                    items.push((rgba, w, h));
                    paths.push(p);
                }
                None => decode_failures += 1,
            }
        }
        if items.is_empty() {
            done += chunk.len();
            continue;
        }
        let vecs = clip.embed_rgba_batch(&items)
            .with_context(|| format!("embed_rgba_batch failed (paths: {paths:?})"))?;
        for (v, p) in vecs.iter().zip(paths.iter()) {
            writer.push(v, p)?;
        }
        done += chunk.len();
        let elapsed = start.elapsed().as_secs_f32();
        let rate = done as f32 / elapsed.max(1e-3);
        log::info!(
            "encoded {}/{} ({:.1} dds/s, eta {:.0}s, decode-failures {})",
            done, total, rate,
            (total - done) as f32 / rate.max(1e-3),
            decode_failures
        );
    }

    let (bin, paths_path, count) = writer.finish()?;
    log::info!(
        "wrote {} embeddings ({} decode-failures) to {} ({}) in {:.1}s",
        count, decode_failures, bin.display(), paths_path.display(),
        start.elapsed().as_secs_f32()
    );
    Ok(())
}
