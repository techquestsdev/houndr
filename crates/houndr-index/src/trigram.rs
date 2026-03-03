/// A trigram is 3 bytes packed into the lower 24 bits of a u32.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Trigram(pub u32);

impl Trigram {
    /// Pack 3 bytes into a trigram.
    #[inline]
    pub fn new(a: u8, b: u8, c: u8) -> Self {
        Self(((a as u32) << 16) | ((b as u32) << 8) | (c as u32))
    }

    /// Unpack a trigram into its 3 bytes.
    #[inline]
    pub fn bytes(self) -> [u8; 3] {
        [(self.0 >> 16) as u8, (self.0 >> 8) as u8, self.0 as u8]
    }

    /// Extract all trigrams from a byte slice.
    pub fn extract(data: &[u8]) -> Vec<Trigram> {
        if data.len() < 3 {
            return Vec::new();
        }
        let mut trigrams = Vec::with_capacity(data.len() - 2);
        for window in data.windows(3) {
            trigrams.push(Trigram::new(window[0], window[1], window[2]));
        }
        trigrams
    }

    /// Extract unique trigrams from a byte slice.
    pub fn extract_unique(data: &[u8]) -> Vec<Trigram> {
        use rustc_hash::FxHashSet;
        if data.len() < 3 {
            return Vec::new();
        }
        let capacity = (data.len() - 2).min(8192);
        let mut seen = FxHashSet::with_capacity_and_hasher(capacity, Default::default());
        let mut trigrams = Vec::new();
        for window in data.windows(3) {
            let t = Trigram::new(window[0], window[1], window[2]);
            if seen.insert(t) {
                trigrams.push(t);
            }
        }
        trigrams
    }
}

impl std::fmt::Display for Trigram {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let b = self.bytes();
        // Show printable ASCII, otherwise hex
        for &byte in &b {
            if byte.is_ascii_graphic() || byte == b' ' {
                write!(f, "{}", byte as char)?;
            } else {
                write!(f, "\\x{:02x}", byte)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack() {
        let t = Trigram::new(b'f', b'o', b'o');
        assert_eq!(t.bytes(), [b'f', b'o', b'o']);
    }

    #[test]
    fn extract_basic() {
        let data = b"foobar";
        let trigrams = Trigram::extract(data);
        assert_eq!(trigrams.len(), 4); // foo, oob, oba, bar
        assert_eq!(trigrams[0], Trigram::new(b'f', b'o', b'o'));
        assert_eq!(trigrams[1], Trigram::new(b'o', b'o', b'b'));
        assert_eq!(trigrams[2], Trigram::new(b'o', b'b', b'a'));
        assert_eq!(trigrams[3], Trigram::new(b'b', b'a', b'r'));
    }

    #[test]
    fn extract_short() {
        assert!(Trigram::extract(b"").is_empty());
        assert!(Trigram::extract(b"ab").is_empty());
    }

    #[test]
    fn extract_unique_deduplicates() {
        let data = b"aaa"; // only one unique trigram: aaa
        let unique = Trigram::extract_unique(data);
        assert_eq!(unique.len(), 1);
        let data = b"aaaa"; // aaa appears twice but unique returns 1
        let unique = Trigram::extract_unique(data);
        assert_eq!(unique.len(), 1);
    }

    #[test]
    fn ordering() {
        let a = Trigram::new(b'a', b'a', b'a');
        let b = Trigram::new(b'a', b'a', b'b');
        assert!(a < b);
    }

    #[test]
    fn display() {
        let t = Trigram::new(b'f', b'o', b'o');
        assert_eq!(format!("{}", t), "foo");
    }
}
