use crate::error::{Result, WmmoggError};
use crate::wem::Wem;
use ww2ogg::{CodebookLibrary, WwiseRiffVorbis};

/// Convert WEM bytes to OGG Vorbis bytes.
///
/// Auto-detects the codebook library by validating the decoded output.
/// SizeMismatch alone is not a reliable signal for Crimson Desert files —
/// they use aoTuV 6.03 codebooks but default conversion succeeds without error
/// while producing an undecodable stream.
///
/// The returned OGG has its Vorbis comment header replaced with a
/// WEM_ROUNDTRIP_V1 tag carrying the metadata needed by `ogg_to_wem`.
pub fn wem_to_ogg(wem_bytes: &[u8]) -> Result<Vec<u8>> {
    let wem = Wem::parse(wem_bytes)?;
    let ogg = convert_with_auto_detect(wem_bytes)?;
    let ogg = inject_roundtrip_comment(ogg, &wem, wem_bytes)?;
    Ok(ogg)
}

fn convert_with_auto_detect(wem_bytes: &[u8]) -> Result<Vec<u8>> {
    // Try default packed codebooks first.
    match try_convert(wem_bytes, CodebookLibrary::default_codebooks()?) {
        Err(WmmoggError::Forward(ww2ogg::WemError::SizeMismatch { .. })) => {
            log::debug!("default codebooks: size mismatch, trying aoTuV 6.03");
        }
        Err(e) => return Err(e),
        Ok(ogg) => {
            // Conversion succeeded structurally — validate the Vorbis bitstream.
            // Crimson Desert files use aoTuV codebooks; default conversion produces
            // a structurally valid OGG that lewton cannot decode.
            if ww2ogg::validate(&ogg).is_ok() {
                return Ok(ogg);
            }
            log::debug!("default codebooks produced undecodable stream, trying aoTuV 6.03");
        }
    }
    let ogg = try_convert(wem_bytes, CodebookLibrary::aotuv_codebooks()?)?;
    if let Err(e) = ww2ogg::validate(&ogg) {
        log::warn!("aoTuV codebooks also produced undecodable stream: {e}");
    }
    Ok(ogg)
}

fn try_convert(wem_bytes: &[u8], codebooks: CodebookLibrary) -> Result<Vec<u8>> {
    let cursor = std::io::Cursor::new(wem_bytes);
    let mut converter = WwiseRiffVorbis::new(cursor, codebooks)?;
    let mut out = Vec::new();
    converter.generate_ogg(&mut out)?;
    Ok(out)
}

/// Replace the ww2ogg placeholder Vorbis comment with a structured tag
/// that carries the WEM metadata needed for lossless reverse conversion.
fn inject_roundtrip_comment(ogg: Vec<u8>, wem: &Wem, wem_bytes: &[u8]) -> Result<Vec<u8>> {
    use crate::ogg_comment::{replace_comment_packet, build_roundtrip_comment};
    let comment = build_roundtrip_comment(wem, wem_bytes);
    replace_comment_packet(ogg, &comment)
}
