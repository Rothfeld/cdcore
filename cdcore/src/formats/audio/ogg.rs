/// OGG/Vorbis processing for WEM conversion.
///
/// Contains all Vorbis internals: bit I/O, packed codebook library, setup
/// header stripping, audio packet stripping, OGG page operations, and the
/// WEM_ROUNDTRIP_V1 comment tag.  Everything here is pub(super); callers
/// go through forward.rs / reverse.rs / replace.rs.

use sha2::{Sha256, Digest};
use std::collections::HashMap;
use std::sync::OnceLock;

use super::{Result, WemError};
use super::wem::{PacketHeaderFormat, Wem};

// ---------------------------------------------------------------------------
// Bit I/O — LSB-first reader/writer for Vorbis packet parsing
// ---------------------------------------------------------------------------

pub struct BitReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, byte_pos: 0, bit_pos: 0 }
    }

    pub fn read_bit(&mut self) -> Result<bool> {
        if self.byte_pos >= self.data.len() {
            return Err(WemError::VorbisParse("unexpected end of bits".into()));
        }
        let bit = (self.data[self.byte_pos] >> self.bit_pos) & 1 != 0;
        self.bit_pos += 1;
        if self.bit_pos == 8 { self.bit_pos = 0; self.byte_pos += 1; }
        Ok(bit)
    }

    pub fn read_bits(&mut self, n: u8) -> Result<u32> {
        assert!(n <= 32);
        let mut val = 0u32;
        for i in 0..n { if self.read_bit()? { val |= 1 << i; } }
        Ok(val)
    }
}

pub struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    bit_pos: u8,
}

impl BitWriter {
    pub fn new() -> Self { Self { bytes: Vec::new(), current: 0, bit_pos: 0 } }

    pub fn write_bit(&mut self, b: bool) {
        if b { self.current |= 1 << self.bit_pos; }
        self.bit_pos += 1;
        if self.bit_pos == 8 { self.bytes.push(self.current); self.current = 0; self.bit_pos = 0; }
    }

    pub fn write_bits(&mut self, val: u32, n: u8) {
        for i in 0..n { self.write_bit((val >> i) & 1 != 0); }
    }

    pub fn finish(mut self) -> Vec<u8> {
        if self.bit_pos > 0 { self.bytes.push(self.current); }
        self.bytes
    }
}

// ---------------------------------------------------------------------------
// Packed codebook library — hash LUT for setup stripping
// (sourced from ww2ogg 0.1.0, BSD-3-Clause)
// ---------------------------------------------------------------------------

const PACKED_DEFAULT: &[u8] = include_bytes!("codebooks/packed_codebooks.bin");
const PACKED_AOTUV:   &[u8] = include_bytes!("codebooks/packed_codebooks_aoTuV_603.bin");

pub struct CodebookLut { map: HashMap<[u8; 32], u32> }

impl CodebookLut {
    pub fn lookup(&self, expanded: &[u8]) -> Option<u32> {
        let hash: [u8; 32] = Sha256::digest(expanded).into();
        self.map.get(&hash).copied()
    }
}

pub fn default_lut() -> Result<&'static CodebookLut> {
    static LUT: OnceLock<CodebookLut> = OnceLock::new();
    Ok(LUT.get_or_init(|| build_lut(PACKED_DEFAULT)
        .expect("embedded packed_codebooks.bin failed to build LUT")))
}

pub fn aotuv_lut() -> Result<&'static CodebookLut> {
    static LUT: OnceLock<CodebookLut> = OnceLock::new();
    Ok(LUT.get_or_init(|| build_lut(PACKED_AOTUV)
        .expect("embedded packed_codebooks_aoTuV_603.bin failed to build LUT")))
}

fn build_lut(packed: &[u8]) -> std::result::Result<CodebookLut, String> {
    let (data, offsets) = parse_packed_library(packed)?;
    let n = offsets.len().saturating_sub(1);
    let mut map = HashMap::with_capacity(n);
    for i in 0..n {
        let expanded = expand_packed_codebook(&data[offsets[i]..offsets[i+1]])
            .map_err(|e| format!("expand codebook {i}: {e}"))?;
        let hash: [u8; 32] = Sha256::digest(&expanded).into();
        map.insert(hash, i as u32);
    }
    Ok(CodebookLut { map })
}

