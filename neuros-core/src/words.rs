//! Low-level encoding for the WOID database files.
//!
//! Every file is a sequence of big-endian 16-bit words and every pointer
//! counts words, not bytes.

/// Field delimiter `#`.
pub const FIELD_DELIM: u16 = 0x0023;
/// Bag (multi-value) delimiter `$`.
pub const BAG_DELIM: u16 = 0x0024;
/// End of record `%`.
pub const END_OF_RECORD: u16 = 0x0025;
/// Escaper `/`.
pub const ESCAPER: u16 = 0x002F;

/// Record flag for a live record (bit 15 is always set).
pub const RECORD_FLAG: u16 = 0x8000;
/// Record flag bit marking a deleted record.
pub const RECORD_DELETED: u16 = 0x0001;

pub const SAI_SIGNATURE: u32 = 0x0518_1971;
pub const PAI_SIGNATURE: u32 = 0x0116_2002;

/// Is this word one of the four special words that must be escaped
/// inside field data?
pub fn is_special(w: u16) -> bool {
    matches!(w, FIELD_DELIM | BAG_DELIM | END_OF_RECORD | ESCAPER)
}

/// A growable buffer of 16-bit words.
#[derive(Debug, Default, Clone)]
pub struct WordBuf {
    words: Vec<u16>,
}

impl WordBuf {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current length in words; also the word pointer of the next push.
    pub fn len(&self) -> u32 {
        self.words.len() as u32
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Push a raw word (no escaping).
    pub fn word(&mut self, w: u16) {
        self.words.push(w);
    }

    /// Push a raw 32-bit value as two words, high word first (no escaping).
    pub fn u32(&mut self, v: u32) {
        self.words.push((v >> 16) as u16);
        self.words.push(v as u16);
    }

    /// Push words of field data, escaping any special words.
    pub fn escaped(&mut self, data: &[u16]) {
        for &w in data {
            if is_special(w) {
                self.words.push(ESCAPER);
            }
            self.words.push(w);
        }
    }

    /// Push a 32-bit value as field data (escaped).
    pub fn escaped_u32(&mut self, v: u32) {
        self.escaped(&[(v >> 16) as u16, v as u16]);
    }

    /// Push a string as field data in `sz` format (escaped).
    pub fn escaped_sz(&mut self, s: &str) {
        self.escaped(&sz_words(s));
    }

    /// Append raw bytes (must be even-length).
    pub fn raw_bytes(&mut self, bytes: &[u8]) {
        assert!(bytes.len().is_multiple_of(2), "raw bytes must be word aligned");
        self.words.extend(bytes.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])));
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.words.iter().flat_map(|w| w.to_be_bytes()).collect()
    }
}

/// Encode a string in `sz` format: null-padded to a word boundary, then
/// terminated with a null word.  `"foo"` → `66 6f 6f 00 00 00`.
pub fn sz_words(s: &str) -> Vec<u16> {
    let mut bytes = s.as_bytes().to_vec();
    if bytes.len() % 2 == 1 {
        bytes.push(0);
    }
    bytes.extend_from_slice(&[0, 0]);
    bytes.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes([c[0], c[1]])).collect()
}

/// Decode `sz` data: everything up to the first NUL byte.
pub fn decode_sz(words: &[u16]) -> String {
    let bytes: Vec<u8> = words.iter().flat_map(|w| w.to_be_bytes()).collect();
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Read a big-endian word at a word offset.
pub fn word_at(bytes: &[u8], ptr: u32) -> Option<u16> {
    let i = ptr as usize * 2;
    bytes.get(i..i + 2).map(|b| u16::from_be_bytes([b[0], b[1]]))
}

/// Read a big-endian 32-bit value at a word offset.
pub fn u32_at(bytes: &[u8], ptr: u32) -> Option<u32> {
    Some(((word_at(bytes, ptr)? as u32) << 16) | word_at(bytes, ptr + 1)? as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sz_matches_spec_example() {
        let mut b = WordBuf::new();
        b.escaped_sz("foo");
        assert_eq!(b.into_bytes(), vec![0x66, 0x6f, 0x6f, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn sz_even_length_gets_null_word() {
        assert_eq!(sz_words("ab"), vec![0x6162, 0x0000]);
    }

    #[test]
    fn pointer_escaping_matches_spec_example() {
        let mut b = WordBuf::new();
        b.escaped_u32(0x0000_0023);
        assert_eq!(b.into_bytes(), vec![0x00, 0x00, 0x00, 0x2f, 0x00, 0x23]);
    }

    #[test]
    fn decode_roundtrip() {
        assert_eq!(decode_sz(&sz_words("Hello")), "Hello");
        assert_eq!(decode_sz(&sz_words("")), "");
    }
}
