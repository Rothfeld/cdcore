mod error;
mod forward;
mod ogg_comment;
mod replace;
mod reverse;
mod wem;

pub use error::{Result, WmmoggError};

/// Convert Wwise Vorbis (.wem) bytes to standard Ogg Vorbis bytes.
///
/// Auto-detects the packed codebook library.
/// The output OGG contains a WEM_ROUNDTRIP_V1 Vorbis comment carrying
/// the metadata needed by `ogg_to_wem`.
pub fn wem_to_ogg(wem_bytes: &[u8]) -> Result<Vec<u8>> {
    forward::wem_to_ogg(wem_bytes)
}

/// Convert an Ogg Vorbis produced by `wem_to_ogg` back to WEM bytes.
///
/// The input must contain a WEM_ROUNDTRIP_V1 comment; otherwise returns an error.
pub fn ogg_to_wem(ogg_bytes: &[u8]) -> Result<Vec<u8>> {
    reverse::ogg_to_wem(ogg_bytes)
}

/// Replace the audio in an existing WEM with audio from a standard Vorbis OGG.
///
/// `original_wem` provides the structural template (fmt chunk, extra chunks).
/// `new_ogg` provides the replacement audio.
///
/// # Hard constraints
///
/// - Channel count must exactly match the original WEM.
/// - Sample rate must exactly match the original WEM.
///
/// Violating either returns a clear error. No silent corruption.
///
/// # Codebook requirement
///
/// The new OGG must use standard libvorbis or aoTuV 6.03 codebooks.
/// An OGG produced by `ffmpeg -c:a libvorbis` satisfies this.
pub fn replace_wem_audio(original_wem: &[u8], new_ogg: &[u8]) -> Result<Vec<u8>> {
    replace::replace_wem_audio(original_wem, new_ogg)
}
