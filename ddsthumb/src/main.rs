use std::fs;
use std::io::BufWriter;
use std::path::{Path, PathBuf};

use clap::Parser;
use walkdir::WalkDir;

use crimsonforge_core::formats::dds::decode_dds_to_rgba;

#[derive(Parser)]
#[command(name = "ddsthumb", about = "Generate PNG thumbnails from DDS files")]
struct Args {
    /// Input .dds file or directory (scanned recursively)
    input: String,

    /// Output directory for PNG thumbnails
    output: String,

    /// Thumbnail size in pixels (width = height)
    #[arg(long, default_value = "128")]
    size: u32,
}

fn main() {
    let args = Args::parse();
    let input = PathBuf::from(&args.input);
    let out_dir = PathBuf::from(&args.output);
    fs::create_dir_all(&out_dir).expect("failed to create output directory");

    let files: Vec<PathBuf> = if input.is_file() {
        vec![input.clone()]
    } else {
        WalkDir::new(&input)
            .into_iter()
            .filter_map(|e| e.ok())
            .filter(|e| e.path().extension().map(|x| x.eq_ignore_ascii_case("dds")).unwrap_or(false))
            .map(|e| e.into_path())
            .collect()
    };

    let total = files.len();
    let mut errors = 0usize;

    eprintln!("Found {total} DDS files — generating {}px thumbnails ...", args.size);

    for (n, path) in files.iter().enumerate() {
        let rel  = path.strip_prefix(&input).unwrap_or(path);
        let dest = out_dir.join(rel).with_extension("png");

        let result = (|| -> Result<(), String> {
            let data = fs::read(path).map_err(|e| e.to_string())?;
            let (w, h, rgba) = decode_dds_to_rgba(&data).map_err(|e| e.to_string())?;
            let thumb = thumbnail(&rgba, w, h, args.size);
            if let Some(p) = dest.parent() { fs::create_dir_all(p).map_err(|e| e.to_string())?; }
            write_png(&dest, &thumb, args.size)
        })();

        if let Err(e) = result {
            eprintln!("error: {}: {e}", path.display());
            errors += 1;
        }
        let n = n + 1;
        if n % 1000 == 0 || n == total {
            eprintln!("  {n}/{total}  errors={errors}");
        }
    }

    eprintln!("Done. {} written, {} errors.", total - errors, errors);
}

fn thumbnail(rgba: &[u8], sw: u32, sh: u32, size: u32) -> Vec<u8> {
    let mut out = vec![0u8; (size * size * 4) as usize];
    for dy in 0..size {
        for dx in 0..size {
            let sx = (dx * sw / size).min(sw.saturating_sub(1));
            let sy = (dy * sh / size).min(sh.saturating_sub(1));
            let sp = ((sy * sw + sx) * 4) as usize;
            let dp = ((dy * size + dx) * 4) as usize;
            if sp + 4 <= rgba.len() {
                out[dp..dp + 4].copy_from_slice(&rgba[sp..sp + 4]);
            }
        }
    }
    out
}

fn write_png(path: &Path, rgba: &[u8], size: u32) -> Result<(), String> {
    let file = fs::File::create(path).map_err(|e| e.to_string())?;
    let mut enc = png::Encoder::new(BufWriter::new(file), size, size);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()
        .and_then(|mut w| w.write_image_data(rgba))
        .map_err(|e| e.to_string())
}
