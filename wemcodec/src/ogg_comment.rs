/// OGG Vorbis comment packet replacement.
///
/// The Vorbis comment is the second logical packet.  ww2ogg writes a single
/// placeholder comment ("made with ww2ogg" or similar).  We replace it with
/// our own structured tag so the reverse path has all the info it needs.
///
/// OGG page layout:
///   - page 0: ID header (first=true, packet_type=0x01)
///   - page 1: comment header (packet_type=0x03) — may span multiple pages
///   - page 2+: setup header, then audio
///
/// We walk pages until we find the comment header, rebuild that page(s) with
/// the new comment bytes, then copy the rest verbatim.  CRCs are recomputed.

use crate::error::{Result, WmmoggError};
use crate::wem::{PacketHeaderFormat, Wem};

/// Build the WEM_ROUNDTRIP_V1 comment string.
///
/// Stores all metadata needed to reconstruct the original WEM byte-for-byte:
/// - The complete WEM file header (all bytes before data chunk content)
/// - The preamble bytes before the setup packet (seek table)
/// - The original stripped setup packet bytes (with 2-byte size header)
/// - Audio packet format and position parameters
pub fn build_roundtrip_comment(wem: &Wem, wem_bytes: &[u8]) -> Vec<u8> {
    let pkt_hdr = match wem.packet_fmt {
        PacketHeaderFormat::TwoByte   => "2",
        PacketHeaderFormat::SixByte   => "6",
        PacketHeaderFormat::EightByte => "8",
    };

    let setup_off = wem.fmt.setup_packet_offset as usize;
    let audio_off = wem.fmt.first_audio_packet_offset as usize;

    // Find the data chunk offset in the original file.
    let data_content_offset = find_data_content_offset(wem_bytes)
        .expect("wem must have a data chunk");

    // WEM header: everything up to (and including) the data chunk header (8 bytes).
    let wem_header = &wem_bytes[..data_content_offset];

    // Preamble: bytes before setup packet in data chunk.
    let preamble = &wem.data[..setup_off];
    // Setup packet: 2-byte size header + data.
    let setup_size = u16::from_le_bytes(wem.data[setup_off..setup_off+2].try_into().unwrap()) as usize;
    let setup_packet_with_header = &wem.data[setup_off..setup_off+2+setup_size];

    let tag = format!(
        "WEM_ROUNDTRIP_V1=packet_header={pkt_hdr};\
         channels={ch};sample_rate={sr};sample_count={sc};\
         blocksize_0={b0};blocksize_1={b1};\
         setup_offset={soff};audio_offset={aoff};\
         header={header_hex};preamble={pre_hex};setup={setup_hex}",
        ch         = wem.fmt.channels,
        sr         = wem.fmt.sample_rate,
        sc         = wem.fmt.sample_count,
        b0         = wem.fmt.blocksize_0_exp,
        b1         = wem.fmt.blocksize_1_exp,
        soff       = setup_off,
        aoff       = audio_off,
        header_hex = hex_encode(wem_header),
        pre_hex    = hex_encode(preamble),
        setup_hex  = hex_encode(setup_packet_with_header),
    );

    build_vorbis_comment_packet("wemcodec", &[tag.as_str()])
}

/// Find the byte offset of the data chunk's content (after the 8-byte id+size header).
fn find_data_content_offset(wem_bytes: &[u8]) -> Option<usize> {
    let mut pos = 12usize;
    while pos + 8 <= wem_bytes.len() {
        let id = &wem_bytes[pos..pos+4];
        let sz = u32::from_le_bytes(wem_bytes[pos+4..pos+8].try_into().ok()?) as usize;
        if id == b"data" {
            return Some(pos + 8);
        }
        pos += 8 + sz + (sz & 1);
    }
    None
}

fn hex_encode(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()/2)
        .map(|i| u8::from_str_radix(&s[2*i..2*i+2], 16).ok())
        .collect()
}

/// Serialize a Vorbis comment packet (packet_type=0x03).
///
/// The Vorbis spec requires a framing bit (LSB=1) as the last bit of the packet.
/// Without it, strict decoders (lewton) reject the comment header.
fn build_vorbis_comment_packet(vendor: &str, comments: &[&str]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(0x03);
    out.extend_from_slice(b"vorbis");
    let vb = vendor.as_bytes();
    out.extend_from_slice(&(vb.len() as u32).to_le_bytes());
    out.extend_from_slice(vb);
    out.extend_from_slice(&(comments.len() as u32).to_le_bytes());
    for c in comments {
        let cb = c.as_bytes();
        out.extend_from_slice(&(cb.len() as u32).to_le_bytes());
        out.extend_from_slice(cb);
    }
    out.push(0x01); // framing bit: LSB must be 1 per Vorbis spec §5.2
    out
}

