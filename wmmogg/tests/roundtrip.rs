/// End-to-end tests for wem_to_ogg, ogg_to_wem, and replace_wem_audio.
///
/// Run with:
///   cargo test --manifest-path crates/wmmogg/Cargo.toml --tests -- --nocapture

use std::path::{Path, PathBuf};

fn corpus_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus")
}

fn corpus_wems() -> Vec<PathBuf> {
    let mut files: Vec<_> = std::fs::read_dir(corpus_dir())
        .expect("corpus dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("wem"))
        .collect();
    files.sort();
    files
}

#[test]
fn forward_all_corpus() {
    let _ = env_logger::try_init();
    let wems = corpus_wems();
    assert!(!wems.is_empty(), "no .wem files in corpus/");

    let mut pass = 0;
    let mut fail = 0;
    for path in &wems {
        let data = std::fs::read(path).expect("read wem");
        match wmmogg::wem_to_ogg(&data) {
            Ok(ogg) => {
                assert!(!ogg.is_empty(), "empty ogg for {}", path.display());
                assert!(&ogg[..4] == b"OggS", "not OggS for {}", path.display());
                println!("PASS forward {}: {} → {} bytes", path.file_name().unwrap().to_string_lossy(), data.len(), ogg.len());
                pass += 1;
            }
            Err(e) => {
                eprintln!("FAIL forward {}: {e}", path.file_name().unwrap().to_string_lossy());
                fail += 1;
            }
        }
    }
    println!("\nForward: {pass} pass, {fail} fail / {} total", wems.len());
    assert_eq!(fail, 0, "{fail} forward conversions failed");
}

#[test]
fn roundtrip_all_corpus() {
    let _ = env_logger::try_init();
    let wems = corpus_wems();
    assert!(!wems.is_empty(), "no .wem files in corpus/");

    let mut pass = 0;
    let mut fail = 0;
    let mut diverge = 0;

    for path in &wems {
        let original = std::fs::read(path).expect("read wem");

        let ogg = match wmmogg::wem_to_ogg(&original) {
            Ok(o) => o,
            Err(e) => {
                eprintln!("FAIL forward {}: {e}", path.file_name().unwrap().to_string_lossy());
                fail += 1;
                continue;
            }
        };

        let rebuilt = match wmmogg::ogg_to_wem(&ogg) {
            Ok(w) => w,
            Err(e) => {
                eprintln!("FAIL reverse {}: {e}", path.file_name().unwrap().to_string_lossy());
                fail += 1;
                continue;
            }
        };

        if original == rebuilt {
            println!("PASS roundtrip {}", path.file_name().unwrap().to_string_lossy());
            pass += 1;
        } else {
            let first_diff = original.iter().zip(rebuilt.iter())
                .enumerate()
                .find(|(_, (a, b))| a != b)
                .map(|(i, _)| i);
            eprintln!(
                "DIVERGE {} original={} rebuilt={} first_diff={:?}",
                path.file_name().unwrap().to_string_lossy(),
                original.len(), rebuilt.len(), first_diff
            );
            diverge += 1;
        }
    }

    println!("\nRoundtrip: {pass} pass, {fail} error, {diverge} diverge / {} total", wems.len());
    assert_eq!(fail + diverge, 0, "{fail} errors + {diverge} divergences");
}

#[test]
fn forward_output_is_decodable() {
    // Use ww2ogg's validate() to confirm the OGG is a well-formed Vorbis stream.
    let _ = env_logger::try_init();
    let wems = corpus_wems();
    let mut pass = 0;
    let mut fail = 0;
    for path in &wems {
        let data = std::fs::read(path).expect("read wem");
        let ogg = wmmogg::wem_to_ogg(&data).expect("forward");
        match ww2ogg::validate(&ogg) {
            Ok(()) => { pass += 1; }
            Err(e) => {
                eprintln!("INVALID {}: {e}", path.file_name().unwrap().to_string_lossy());
                fail += 1;
            }
        }
    }
    println!("validate: {pass} ok, {fail} invalid / {} total", wems.len());
    assert_eq!(fail, 0);
}

#[test]
fn ffmpeg_can_decode_output() {
    let _ = env_logger::try_init();
    let corpus = corpus_dir();
    let path = corpus.join("388161812.wem");
    let data = std::fs::read(path).expect("read wem");
    let ogg = wmmogg::wem_to_ogg(&data).expect("forward");

    let tmp = std::env::temp_dir().join("wmmogg_test.ogg");
    std::fs::write(&tmp, &ogg).expect("write tmp");

    let out = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", tmp.to_str().unwrap(),
               "-f", "null", "-"])
        .output()
        .expect("ffmpeg");

    eprintln!("ffmpeg exit: {}", out.status);
    let stderr = String::from_utf8_lossy(&out.stderr);
    // Look for duration and stream info lines
    for line in stderr.lines() {
        if line.contains("Duration") || line.contains("Stream") || line.contains("Error") || line.contains("Invalid") {
            eprintln!("{line}");
        }
    }
    let _ = std::fs::remove_file(&tmp);
    assert!(out.status.success(), "ffmpeg failed to decode OGG");
}