fn parse_packed_library(packed: &[u8]) -> std::result::Result<(&[u8], Vec<usize>), String> {
    if packed.len() < 8 { return Err("packed library too small".into()); }
    let table_offset = u32::from_le_bytes(packed[packed.len()-4..].try_into().unwrap()) as usize;
    if table_offset + 8 > packed.len() {
        return Err(format!("table_offset {table_offset} out of range"));
    }
    let n = (packed.len() - 4 - table_offset) / 4;
    let offsets = (0..n).map(|i| {
        let pos = table_offset + i * 4;
        u32::from_le_bytes(packed[pos..pos+4].try_into().unwrap()) as usize
    }).collect();
    Ok((&packed[..table_offset], offsets))
}

/// Expand one packed library entry to standard Vorbis codebook form.
///
/// Packed: 4-bit dims, 14-bit entries, 1-bit lookup type.
/// Standard Vorbis: 24-bit sync, 16-bit dims, 24-bit entries, 4-bit lookup type.
fn expand_packed_codebook(packed: &[u8]) -> std::result::Result<Vec<u8>, String> {
    let mut r = BitReader::new(packed);
    let mut w = BitWriter::new();

    let dimensions = r.read_bits(4).map_err(|e| format!("dims: {e}"))?;
    let entries    = r.read_bits(14).map_err(|e| format!("entries: {e}"))?;

    w.write_bits(0x564342, 24);
    w.write_bits(dimensions, 16);
    w.write_bits(entries, 24);

    let ordered = r.read_bit().map_err(|e| format!("ordered: {e}"))?;
    w.write_bit(ordered);

    if ordered {
        let init_len = r.read_bits(5).map_err(|e| format!("init_len: {e}"))?;
        w.write_bits(init_len, 5);
        let mut current = 0u32;
        while current < entries {
            let bits = ilog(entries - current) as u8;
            let count = r.read_bits(bits).map_err(|e| format!("count: {e}"))?;
            w.write_bits(count, bits);
            current += count;
        }
    } else {
        let cll    = r.read_bits(3).map_err(|e| format!("cll: {e}"))? as u8;
        let sparse = r.read_bit().map_err(|e| format!("sparse: {e}"))?;
        if cll == 0 || cll > 5 { return Err(format!("bad codeword_length_length {cll}")); }
        w.write_bit(sparse);
        for _ in 0..entries {
            let present = if sparse {
                let p = r.read_bit().map_err(|e| format!("present: {e}"))?;
                w.write_bit(p); p
            } else { true };
            if present {
                let len = r.read_bits(cll).map_err(|e| format!("len: {e}"))?;
                w.write_bits(len, 5);
            }
        }
    }

    let lookup_type = r.read_bit().map_err(|e| format!("lookup_type: {e}"))?;
    w.write_bits(if lookup_type { 1 } else { 0 }, 4);

    if lookup_type {
        for _ in 0..2 { w.write_bits(r.read_bits(32).map_err(|e| format!("float: {e}"))?, 32); }
        let value_length = r.read_bits(4).map_err(|e| format!("value_length: {e}"))?;
        w.write_bits(value_length, 4);
        w.write_bit(r.read_bit().map_err(|e| format!("seq: {e}"))?);
        let quantvals = book_map_type1_quantvals(entries, dimensions);
        for _ in 0..quantvals {
            let v = r.read_bits((value_length + 1) as u8).map_err(|e| format!("val: {e}"))?;
            w.write_bits(v, (value_length + 1) as u8);
        }
    }

    Ok(w.finish())
}

// ---------------------------------------------------------------------------
// Vorbis setup header stripping (expanded OGG → Wwise stripped)
// ---------------------------------------------------------------------------

