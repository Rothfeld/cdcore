//! Bob Jenkins Lookup3 -- two variants used by Crimson Desert.
//!
//! `pa_checksum` -- Pearl Abyss custom variant used for PAMT/PAPGT CRCs.
//! `hashlittle`  -- standard lookup3 used for ChaCha20 key derivation.

const PA_MAGIC: u32 = 0x2145E233;

/// PaChecksum: Pearl Abyss custom Bob Jenkins variant.
///
/// Used for all CRC fields in PAMT (at offset 0) and PAPGT (at offset 4),
/// computed over the data starting at offset 12.
pub fn pa_checksum(data: &[u8]) -> u32 {
    let length = data.len();
    if length == 0 {
        return 0;
    }

    let init = (length as u32).wrapping_sub(PA_MAGIC);
    let (mut a, mut b, mut c) = (init, init, init);

    let full_blocks = length / 12;
    let tail_start = full_blocks * 12;

    for i in 0..full_blocks {
        let off = i * 12;
        a = a.wrapping_add(u32::from_le_bytes(data[off..off + 4].try_into().unwrap()));
        b = b.wrapping_add(u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()));
        c = c.wrapping_add(u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()));
        mix(&mut a, &mut b, &mut c);
    }

    let remaining = length - tail_start;
    let off = tail_start;

    if remaining >= 12 { c = c.wrapping_add((data[off + 11] as u32) << 24); }
    if remaining >= 11 { c = c.wrapping_add((data[off + 10] as u32) << 16); }
    if remaining >= 10 { c = c.wrapping_add((data[off +  9] as u32) <<  8); }
    if remaining >=  9 { c = c.wrapping_add( data[off +  8] as u32); }
    if remaining >=  8 { b = b.wrapping_add((data[off +  7] as u32) << 24); }
    if remaining >=  7 { b = b.wrapping_add((data[off +  6] as u32) << 16); }
    if remaining >=  6 { b = b.wrapping_add((data[off +  5] as u32) <<  8); }
    if remaining >=  5 { b = b.wrapping_add( data[off +  4] as u32); }
    if remaining >=  4 { a = a.wrapping_add((data[off +  3] as u32) << 24); }
    if remaining >=  3 { a = a.wrapping_add((data[off +  2] as u32) << 16); }
    if remaining >=  2 { a = a.wrapping_add((data[off +  1] as u32) <<  8); }
    if remaining >=  1 { a = a.wrapping_add( data[off      ] as u32); }

    pa_final(a, b, c)
}

/// Standard Bob Jenkins lookup3 `hashlittle` -- returns the primary hash `c`.
///
/// Used for deriving ChaCha20 encryption keys from filenames.
pub fn hashlittle(data: &[u8], initval: u32) -> u32 {
    let mut length = data.len();
    let init = 0xDEADBEEFu32
        .wrapping_add(length as u32)
        .wrapping_add(initval);
    let (mut a, mut b, mut c) = (init, init, init);
    let mut off = 0usize;

    while length > 12 {
        a = a.wrapping_add(u32::from_le_bytes(data[off..off + 4].try_into().unwrap()));
        b = b.wrapping_add(u32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap()));
        c = c.wrapping_add(u32::from_le_bytes(data[off + 8..off + 12].try_into().unwrap()));
        mix(&mut a, &mut b, &mut c);
        off += 12;
        length -= 12;
    }

    if length == 0 {
        return c;
    }

    let mut tail = [0u8; 12];
    tail[..length].copy_from_slice(&data[off..off + length]);

    // c (bytes 8-11)
    if length >= 12 {
        c = c.wrapping_add(u32::from_le_bytes(tail[8..12].try_into().unwrap()));
    } else if length >= 9 {
        let v = u32::from_le_bytes(tail[8..12].try_into().unwrap());
        let mask = u32::MAX >> (8 * (12 - length));
        c = c.wrapping_add(v & mask);
    }

    // b (bytes 4-7)
    if length >= 8 {
        b = b.wrapping_add(u32::from_le_bytes(tail[4..8].try_into().unwrap()));
    } else if length >= 5 {
        let v = u32::from_le_bytes(tail[4..8].try_into().unwrap());
        let mask = u32::MAX >> (8 * (8 - length));
        b = b.wrapping_add(v & mask);
    }

    // a (bytes 0-3)
    if length >= 4 {
        a = a.wrapping_add(u32::from_le_bytes(tail[0..4].try_into().unwrap()));
    } else {
        let v = u32::from_le_bytes(tail[0..4].try_into().unwrap());
        let mask = u32::MAX >> (8 * (4 - length));
        a = a.wrapping_add(v & mask);
    }

    hl_final(a, b, c)
}

#[inline(always)]
fn mix(a: &mut u32, b: &mut u32, c: &mut u32) {
    *a = a.wrapping_sub(*c); *a ^= c.rotate_left(4);  *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a); *b ^= a.rotate_left(6);  *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b); *c ^= b.rotate_left(8);  *b = b.wrapping_add(*a);
    *a = a.wrapping_sub(*c); *a ^= c.rotate_left(16); *c = c.wrapping_add(*b);
    *b = b.wrapping_sub(*a); *b ^= a.rotate_left(19); *a = a.wrapping_add(*c);
    *c = c.wrapping_sub(*b); *c ^= b.rotate_left(4);  *b = b.wrapping_add(*a);
}

/// PaChecksum final mix (differs from standard hashlittle).
#[inline(always)]
fn pa_final(a: u32, b: u32, c: u32) -> u32 {
    let v82 = (b ^ c).wrapping_sub(b.rotate_left(14));
    let v83 = (a ^ v82).wrapping_sub(v82.rotate_left(11));
    let v84 = (v83 ^ b).wrapping_sub(v83.rotate_right(7));
    let v85 = (v84 ^ v82).wrapping_sub(v84.rotate_left(16));
    let v86 = v85.rotate_left(4);
    let t   = (v83 ^ v85).wrapping_sub(v86);
    let v87 = (t ^ v84).wrapping_sub(t.rotate_left(14));
    (v87 ^ v85).wrapping_sub(v87.rotate_right(8))
}

/// Standard hashlittle final mix.
#[inline(always)]
fn hl_final(mut a: u32, mut b: u32, mut c: u32) -> u32 {
    c ^= b; c = c.wrapping_sub(b.rotate_left(14));
    a ^= c; a = a.wrapping_sub(c.rotate_left(11));
    b ^= a; b = b.wrapping_sub(a.rotate_left(25));
    c ^= b; c = c.wrapping_sub(b.rotate_left(16));
    a ^= c; a = a.wrapping_sub(c.rotate_left(4));
    b ^= a; b = b.wrapping_sub(a.rotate_left(14));
    c ^= b; c = c.wrapping_sub(b.rotate_left(24));
    c
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pa_checksum_empty() {
        assert_eq!(pa_checksum(&[]), 0);
    }

    #[test]
    fn hashlittle_empty() {
        // For empty data, hashlittle returns c = 0xDEADBEEF + 0 + initval.
        // With initval=0: c = 0xDEADBEEF, then returns c directly (length==0).
        assert_eq!(hashlittle(&[], 0), 0xDEADBEEF);
    }
}