#[test]
fn output_is_non_silent() {
    let _ = env_logger::try_init();
    let corpus = corpus_dir();
    let path = corpus.join("388161812.wem");
    let data = std::fs::read(path).expect("read wem");
    let ogg = wmmogg::wem_to_ogg(&data).expect("forward");

    let tmp = std::env::temp_dir().join("wmmogg_silence_test.ogg");
    std::fs::write(&tmp, &ogg).expect("write");

    // Decode to raw s16le and check max amplitude
    let out = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", tmp.to_str().unwrap(),
               "-f", "s16le", "-ac", "1", "-ar", "48000",
               "/tmp/wmmogg_pcm.raw"])
        .output()
        .expect("ffmpeg decode");
    assert!(out.status.success(), "ffmpeg decode failed: {}", String::from_utf8_lossy(&out.stderr));

    let pcm = std::fs::read("/tmp/wmmogg_pcm.raw").expect("read pcm");
    let samples: Vec<i16> = pcm.chunks_exact(2)
        .map(|c| i16::from_le_bytes([c[0], c[1]]))
        .collect();
    let max_amp = samples.iter().map(|&s| s.unsigned_abs()).max().unwrap_or(0);
    eprintln!("samples={} max_amplitude={max_amp}", samples.len());

    let _ = std::fs::remove_file(&tmp);
    let _ = std::fs::remove_file("/tmp/wmmogg_pcm.raw");

    assert!(max_amp > 100, "audio appears silent (max_amp={max_amp})");
}

#[test]
fn replace_wem_audio_produces_playable_output() {
    // Workflow: WEM -> OGG -> re-encode with ffmpeg (libvorbis) -> replace_wem_audio -> new WEM
    // Simulates replacing game audio with a user-supplied recording.
    let _ = env_logger::try_init();
    let corpus = corpus_dir();
    let path = corpus.join("388161812.wem");
    let wem_bytes = std::fs::read(&path).expect("read wem");

    // Forward: WEM -> OGG
    let ogg = wmmogg::wem_to_ogg(&wem_bytes).expect("wem_to_ogg");

    // Re-encode the OGG with ffmpeg -c:a libvorbis (standard codebooks).
    // This simulates a user recording encoded by a standard Vorbis encoder.
    let tmp_in  = std::env::temp_dir().join("wmmogg_replace_in.ogg");
    let tmp_out = std::env::temp_dir().join("wmmogg_replace_reencoded.ogg");
    std::fs::write(&tmp_in, &ogg).expect("write tmp_in");

    let status = std::process::Command::new("ffmpeg")
        .args(["-y", "-i", tmp_in.to_str().unwrap(),
               "-c:a", "libvorbis", "-q:a", "6",
               tmp_out.to_str().unwrap()])
        .output()
        .expect("ffmpeg");
    assert!(status.status.success(), "ffmpeg re-encode failed: {}",
            String::from_utf8_lossy(&status.stderr));

    let new_ogg = std::fs::read(&tmp_out).expect("read reencoded ogg");

    // Replace: original WEM + new OGG -> new WEM
    let new_wem = wmmogg::replace_wem_audio(&wem_bytes, &new_ogg)
        .expect("replace_wem_audio");

    assert!(!new_wem.is_empty());
    assert_eq!(&new_wem[..4], b"RIFF");

    // Verify new WEM decodes correctly via roundtrip.
    let new_ogg2 = wmmogg::wem_to_ogg(&new_wem).expect("wem_to_ogg on new wem");
    match ww2ogg::validate(&new_ogg2) {
        Ok(()) => eprintln!("replace_wem_audio: output validates OK"),
        Err(e) => panic!("replace_wem_audio output failed validation: {e}"),
    }

    let _ = std::fs::remove_file(&tmp_in);
    let _ = std::fs::remove_file(&tmp_out);
    eprintln!("original WEM: {} bytes  ->  new WEM: {} bytes", wem_bytes.len(), new_wem.len());
}
