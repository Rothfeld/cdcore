//! cdml-index: walk a folder of images, embed each with CLIP, persist the index.
//!
//! Example:
//!   cdml-index --batch 32 --images ./screenshots --out ./cdml_index

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use walkdir::WalkDir;

use cdml::{Clip, ClipVariant, IndexWriter};

#[derive(Debug, Parser)]
#[command(about = "Embed a folder of images with CLIP and write a cdml index")]
struct Args {
    #[arg(long)]
    images: PathBuf,
    #[arg(long)]
    out: PathBuf,
    #[arg(long, default_value = "base32")]
    variant: ClipVariant,
    #[arg(long, default_value_t = 16)]
    batch: usize,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let device = Clip::best_device();
    log::info!("loading clip {:?} on {:?}", args.variant, device);
    let clip = Clip::load(args.variant, device)
        .context("failed to load clip model")?;
    log::info!(
        "model ready: image_size={}, embed_dim={}",
        args.variant.image_size(),
        args.variant.embed_dim()
    );

    let images = collect_images(&args.images);
    assert!(!images.is_empty(), "no images found under {}", args.images.display());
    log::info!("found {} images under {}", images.len(), args.images.display());

    let mut writer = IndexWriter::create(&args.out, args.variant.embed_dim())?;
    let total = images.len();
    let start = Instant::now();
    let mut done = 0usize;

    for chunk in images.chunks(args.batch) {
        let chunk_paths: Vec<PathBuf> = chunk.to_vec();
        let vecs = clip.embed_images(&chunk_paths)
            .with_context(|| format!("encode batch failed: {chunk_paths:?}"))?;
        for (v, p) in vecs.iter().zip(chunk_paths.iter()) {
            writer.push(v, &p.to_string_lossy())?;
        }
        done += chunk.len();
        let elapsed = start.elapsed().as_secs_f32();
        let rate = done as f32 / elapsed.max(1e-3);
        log::info!(
            "encoded {}/{} ({:.1} img/s, eta {:.0}s)",
            done, total, rate,
            (total - done) as f32 / rate.max(1e-3)
        );
    }

    let (bin, paths, count) = writer.finish()?;
    log::info!(
        "wrote {} embeddings to {} ({}) in {:.1}s",
        count, bin.display(), paths.display(),
        start.elapsed().as_secs_f32()
    );
    Ok(())
}

fn collect_images(root: &Path) -> Vec<PathBuf> {
    const EXTS: &[&str] = &["jpg", "jpeg", "png", "bmp", "webp", "tiff"];
    WalkDir::new(root)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| {
            p.extension()
                .and_then(|s| s.to_str())
                .map(|s| EXTS.iter().any(|ext| ext.eq_ignore_ascii_case(s)))
                .unwrap_or(false)
        })
        .collect()
}
