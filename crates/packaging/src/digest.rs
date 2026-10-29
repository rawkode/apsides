//! SHA-256 digests in the `sha256:<64 lowercase hex>` form every Apsides
//! document uses.

use sha2::{Digest, Sha256};
use std::io::{self, Read};

/// The lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// `sha256:` followed by the lowercase hex SHA-256 of `bytes`.
#[must_use]
pub fn digest(bytes: &[u8]) -> String {
    format!("sha256:{}", sha256_hex(bytes))
}

/// Whether `value` is exactly 64 lowercase hex characters.
#[must_use]
pub fn is_hex64(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Whether `value` is `sha256:` followed by 64 lowercase hex characters.
#[must_use]
pub fn is_digest(value: &str) -> bool {
    value.strip_prefix("sha256:").is_some_and(is_hex64)
}

/// A reader that hashes and counts every byte it yields.
pub struct HashingReader<R> {
    inner: R,
    hasher: Sha256,
    read: u64,
}

impl<R: Read> HashingReader<R> {
    pub fn new(inner: R) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            read: 0,
        }
    }

    /// The digest of everything read so far, and how many bytes that was.
    pub fn finish(self) -> (String, u64) {
        (format!("sha256:{:x}", self.hasher.finalize()), self.read)
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.hasher.update(&buffer[..count]);
        self.read += count as u64;
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digests_are_prefixed_lowercase_hex() {
        assert_eq!(
            digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert!(is_digest(&digest(b"")));
        for value in [
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            "sha256:BA7816BF8F01CFEA414140DE5DAE2223B00361A396177A9CB410FF61F20015AD",
            "sha256:ba7816bf",
            "sha512:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
        ] {
            assert!(!is_digest(value), "{value} was accepted");
        }
    }

    #[test]
    fn the_hashing_reader_hashes_what_it_yields() -> io::Result<()> {
        let mut reader = HashingReader::new(&b"abcdef"[..]);
        let mut first = [0_u8; 3];
        reader.read_exact(&mut first)?;
        assert_eq!(reader.finish(), (digest(b"abc"), 3));
        let mut reader = HashingReader::new(&b"abcdef"[..]);
        io::copy(&mut reader, &mut io::sink())?;
        assert_eq!(reader.finish(), (digest(b"abcdef"), 6));
        Ok(())
    }
}