pub fn strip_setup_header(packet: &[u8], channels: usize, lut: &CodebookLut) -> Result<Vec<u8>> {
    if packet.len() < 7 || &packet[0..7] != b"\x05vorbis" {
        return Err(WemError::VorbisParse("not a vorbis setup packet".into()));
    }
    let mut r = BitReader::new(&packet[7..]);
    let mut w = BitWriter::new();

    let cb_count_m1 = r.read_bits(8)? as usize;
    w.write_bits(cb_count_m1 as u32, 8);
    for _ in 0..=cb_count_m1 {
        let cb = read_full_codebook_bits(&mut r)?;
        let idx = lut.lookup(&cb).ok_or(WemError::CodebookNotFound)?;
        w.write_bits(idx, 10);
    }

    let time_count_m1 = r.read_bits(6)? as usize;
    for _ in 0..=time_count_m1 {
        assert_eq!(r.read_bits(16)?, 0, "time domain type must be 0");
    }

    let floor_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(floor_count_m1 as u32, 6);
    for _ in 0..=floor_count_m1 {
        assert_eq!(r.read_bits(16)?, 1, "only floor type 1 supported");
        strip_floor_type1(&mut r, &mut w)?;
    }

    let res_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(res_count_m1 as u32, 6);
    for _ in 0..=res_count_m1 {
        w.write_bits(r.read_bits(16)?, 2);
        strip_residue(&mut r, &mut w)?;
    }

    let map_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(map_count_m1 as u32, 6);
    for _ in 0..=map_count_m1 {
        assert_eq!(r.read_bits(16)?, 0, "only mapping type 0 supported");
        strip_mapping(&mut r, &mut w, channels)?;
    }

    let mode_count_m1 = r.read_bits(6)? as usize;
    w.write_bits(mode_count_m1 as u32, 6);
    for _ in 0..=mode_count_m1 {
        let blockflag = r.read_bit()?;
        let _wt = r.read_bits(16)?;
        let _tt = r.read_bits(16)?;
        let mapping = r.read_bits(8)?;
        w.write_bit(blockflag);
        w.write_bits(mapping, 8);
    }

    assert!(r.read_bit()?, "setup framing bit must be 1");
    Ok(w.finish())
}

fn strip_floor_type1(r: &mut BitReader, w: &mut BitWriter) -> Result<()> {
    let partitions = r.read_bits(5)?;
    w.write_bits(partitions, 5);
    let mut pc = vec![0u32; partitions as usize];
    for i in 0..partitions as usize { let c = r.read_bits(4)?; w.write_bits(c, 4); pc[i] = c; }
    let max_class = pc.iter().copied().max().unwrap_or(0) as usize;
    let mut class_dims = vec![0u32; max_class + 1];
    for c in 0..=max_class {
        let dm1 = r.read_bits(3)?; w.write_bits(dm1, 3); class_dims[c] = dm1 + 1;
        let sub = r.read_bits(2)?; w.write_bits(sub, 2);
        if sub != 0 { w.write_bits(r.read_bits(8)?, 8); }
        for _ in 0..(1u32 << sub) { w.write_bits(r.read_bits(8)?, 8); }
    }
    w.write_bits(r.read_bits(2)?, 2);
    let rangebits = r.read_bits(4)?; w.write_bits(rangebits, 4);
    for i in 0..partitions as usize {
        for _ in 0..class_dims[pc[i] as usize] {
            w.write_bits(r.read_bits(rangebits as u8)?, rangebits as u8);
        }
    }
    Ok(())
}

fn strip_residue(r: &mut BitReader, w: &mut BitWriter) -> Result<()> {
    for _ in 0..3 { w.write_bits(r.read_bits(24)?, 24); }
    let cls_m1 = r.read_bits(6)?; w.write_bits(cls_m1, 6);
    w.write_bits(r.read_bits(8)?, 8);
    let classifications = cls_m1 as usize + 1;
    let mut cascade = vec![[false; 8]; classifications];
    for c in 0..classifications {
        let high = r.read_bits(3)?; w.write_bits(high, 3);
        let flag = r.read_bit()?;   w.write_bit(flag);
        let low  = if flag { let v = r.read_bits(5)?; w.write_bits(v, 5); v } else { 0 };
        let combined = high * 8 + if flag { low } else { 0 };
        for pass in 0..8 { cascade[c][pass] = (combined >> pass) & 1 != 0; }
    }
    for c in 0..classifications {
        for pass in 0..8 { if cascade[c][pass] { w.write_bits(r.read_bits(8)?, 8); } }
    }
    Ok(())
}

