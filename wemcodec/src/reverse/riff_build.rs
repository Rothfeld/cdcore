/// Serialize a stream of stripped Wwise packets into the data chunk byte stream.
///
/// Each packet is prepended with a 2-byte little-endian size header
/// (the only format used by modern Wwise / Crimson Desert).
pub fn build_data_chunk_2byte(packets: &[Vec<u8>]) -> Vec<u8> {
    let mut data = Vec::new();
    for pkt in packets {
        let sz = pkt.len() as u16;
        data.extend_from_slice(&sz.to_le_bytes());
        data.extend_from_slice(pkt);
    }
    data
}

/// Serialize with 6-byte headers: u32 granule + u16 size (both LE for RIFF).
pub fn build_data_chunk_6byte(packets: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut data = Vec::new();
    for (granule, pkt) in packets {
        data.extend_from_slice(&granule.to_le_bytes());
        data.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
        data.extend_from_slice(pkt);
    }
    data
}
