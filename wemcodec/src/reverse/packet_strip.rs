/// Standard Vorbis audio packet → Wwise modified audio packet.
///
/// ww2ogg's forward path transforms Wwise packets into standard Vorbis:
///   1. Reads `ilog(mode_count-1)` mode bits from the front of the packet.
///   2. If mode's blockflag==1 (long window), reads and discards 2 window bits
///      (prev_window, next_window).
///   3. Prepends packet_type bit (0) to the standard packet.
///
/// Reverse:
///   1. Strip the leading 0 bit (packet_type).
///   2. Read `ilog(mode_count-1)` bits → mode index.
///   3. If mode's blockflag==1: re-insert 2 window bits at their position.
///      (We use 0 for both; the game re-derives them from context.)
///   4. Move the mode bits to the front (they're already there after stripping
///      the type bit — nothing else to do).

use crate::error::{Result, WmmoggError};
use super::bit_io::{BitReader, BitWriter};

/// Mode table entry — only blockflag matters here.
#[derive(Clone, Copy)]
pub struct Mode {
    pub blockflag: bool,
}

/// Strip a standard Vorbis audio packet to Wwise form.
///
/// `modes` is the mode table from the setup header.
pub fn strip_audio_packet(packet: &[u8], modes: &[Mode]) -> Result<Vec<u8>> {
    if packet.is_empty() {
        return Err(WmmoggError::VorbisParse("empty audio packet".into()));
    }

    let mut r = BitReader::new(packet);
    let mut w = BitWriter::new();

    // Standard Vorbis audio packet starts with packet_type=0 (1 bit).
    let packet_type = r.read_bit()?;
    if packet_type {
        return Err(WmmoggError::VorbisParse("audio packet has non-zero type bit".into()));
    }
    // Wwise drops this bit — don't write it.

    // Mode index: ilog(mode_count - 1) bits.
    let mode_bits = ilog(modes.len() as u32 - 1) as u8;
    let mode_idx  = r.read_bits(mode_bits)? as usize;
    if mode_idx >= modes.len() {
        return Err(WmmoggError::VorbisParse(
            format!("mode index {mode_idx} >= mode count {}", modes.len())
        ));
    }
    // Write mode bits first in the Wwise packet.
    w.write_bits(mode_idx as u32, mode_bits);

    if modes[mode_idx].blockflag {
        // Standard Vorbis long-window: prev_window and next_window bits here.
        // Wwise drops them; consume and discard.
        let _prev_window = r.read_bit()?;
        let _next_window = r.read_bit()?;
    }

    // Copy exactly (original_size * 8 - mode_bits) rest bits.
    // The expanded OGG packet is always 1 byte larger than the original Wwise
    // packet (1 type bit pushes the last content bit into an extra byte).
    // Without this truncation, the trailing zero-padding byte would be carried
    // through, making the rebuilt packet 1 byte too large.
    //
    // original_size = packet.len() - 1  (expanded is always 1 byte larger)
    let original_bits = (packet.len() - 1) * 8;
    let rest_bits = original_bits.saturating_sub(mode_bits as usize);
    for _ in 0..rest_bits {
        w.write_bit(r.read_bit()?);
    }

    Ok(w.finish())
}

fn ilog(v: u32) -> u32 {
    if v == 0 { 0 } else { 32 - v.leading_zeros() }
}