fn strip_mapping(r: &mut BitReader, w: &mut BitWriter, channels: usize) -> Result<()> {
    let submaps_flag = r.read_bit()?; w.write_bit(submaps_flag);
    let submaps = if submaps_flag { let s = r.read_bits(4)? + 1; w.write_bits(s-1, 4); s as usize } else { 1 };
    let coupling = r.read_bit()?; w.write_bit(coupling);
    if coupling {
        let ilog_ch = ilog((channels - 1) as u32) as u8;
        let steps = r.read_bits(8)? + 1; w.write_bits(steps-1, 8);
        for _ in 0..steps {
            w.write_bits(r.read_bits(ilog_ch)?, ilog_ch);
            w.write_bits(r.read_bits(ilog_ch)?, ilog_ch);
        }
    }
    // 2 reserved bits present in both expanded and stripped Wwise form.
    let reserved = r.read_bits(2)?;
    assert_eq!(reserved, 0, "mapping reserved bits must be 0");
    w.write_bits(reserved, 2);
    if submaps > 1 { for _ in 0..channels { w.write_bits(r.read_bits(4)?, 4); } }
    for _ in 0..submaps {
        w.write_bits(r.read_bits(8)?, 8);
        w.write_bits(r.read_bits(8)?, 8);
        w.write_bits(r.read_bits(8)?, 8);
    }
    Ok(())
}

/// Read and capture one full expanded Vorbis codebook from the bit stream.
fn read_full_codebook_bits(r: &mut BitReader) -> Result<Vec<u8>> {
    let mut w = BitWriter::new();
    let sync = r.read_bits(24)?;
    if sync != 0x564342 {
        return Err(WemError::VorbisParse(format!("codebook sync 0x{sync:06x} != 0x564342")));
    }
    w.write_bits(sync, 24);
    let dimensions = r.read_bits(16)?; w.write_bits(dimensions, 16);
    let entries    = r.read_bits(24)?; w.write_bits(entries, 24);
    let ordered = r.read_bit()?; w.write_bit(ordered);
    if ordered {
        let init = r.read_bits(5)?; w.write_bits(init, 5);
        let mut cur = 0u32;
        while cur < entries {
            let bits = ilog(entries - cur) as u8;
            let count = r.read_bits(bits)?; w.write_bits(count, bits);
            cur += count;
        }
    } else {
        let sparse = r.read_bit()?; w.write_bit(sparse);
        for _ in 0..entries {
            if sparse { let p = r.read_bit()?; w.write_bit(p); if !p { continue; } }
            let len_m1 = r.read_bits(5)?; w.write_bits(len_m1, 5);
        }
    }
    let lookup_type = r.read_bits(4)?; w.write_bits(lookup_type, 4);
    if lookup_type == 1 || lookup_type == 2 {
        let min = r.read_bits(32)?; w.write_bits(min, 32);
        let delta = r.read_bits(32)?; w.write_bits(delta, 32);
        let vbits = r.read_bits(4)? + 1; w.write_bits(vbits - 1, 4);
        let seq = r.read_bit()?; w.write_bit(seq);
        let lv = if lookup_type == 1 { book_map_type1_quantvals(entries, dimensions) } else { entries * dimensions };
        for _ in 0..lv { let v = r.read_bits(vbits as u8)?; w.write_bits(v, vbits as u8); }
    }
    Ok(w.finish())
}

pub fn read_full_codebook_bits_skip(r: &mut BitReader) -> Result<()> {
    read_full_codebook_bits(r)?; Ok(())
}

// ---------------------------------------------------------------------------
// Audio packet stripping and data chunk building
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
pub struct Mode { pub blockflag: bool }