/// Parse a Vorbis comment packet and return the raw comment tags.
pub fn parse_roundtrip_comment(packet: &[u8]) -> Option<RoundtripMeta> {
    // packet_type(1) + "vorbis"(6) + vendor_len(4) + vendor + count(4) + comments
    if packet.len() < 11 || &packet[0..7] != b"\x03vorbis" {
        return None;
    }
    let mut pos = 7usize;
    let vlen = u32::from_le_bytes(packet[pos..pos+4].try_into().ok()?) as usize;
    pos += 4 + vlen;
    if pos + 4 > packet.len() { return None; }
    let count = u32::from_le_bytes(packet[pos..pos+4].try_into().ok()?) as usize;
    pos += 4;

    for _ in 0..count {
        if pos + 4 > packet.len() { break; }
        let slen = u32::from_le_bytes(packet[pos..pos+4].try_into().ok()?) as usize;
        pos += 4;
        if pos + slen > packet.len() { break; }
        let s = std::str::from_utf8(&packet[pos..pos+slen]).ok()?;
        pos += slen;
        if let Some(meta) = RoundtripMeta::parse(s) {
            return Some(meta);
        }
    }
    None
}

/// Metadata recovered from WEM_ROUNDTRIP_V1 tag.
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct RoundtripMeta {
    pub packet_header: u8,
    pub channels: u16,
    pub sample_rate: u32,
    pub sample_count: u32,
    pub blocksize_0_exp: u8,
    pub blocksize_1_exp: u8,
    pub setup_offset: u32,
    pub audio_offset: u32,
    /// Complete WEM file header (all bytes before data chunk content).
    /// Includes RIFF header, all non-data chunks, and data chunk id+size.
    pub wem_header: Vec<u8>,
    /// Preamble bytes before setup packet (seek table).
    pub preamble: Vec<u8>,
    /// Original setup packet bytes including 2-byte size header.
    pub setup_packet: Vec<u8>,
}

impl RoundtripMeta {
    fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("WEM_ROUNDTRIP_V1=")?;
        let kv: std::collections::HashMap<&str, &str> = rest
            .split(';')
            .filter_map(|p| {
                let mut it = p.splitn(2, '=');
                Some((it.next()?, it.next()?))
            })
            .collect();

        let get = |k: &str| -> Option<&str> { kv.get(k).copied() };

        Some(RoundtripMeta {
            packet_header:   get("packet_header")?.parse().ok()?,
            channels:        get("channels")?.parse().ok()?,
            sample_rate:     get("sample_rate")?.parse().ok()?,
            sample_count:    get("sample_count")?.parse().ok()?,
            blocksize_0_exp: get("blocksize_0")?.parse().ok()?,
            blocksize_1_exp: get("blocksize_1")?.parse().ok()?,
            setup_offset:    get("setup_offset")?.parse().ok()?,
            audio_offset:    get("audio_offset")?.parse().ok()?,
            wem_header:   hex_decode(get("header")?)?,
            preamble:     hex_decode(get("preamble")?)?,
            setup_packet: hex_decode(get("setup")?)?,
        })
    }
}

// ---------------------------------------------------------------------------
// OGG page walking
// ---------------------------------------------------------------------------

const OGG_PAGE_MAGIC: &[u8; 4] = b"OggS";

struct Page {
    #[allow(dead_code)]
    offset: usize,
    /// Capture pattern through end of data (the full raw page).
    raw: Vec<u8>,
    /// Granule position field (bytes 6..14).
    granule_pos: u64,
    /// Header type flags (byte 5).
    header_type: u8,
    /// Sequence number (bytes 18..22).
    seq_no: u32,
    /// Serial number (bytes 14..18).
    serial_no: u32,
    /// All segment payloads concatenated.
    payload: Vec<u8>,
}

