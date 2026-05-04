//! ChaCha20 encryption/decryption with filename-based key derivation.
//!
//! Keys are derived from the file's basename (lowercase) using Bob Jenkins
//! hashlittle with HASH_INITVAL=0x000C5EDE. The cipher is symmetric.

use chacha20::cipher::{KeyIvInit, StreamCipher, StreamCipherSeek};
use chacha20::ChaCha20;

use super::checksum::hashlittle;

const HASH_INITVAL: u32 = 0x000C5EDE;
const IV_XOR: u32 = 0x60616263;
const XOR_DELTAS: [u32; 8] = [
    0x00000000, 0x0A0A0A0A, 0x0C0C0C0C, 0x06060606,
    0x0E0E0E0E, 0x0A0A0A0A, 0x06060606, 0x02020202,
];

/// Derive a 32-byte key and 16-byte IV from a filename basename.
///
/// The basename is lowercased before hashing.
pub fn derive_key_iv(filename: &str) -> ([u8; 32], [u8; 16]) {
    let basename = std::path::Path::new(filename)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(filename);
    let lower = basename.to_lowercase();
    let seed = hashlittle(lower.as_bytes(), HASH_INITVAL);

    let mut iv = [0u8; 16];
    let seed_le = seed.to_le_bytes();
    iv[..4].copy_from_slice(&seed_le);
    iv[4..8].copy_from_slice(&seed_le);
    iv[8..12].copy_from_slice(&seed_le);
    iv[12..16].copy_from_slice(&seed_le);

    let key_base = seed ^ IV_XOR;
    let mut key = [0u8; 32];
    for (i, &delta) in XOR_DELTAS.iter().enumerate() {
        let word = (key_base ^ delta).to_le_bytes();
        key[i * 4..i * 4 + 4].copy_from_slice(&word);
    }

    (key, iv)
}

/// ChaCha20 encrypt or decrypt (symmetric -- same operation both ways).
///
/// `iv` is 16 bytes: `[counter:u32 LE][nonce:12 bytes]`, matching the
/// Python `cryptography` library's ChaCha20 nonce format.
pub fn chacha20_crypt(data: &mut [u8], key: &[u8; 32], iv: &[u8; 16]) {
    let counter = u32::from_le_bytes(iv[..4].try_into().unwrap());
    let nonce: &[u8; 12] = iv[4..].try_into().unwrap();

    let key_arr: &chacha20::Key = key.into();
    let nonce_arr: &chacha20::Nonce = nonce.into();
    let mut cipher = ChaCha20::new(key_arr, nonce_arr);

    if counter != 0 {
        cipher.seek(counter as u64 * 64);
    }
    cipher.apply_keystream(data);
}

/// Decrypt in-place using a key derived from the given filename.
pub fn decrypt_inplace(data: &mut [u8], filename: &str) {
    let (key, iv) = derive_key_iv(filename);
    chacha20_crypt(data, &key, &iv);
}

/// Decrypt to a new Vec using a key derived from the given filename.
pub fn decrypt(data: &[u8], filename: &str) -> Vec<u8> {
    let mut out = data.to_vec();
    decrypt_inplace(&mut out, filename);
    out
}

/// Encrypt to a new Vec (identical to decrypt -- ChaCha20 is symmetric).
pub fn encrypt(data: &[u8], filename: &str) -> Vec<u8> {
    decrypt(data, filename)
}

/// Returns true if the given file path should be ChaCha20-encrypted.
pub fn is_encrypted(path: &str) -> bool {
    let lower = path.to_lowercase();
    let ext = std::path::Path::new(&lower)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    matches!(
        ext,
        "xml" | "paloc" | "css" | "html" | "thtml" | "pami" | "uianiminit"
            | "spline2d" | "spline" | "mi" | "txt"
            | "app_xml" | "pac_xml" | "prefabdata_xml"
    )
}