pub fn strip_audio_packet(packet: &[u8], modes: &[Mode]) -> Result<Vec<u8>> {
    if packet.is_empty() {
        return Err(WemError::VorbisParse("empty audio packet".into()));
    }
    let mut r = BitReader::new(packet);
    let mut w = BitWriter::new();

    let packet_type = r.read_bit()?;
    if packet_type { return Err(WemError::VorbisParse("audio packet has non-zero type bit".into())); }

    let mode_bits = ilog(modes.len() as u32 - 1) as u8;
    let mode_idx  = r.read_bits(mode_bits)? as usize;
    if mode_idx >= modes.len() {
        return Err(WemError::VorbisParse(format!("mode index {mode_idx} >= {}", modes.len())));
    }
    w.write_bits(mode_idx as u32, mode_bits);

    if modes[mode_idx].blockflag {
        r.read_bit()?; // prev_window
        r.read_bit()?; // next_window
    }

    let original_bits = (packet.len() - 1) * 8;
    let rest_bits = original_bits.saturating_sub(mode_bits as usize);
    for _ in 0..rest_bits { w.write_bit(r.read_bit()?); }

    Ok(w.finish())
}

pub fn build_data_chunk_2byte(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut data = Vec::new();
    for pkt in packets {
        data.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
        data.extend_from_slice(pkt);
    }
    data
}

pub fn build_data_chunk_6byte(packets: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (granule, pkt) in packets {
        data.extend_from_slice(&granule.to_le_bytes());
        data.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
        data.extend_from_slice(pkt);
    }
    data
}

// ---------------------------------------------------------------------------
// OGG page operations and WEM_ROUNDTRIP_V1 comment tag
// ---------------------------------------------------------------------------

const OGG_MAGIC: &[u8; 4] = b"OggS";

struct Page {
    raw: Vec<u8>,
    granule_pos: u64,
    header_type: u8,
    seq_no: u32,
    serial_no: u32,
    payload: Vec<u8>,
}

fn parse_pages(data: &[u8]) -> Result<Vec<Page>> {
    let mut pages = Vec::new();
    let mut pos = 0usize;
    while pos + 27 <= data.len() {
        if &data[pos..pos+4] != OGG_MAGIC {
            return Err(WemError::OggParse(format!("bad magic at {pos}")));
        }
        let header_type = data[pos+5];
        let granule_pos = u64::from_le_bytes(data[pos+6..pos+14].try_into().unwrap());
        let serial_no   = u32::from_le_bytes(data[pos+14..pos+18].try_into().unwrap());
        let seq_no      = u32::from_le_bytes(data[pos+18..pos+22].try_into().unwrap());
        let n_segs      = data[pos+26] as usize;
        if pos + 27 + n_segs > data.len() {
            return Err(WemError::OggParse("segment table truncated".into()));
        }
        let payload_len: usize = data[pos+27..pos+27+n_segs].iter().map(|&s| s as usize).sum();
        let end = pos + 27 + n_segs + payload_len;
        if end > data.len() { return Err(WemError::OggParse("page payload truncated".into())); }
        pages.push(Page {
            raw: data[pos..end].to_vec(),
            granule_pos, header_type, seq_no, serial_no,
            payload: data[pos+27+n_segs..end].to_vec(),
        });
        pos = end;
    }
    Ok(pages)
}

pub fn replace_comment_packet(ogg: Vec<u8>, new_comment: &[u8]) -> Result<Vec<u8>> {
    let pages = parse_pages(&ogg)?;
    let mut result = Vec::with_capacity(ogg.len() + new_comment.len());
    let mut replaced = false;
    for page in &pages {
        if !replaced && page.payload.len() >= 7
            && page.payload[0] == 0x03 && &page.payload[1..7] == b"vorbis"
        {
            result.extend_from_slice(&rebuild_page(page, new_comment));
            replaced = true;
        } else {
            result.extend_from_slice(&page.raw);
        }
    }
    if !replaced { return Err(WemError::OggParse("comment packet not found".into())); }
    Ok(result)
}

fn rebuild_page(orig: &Page, payload: &[u8]) -> Vec<u8> {
    let seg_table = build_segment_table(payload.len());
    let hlen = 27 + seg_table.len();
    let mut page = vec![0u8; hlen + payload.len()];
    page[0..4].copy_from_slice(OGG_MAGIC);
    page[5] = orig.header_type;
    page[6..14].copy_from_slice(&orig.granule_pos.to_le_bytes());
    page[14..18].copy_from_slice(&orig.serial_no.to_le_bytes());
    page[18..22].copy_from_slice(&orig.seq_no.to_le_bytes());
    page[26] = seg_table.len() as u8;
    page[27..27+seg_table.len()].copy_from_slice(&seg_table);
    page[hlen..].copy_from_slice(payload);
    let crc = ogg_crc(&page);
    page[22..26].copy_from_slice(&crc.to_le_bytes());
    page
}