fn parse_pages(data: &[u8]) -> Result<Vec<Page>> {
    let mut pages = Vec::new();
    let mut pos = 0usize;
    while pos + 27 <= data.len() {
        if &data[pos..pos+4] != OGG_PAGE_MAGIC {
            return Err(WmmoggError::OggParse(format!("bad magic at offset {pos}")));
        }
        let header_type = data[pos + 5];
        let granule_pos = u64::from_le_bytes(data[pos+6..pos+14].try_into().unwrap());
        let serial_no   = u32::from_le_bytes(data[pos+14..pos+18].try_into().unwrap());
        let seq_no      = u32::from_le_bytes(data[pos+18..pos+22].try_into().unwrap());
        let n_segs      = data[pos + 26] as usize;
        if pos + 27 + n_segs > data.len() {
            return Err(WmmoggError::OggParse("page segment table truncated".into()));
        }
        let seg_table = &data[pos+27..pos+27+n_segs];
        let payload_len: usize = seg_table.iter().map(|&s| s as usize).sum();
        let page_end = pos + 27 + n_segs + payload_len;
        if page_end > data.len() {
            return Err(WmmoggError::OggParse("page payload truncated".into()));
        }
        let payload = data[pos+27+n_segs..page_end].to_vec();
        let raw     = data[pos..page_end].to_vec();
        pages.push(Page { offset: pos, raw, granule_pos, header_type, seq_no, serial_no, payload });
        pos = page_end;
    }
    Ok(pages)
}

/// Replace the Vorbis comment packet in an OGG stream with `new_comment`.
/// All other pages are reproduced verbatim.
pub fn replace_comment_packet(ogg: Vec<u8>, new_comment: &[u8]) -> Result<Vec<u8>> {
    let pages = parse_pages(&ogg)?;

    // The comment packet is always the second logical packet.
    // In standard OGG it sits alone on page index 1 (the second page).
    // We need to find which page(s) contain packet_type=0x03.
    //
    // Strategy: reconstruct the logical stream packet by packet.
    // The comment header fits in one page in practice (it's tiny).
    // We find the page whose payload starts with 0x03 0x76 (0x03 'v'),
    // replace its payload with new_comment, recompute CRC, and splice it in.

    let mut result = Vec::with_capacity(ogg.len() + new_comment.len());
    let mut replaced = false;

    for page in &pages {
        if !replaced
            && page.payload.len() >= 7
            && page.payload[0] == 0x03
            && &page.payload[1..7] == b"vorbis"
        {
            // This page is the comment header. Rebuild it with new_comment.
            let rebuilt = rebuild_page(page, new_comment);
            result.extend_from_slice(&rebuilt);
            replaced = true;
        } else {
            result.extend_from_slice(&page.raw);
        }
    }

    if !replaced {
        return Err(WmmoggError::OggParse("comment packet not found in ogg stream".into()));
    }

    Ok(result)
}

/// Rebuild an OGG page with a new payload, recomputing the CRC.
fn rebuild_page(orig: &Page, new_payload: &[u8]) -> Vec<u8> {
    // Build segment table for new_payload (255-byte segments).
    let seg_table = build_segment_table(new_payload.len());
    let n_segs = seg_table.len() as u8;

    let header_len = 27 + seg_table.len();
    let mut page = vec![0u8; header_len + new_payload.len()];
    page[0..4].copy_from_slice(OGG_PAGE_MAGIC);
    page[4] = 0; // version
    page[5] = orig.header_type;
    page[6..14].copy_from_slice(&orig.granule_pos.to_le_bytes());
    page[14..18].copy_from_slice(&orig.serial_no.to_le_bytes());
    page[18..22].copy_from_slice(&orig.seq_no.to_le_bytes());
    page[22..26].copy_from_slice(&[0u8; 4]); // CRC placeholder
    page[26] = n_segs;
    page[27..27+seg_table.len()].copy_from_slice(&seg_table);
    page[header_len..].copy_from_slice(new_payload);

    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

fn build_segment_table(payload_len: usize) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut remaining = payload_len;
    loop {
        if remaining >= 255 {
            segs.push(255u8);
            remaining -= 255;
        } else {
            segs.push(remaining as u8);
            break;
        }
    }
    // A packet that ends on a 255-byte segment boundary needs a 0-byte
    // terminator to signal end-of-packet.
    if payload_len > 0 && payload_len % 255 == 0 {
        segs.push(0);
    }
    segs
}

fn ogg_crc(data: &[u8]) -> u32 {
    // CRC-32 with OGG polynomial 0x04c11db7, no reflection, init=0.
    static TABLE: std::sync::OnceLock<[u32; 256]> = std::sync::OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0u32..256 {
            let mut r = i << 24;
            for _ in 0..8 {
                r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04c1_1db7 } else { r << 1 };
            }
            t[i as usize] = r;
        }
        t
    });
    let mut crc = 0u32;
    for &b in data {
        crc = (crc << 8) ^ table[((crc >> 24) as u8 ^ b) as usize];
    }
    crc
}
