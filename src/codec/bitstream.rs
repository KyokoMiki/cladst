//! Bit-level stream reader and writer for DST frame packing/unpacking.

/// Write individual bits MSB-first into a byte buffer.
pub struct BitstreamWriter {
    buffer: Vec<u8>,
    current_byte: u8,
    bits_in_current: u8,
    total_bits: usize,
}

impl BitstreamWriter {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            current_byte: 0,
            bits_in_current: 0,
            total_bits: 0,
        }
    }

    /// Current write position in bits.
    pub fn bit_position(&self) -> usize {
        self.total_bits
    }

    /// Write a single bit (0 or 1).
    #[inline]
    pub fn write_bit(&mut self, bit: u8) {
        self.current_byte = (self.current_byte << 1) | (bit & 1);
        self.bits_in_current += 1;
        self.total_bits += 1;
        if self.bits_in_current == 8 {
            self.buffer.push(self.current_byte);
            self.current_byte = 0;
            self.bits_in_current = 0;
        }
    }

    /// Write n_bits from value, MSB first.
    pub fn write_bits(&mut self, value: u32, n_bits: usize) {
        for i in (0..n_bits).rev() {
            self.write_bit(((value >> i) & 1) as u8);
        }
    }

    /// Bulk-write a slice of single-bit values (each 0 or 1).
    pub fn write_bit_slice(&mut self, bits: &[u8]) {
        for &bit in bits {
            self.write_bit(bit);
        }
    }

    /// Write n_bits as two's complement signed integer, MSB first.
    pub fn write_bits_signed(&mut self, value: i32, n_bits: usize) {
        let unsigned = if value < 0 {
            (value + (1 << n_bits)) as u32
        } else {
            value as u32
        };
        self.write_bits(unsigned, n_bits);
    }

    /// Write unary code: `value` zeros followed by a 1.
    pub fn write_unary(&mut self, value: u32) {
        for _ in 0..value {
            self.write_bit(0);
        }
        self.write_bit(1);
    }

    /// Flush to byte-aligned output and return bytes.
    pub fn get_bytes(&self) -> Vec<u8> {
        if self.bits_in_current > 0 {
            let pad = 8 - self.bits_in_current;
            let mut result = self.buffer.clone();
            result.push(self.current_byte << pad);
            result
        } else {
            self.buffer.clone()
        }
    }
}

impl Default for BitstreamWriter {
    fn default() -> Self {
        Self::new()
    }
}

/// Read individual bits MSB-first from a byte buffer.
pub struct BitstreamReader<'a> {
    data: &'a [u8],
    byte_pos: usize,
    bit_pos: u8, // 0-7, 0 = MSB
    total_bits_read: usize,
}

impl<'a> BitstreamReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            byte_pos: 0,
            bit_pos: 0,
            total_bits_read: 0,
        }
    }

    /// Current read position in bits.
    pub fn bit_position(&self) -> usize {
        self.total_bits_read
    }

    /// Number of bits remaining in the buffer.
    pub fn bits_remaining(&self) -> usize {
        self.data.len() * 8 - self.total_bits_read
    }

    /// Read a single bit (0 or 1).
    pub fn read_bit(&mut self) -> u8 {
        if self.byte_pos >= self.data.len() {
            return 0; // Past end of data, return 0
        }
        let bit = (self.data[self.byte_pos] >> (7 - self.bit_pos)) & 1;
        self.bit_pos += 1;
        self.total_bits_read += 1;
        if self.bit_pos == 8 {
            self.byte_pos += 1;
            self.bit_pos = 0;
        }
        bit
    }

    /// Read n_bits, MSB first, return as unsigned integer.
    pub fn read_bits(&mut self, n_bits: usize) -> u32 {
        let mut value: u32 = 0;
        for _ in 0..n_bits {
            value = (value << 1) | self.read_bit() as u32;
        }
        value
    }

    /// Read n_bits as two's complement signed integer.
    pub fn read_bits_signed(&mut self, n_bits: usize) -> i32 {
        let value = self.read_bits(n_bits);
        if value >= (1 << (n_bits - 1)) {
            value as i32 - (1 << n_bits)
        } else {
            value as i32
        }
    }

    /// Read unary code: count zeros until a 1 is found.
    pub fn read_unary(&mut self) -> u32 {
        let mut count: u32 = 0;
        while self.read_bit() == 0 {
            count += 1;
        }
        count
    }
}