fn build_segment_table(len: usize) -> Vec<u8> {
    let mut segs = Vec::new();
    let mut rem = len;
    loop {
        if rem >= 255 { segs.push(255u8); rem -= 255; }
        else { segs.push(rem as u8); break; }
    }
    if len > 0 && len % 255 == 0 { segs.push(0); }
    segs
}

fn ogg_crc(data: &[u8]) -> u32 {
    static TABLE: OnceLock<[u32; 256]> = OnceLock::new();
    let table = TABLE.get_or_init(|| {
        let mut t = [0u32; 256];
        for i in 0u32..256 {
            let mut r = i << 24;
            for _ in 0..8 { r = if r & 0x8000_0000 != 0 { (r << 1) ^ 0x04c1_1db7 } else { r << 1 }; }
            t[i as usize] = r;
        }
        t
    });
    let mut crc = 0u32;
    for &b in data { crc = (crc << 8) ^ table[((crc >> 24) as u8 ^ b) as usize]; }
    crc
}

// WEM_ROUNDTRIP_V1 tag

pub fn build_roundtrip_comment(wem: &Wem, wem_bytes: &[u8]) -> Vec<u8> {
    let pkt_hdr = match wem.packet_fmt {
        PacketHeaderFormat::TwoByte   => "2",
        PacketHeaderFormat::SixByte   => "6",
        PacketHeaderFormat::EightByte => "8",
    };
    let setup_off = wem.fmt.setup_packet_offset as usize;
    let audio_off = wem.fmt.first_audio_packet_offset as usize;
    let data_offset = find_data_content_offset(wem_bytes).expect("wem must have a data chunk");
    let wem_header = &wem_bytes[..data_offset];
    let preamble   = &wem.data[..setup_off];
    let setup_size = u16::from_le_bytes(wem.data[setup_off..setup_off+2].try_into().unwrap()) as usize;
    let setup_with_hdr = &wem.data[setup_off..setup_off+2+setup_size];
    let tag = format!(
        "WEM_ROUNDTRIP_V1=packet_header={pkt_hdr};\
         channels={};sample_rate={};sample_count={};\
         blocksize_0={};blocksize_1={};\
         setup_offset={setup_off};audio_offset={audio_off};\
         header={};preamble={};setup={}",
        wem.fmt.channels, wem.fmt.sample_rate, wem.fmt.sample_count,
        wem.fmt.blocksize_0_exp, wem.fmt.blocksize_1_exp,
        hex_encode(wem_header), hex_encode(preamble), hex_encode(setup_with_hdr),
    );
    build_vorbis_comment_packet("wemcodec", &[tag.as_str()])
}

fn find_data_content_offset(wem: &[u8]) -> Option<usize> {
    let mut pos = 12usize;
    while pos + 8 <= wem.len() {
        let sz = u32::from_le_bytes(wem[pos+4..pos+8].try_into().ok()?) as usize;
        if &wem[pos..pos+4] == b"data" { return Some(pos + 8); }
        pos += 8 + sz + (sz & 1);
    }
    None
}

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
    out.push(0x01); // Vorbis spec §5.2 framing bit
    out
}

pub fn parse_roundtrip_comment(packet: &[u8]) -> Option<RoundtripMeta> {
    if packet.len() < 11 || &packet[0..7] != b"\x03vorbis" { return None; }
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
        if let Some(meta) = RoundtripMeta::parse(s) { return Some(meta); }
    }
    None
}

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
    pub wem_header: Vec<u8>,
    pub preamble: Vec<u8>,
    pub setup_packet: Vec<u8>,
}

