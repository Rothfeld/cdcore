//! LSB-first bit reader and writer for Vorbis packet parsing/building.
#![allow(dead_code)]

use crate::error::{Result, WmmoggError};

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
            return Err(WmmoggError::VorbisParse("unexpected end of bits".into()));
        }
        let bit = (self.data[self.byte_pos] >> self.bit_pos) & 1 != 0;
        self.bit_pos += 1;
        if self.bit_pos == 8 { self.bit_pos = 0; self.byte_pos += 1; }
        Ok(bit)
    }

    pub fn read_bits(&mut self, n: u8) -> Result<u32> {
        assert!(n <= 32);
        let mut val = 0u32;
        for i in 0..n {
            if self.read_bit()? { val |= 1 << i; }
        }
        Ok(val)
    }

    pub fn bits_left(&self) -> usize {
        (self.data.len() - self.byte_pos) * 8 - self.bit_pos as usize
    }

    /// Byte position (rounded up) — useful after reading to know how many bytes were consumed.
    pub fn byte_position(&self) -> usize {
        if self.bit_pos == 0 { self.byte_pos } else { self.byte_pos + 1 }
    }

    /// Remaining bytes (whole bytes only).
    pub fn remaining_bytes(&self) -> &'a [u8] {
        if self.bit_pos == 0 {
            &self.data[self.byte_pos..]
        } else {
            &self.data[self.byte_pos + 1..]
        }
    }
}

pub struct BitWriter {
    bytes: Vec<u8>,
    current: u8,
    bit_pos: u8,
}

impl BitWriter {
    pub fn new() -> Self {
        Self { bytes: Vec::new(), current: 0, bit_pos: 0 }
    }

    pub fn write_bit(&mut self, b: bool) {
        if b { self.current |= 1 << self.bit_pos; }
        self.bit_pos += 1;
        if self.bit_pos == 8 {
            self.bytes.push(self.current);
            self.current = 0;
            self.bit_pos = 0;
        }
    }

    pub fn write_bits(&mut self, val: u32, n: u8) {
        for i in 0..n {
            self.write_bit((val >> i) & 1 != 0);
        }
    }

    /// Flush any partial byte (zero-padded) and return the byte vector.
    pub fn finish(mut self) -> Vec<u8> {
        if self.bit_pos > 0 {
            self.bytes.push(self.current);
        }
        self.bytes
    }
}
