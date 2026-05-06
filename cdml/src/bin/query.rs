//! cdml-query: text -> CLIP -> top-k nearest images in an existing index.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;

use cdml::{topk, Clip, ClipVariant, IndexReader};

#[derive(Debug, Parser)]
#[command(about = "Text-to-image search over a cdml index")]
struct Args {
    #[arg(long)]
    index: PathBuf,
    #[arg(long, default_value = "base32")]
    variant: ClipVariant,
    #[arg(long, default_value_t = 10)]
    k: usize,
    #[arg(long)]
    json: bool,
    query: String,
}

fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let reader = IndexReader::open(&args.index)
        .with_context(|| format!("opening index at {}", args.index.display()))?;
    log::info!("index: dim={}, count={}", reader.dim, reader.count);
    assert_eq!(
        reader.dim,
        args.variant.embed_dim(),
        "variant {:?} dim {} != index dim {}",
        args.variant, args.variant.embed_dim(), reader.dim,
    );

    let device = Clip::best_device();
    log::info!("loading clip {:?} on {:?}", args.variant, device);
    let clip = Clip::load(args.variant, device)?;

    let t0 = Instant::now();
    let q = clip.embed_text(&args.query)?;
    log::info!("text encode: {:.1} ms", t0.elapsed().as_secs_f32() * 1000.0);

    let t1 = Instant::now();
    let hits = topk(&reader, &q, args.k)?;
    log::info!(
        "search over {} vectors: {:.1} ms",
        reader.count,
        t1.elapsed().as_secs_f32() * 1000.0
    );

    if args.json {
        println!("{}", serde_json::to_string_pretty(&hits)?);
    } else {
        for (rank, h) in hits.iter().enumerate() {
            println!("{:>3}  {:.4}  {}", rank + 1, h.score, h.path);
        }
    }
    Ok(())
}