impl RoundtripMeta {
    fn parse(s: &str) -> Option<Self> {
        let rest = s.strip_prefix("WEM_ROUNDTRIP_V1=")?;
        let kv: HashMap<&str, &str> = rest.split(';')
            .filter_map(|p| { let mut it = p.splitn(2, '='); Some((it.next()?, it.next()?)) })
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

fn hex_encode(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }

pub fn hex_decode(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 { return None; }
    (0..s.len()/2).map(|i| u8::from_str_radix(&s[2*i..2*i+2], 16).ok()).collect()
}

// ---------------------------------------------------------------------------
// OGG logical stream utilities
// ---------------------------------------------------------------------------

pub fn extract_vorbis_packets(ogg: &[u8]) -> Result<Vec<Vec<u8>>> {
    let mut packets = Vec::new();
    let mut pos = 0usize;
    let mut current: Vec<u8> = Vec::new();
    while pos + 27 <= ogg.len() {
        if &ogg[pos..pos+4] != OGG_MAGIC {
            return Err(WemError::OggParse(format!("bad OggS magic at {pos}")));
        }
        let n_segs = ogg[pos+26] as usize;
        if pos + 27 + n_segs > ogg.len() {
            return Err(WemError::OggParse("segment table truncated".into()));
        }
        let seg_table = &ogg[pos+27..pos+27+n_segs];
        let mut payload_pos = pos + 27 + n_segs;
        for &seg in seg_table {
            let end = payload_pos + seg as usize;
            if end > ogg.len() { return Err(WemError::OggParse("segment payload truncated".into())); }
            current.extend_from_slice(&ogg[payload_pos..end]);
            payload_pos = end;
            if seg < 255 && !current.is_empty() { packets.push(std::mem::take(&mut current)); }
        }
        pos = payload_pos;
    }
    if !current.is_empty() { packets.push(current); }
    Ok(packets)
}

pub fn extract_granule_positions(ogg: &[u8], audio_count: usize) -> Result<Vec<u32>> {
    let mut granules = Vec::new();
    let mut pos = 0usize;
    while pos + 27 <= ogg.len() {
        if &ogg[pos..pos+4] != OGG_MAGIC { break; }
        let granule = u64::from_le_bytes(ogg[pos+6..pos+14].try_into().unwrap());
        let n_segs  = ogg[pos+26] as usize;
        let plen: usize = ogg[pos+27..pos+27+n_segs].iter().map(|&s| s as usize).sum();
        pos += 27 + n_segs + plen;
        if granules.len() < 3 { granules.push(0u32); continue; }
        granules.push(granule as u32);
    }
    granules.truncate(audio_count + 3);
    Ok(granules.into_iter().skip(3).collect())
}

pub fn last_granule_position(ogg: &[u8]) -> Result<u32> {
    let mut last = 0u64;
    let mut pos = 0usize;
    while pos + 27 <= ogg.len() {
        if &ogg[pos..pos+4] != OGG_MAGIC { break; }
        let granule = u64::from_le_bytes(ogg[pos+6..pos+14].try_into().unwrap());
        let n_segs  = ogg[pos+26] as usize;
        if pos + 27 + n_segs > ogg.len() { break; }
        let plen: usize = ogg[pos+27..pos+27+n_segs].iter().map(|&s| s as usize).sum();
        pos += 27 + n_segs + plen;
        if granule != u64::MAX { last = granule; }
    }
    Ok(last as u32)
}

/// Parse the Vorbis ID header: returns (channels, sample_rate, blocksize_0_exp, blocksize_1_exp).
pub fn parse_id_header(packet: &[u8]) -> Result<(u16, u32, u8, u8)> {
    if packet.len() < 30 || &packet[0..7] != b"\x01vorbis" {
        return Err(WemError::OggParse("not a vorbis ID header".into()));
    }
    let channels    = packet[11] as u16;
    let sample_rate = u32::from_le_bytes(packet[12..16].try_into().unwrap());
    let packed_bs   = packet[28];
    if channels == 0 { return Err(WemError::OggParse("ID header: channels=0".into())); }
    if sample_rate == 0 { return Err(WemError::OggParse("ID header: sample_rate=0".into())); }
    Ok((channels, sample_rate, packed_bs & 0x0F, (packed_bs >> 4) & 0x0F))
}

/// Extract the mode table from an expanded standard Vorbis setup packet.
pub fn extract_modes(packet: &[u8], channels: usize) -> Result<Vec<Mode>> {
    if packet.len() < 7 || &packet[0..7] != b"\x05vorbis" {
        return Err(WemError::VorbisParse("not a vorbis setup packet".into()));
    }
    let mut r = BitReader::new(&packet[7..]);

    let cb_count = r.read_bits(8)? as usize + 1;
    for _ in 0..cb_count { read_full_codebook_bits_skip(&mut r)?; }

    let time_count = r.read_bits(6)? as usize + 1;
    for _ in 0..time_count { r.read_bits(16)?; }

    let floor_count = r.read_bits(6)? as usize + 1;
    for _ in 0..floor_count { r.read_bits(16)?; skip_expanded_floor(&mut r)?; }

    let res_count = r.read_bits(6)? as usize + 1;
    for _ in 0..res_count { r.read_bits(16)?; skip_expanded_residue(&mut r)?; }

    let map_count = r.read_bits(6)? as usize + 1;
    for _ in 0..map_count { r.read_bits(16)?; skip_expanded_mapping(&mut r, channels)?; }

    let mode_count = r.read_bits(6)? as usize + 1;
    let mut modes = Vec::with_capacity(mode_count);
    for _ in 0..mode_count {
        let blockflag = r.read_bit()?;
        r.read_bits(16)?; r.read_bits(16)?; r.read_bits(8)?;
        modes.push(Mode { blockflag });
    }
    Ok(modes)
}

fn skip_expanded_floor(r: &mut BitReader) -> Result<()> {
    let partitions = r.read_bits(5)? as usize;
    let mut pc = vec![0usize; partitions];
    for i in 0..partitions { pc[i] = r.read_bits(4)? as usize; }
    let max_class = pc.iter().copied().max().unwrap_or(0);
    let mut class_dims = vec![0u32; max_class + 1];
    for c in 0..=max_class {
        class_dims[c] = r.read_bits(3)? + 1;
        let sub = r.read_bits(2)? as u32;
        if sub != 0 { r.read_bits(8)?; }
        for _ in 0..(1u32 << sub) { r.read_bits(8)?; }
    }
    r.read_bits(2)?;
    let rangebits = r.read_bits(4)? as u8;
    for i in 0..partitions { for _ in 0..class_dims[pc[i]] { r.read_bits(rangebits)?; } }
    Ok(())
}

fn skip_expanded_residue(r: &mut BitReader) -> Result<()> {
    r.read_bits(24)?; r.read_bits(24)?; r.read_bits(24)?;
    let cls = r.read_bits(6)? as usize + 1;
    r.read_bits(8)?;
    let mut cascade = vec![[false; 8]; cls];
    for c in 0..cls {
        let high = r.read_bits(3)?;
        let flag = r.read_bit()?;
        let low  = if flag { r.read_bits(5)? } else { 0 };
        let combined = high * 8 + if flag { low } else { 0 };
        for pass in 0..8 { cascade[c][pass] = (combined >> pass) & 1 != 0; }
    }
    for c in 0..cls { for pass in 0..8 { if cascade[c][pass] { r.read_bits(8)?; } } }
    Ok(())
}

fn skip_expanded_mapping(r: &mut BitReader, channels: usize) -> Result<()> {
    let submaps_flag = r.read_bit()?;
    let submaps = if submaps_flag { r.read_bits(4)? as usize + 1 } else { 1 };
    let coupling = r.read_bit()?;
    if coupling {
        let ilog_ch = ilog((channels - 1) as u32) as u8;
        let steps = r.read_bits(8)? as usize + 1;
        for _ in 0..steps { r.read_bits(ilog_ch)?; r.read_bits(ilog_ch)?; }
    }
    r.read_bits(2)?; // reserved
    if submaps > 1 { for _ in 0..channels { r.read_bits(4)?; } }
    for _ in 0..submaps { r.read_bits(8)?; r.read_bits(8)?; r.read_bits(8)?; }
    Ok(())
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn ilog(v: u32) -> u32 { if v == 0 { 0 } else { 32 - v.leading_zeros() } }

fn book_map_type1_quantvals(entries: u32, dimensions: u32) -> u32 {
    let mut vals = (entries as f64).powf(1.0 / dimensions as f64) as u32;
    loop {
        if vals.saturating_pow(dimensions) > entries { vals -= 1; break; }
        if (vals + 1).saturating_pow(dimensions) > entries { break; }
        vals += 1;
    }
    vals
}
