//! A hand-written USTAR writer.
//!
//! Every byte it writes is fixed here, independent of any tar crate's
//! version, and equals what Python 3.14's `tarfile` wrote in USTAR format for
//! the same entries. Only directories and regular files are written, with
//! uid and gid 0, no link name, no prefix and no PAX headers.

use std::io::{self, Read, Write};

const BLOCK: usize = 512;
/// The archive is padded to a multiple of this many bytes, as `tarfile` does.
const RECORD: u64 = 20 * BLOCK as u64;
/// 8^11: the first value 11 octal digits cannot hold.
const OCTAL_11: u64 = 1 << 33;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Directory,
    File,
}

#[derive(Clone, Copy, Debug)]
pub struct Entry<'a> {
    /// The path without a trailing `/`; one is added for a directory.
    pub name: &'a str,
    pub kind: Kind,
    pub mode: u32,
    pub mtime: u64,
    /// The user and group name, the same for both.
    pub owner: &'a str,
    /// Ignored for a directory, whose size is 0.
    pub size: u64,
}

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum UstarError {
    #[error("tar entry name {0:?} is not ASCII of at most 100 bytes without NUL")]
    Name(String),
    #[error("tar entry owner {0:?} is not ASCII of at most 32 bytes without NUL")]
    Owner(String),
    #[error("tar entry mode {0:o} has bits outside 0o7777")]
    Mode(u32),
    #[error("tar entry size {0} does not fit in 11 octal digits")]
    Size(u64),
    #[error("tar entry mtime {0} does not fit in 11 octal digits")]
    Mtime(u64),
}

/// The 512-byte header for `entry`.
///
/// # Errors
/// Fails on a name, owner, mode, size or mtime the format cannot hold.
pub fn header(entry: &Entry<'_>) -> Result<[u8; BLOCK], UstarError> {
    let name = match entry.kind {
        Kind::Directory => format!("{}/", entry.name),
        Kind::File => entry.name.to_owned(),
    };
    if name.len() > 100 || !name.is_ascii() || name.contains('\0') {
        return Err(UstarError::Name(name));
    }
    if entry.owner.len() > 32 || !entry.owner.is_ascii() || entry.owner.contains('\0') {
        return Err(UstarError::Owner(entry.owner.to_owned()));
    }
    if entry.mode > 0o7777 {
        return Err(UstarError::Mode(entry.mode));
    }
    let size = match entry.kind {
        Kind::Directory => 0,
        Kind::File => entry.size,
    };
    if size >= OCTAL_11 {
        return Err(UstarError::Size(size));
    }
    if entry.mtime >= OCTAL_11 {
        return Err(UstarError::Mtime(entry.mtime));
    }
    let mut block = [0_u8; BLOCK];
    let mut put = |offset: usize, bytes: &[u8]| {
        block[offset..offset + bytes.len()].copy_from_slice(bytes);
    };
    put(0, name.as_bytes());
    put(100, format!("{:07o}\0", entry.mode).as_bytes());
    put(108, b"0000000\0");
    put(116, b"0000000\0");
    put(124, format!("{size:011o}\0").as_bytes());
    put(136, format!("{:011o}\0", entry.mtime).as_bytes());
    put(148, b"        ");
    put(
        156,
        match entry.kind {
            Kind::Directory => b"5",
            Kind::File => b"0",
        },
    );
    put(257, b"ustar\x0000");
    put(265, entry.owner.as_bytes());
    put(297, entry.owner.as_bytes());
    let checksum: u32 = block.iter().map(|byte| u32::from(*byte)).sum();
    block[148..156].copy_from_slice(format!("{checksum:06o}\0 ").as_bytes());
    Ok(block)
}

/// Writes USTAR entries to `inner`.
pub struct Writer<W: Write> {
    inner: W,
    written: u64,
}

impl<W: Write> Writer<W> {
    pub const fn new(inner: W) -> Self {
        Self { inner, written: 0 }
    }

    /// Appends a directory entry for `name` (written with a trailing `/`).
    ///
    /// # Errors
    /// Fails on an entry the format cannot hold and on a write error.
    pub fn directory(&mut self, name: &str, mode: u32, mtime: u64, owner: &str) -> io::Result<()> {
        self.header(&Entry {
            name,
            kind: Kind::Directory,
            mode,
            mtime,
            owner,
            size: 0,
        })
    }

    /// Appends a regular file of exactly `size` bytes read from `data`, then
    /// pads it with NULs to a block boundary.
    ///
    /// # Errors
    /// Fails with [`io::ErrorKind::UnexpectedEof`] when `data` ends before
    /// `size` bytes, on an entry the format cannot hold and on a read or write
    /// error.
    pub fn file(
        &mut self,
        name: &str,
        mode: u32,
        mtime: u64,
        owner: &str,
        size: u64,
        data: &mut dyn Read,
    ) -> io::Result<()> {
        self.header(&Entry {
            name,
            kind: Kind::File,
            mode,
            mtime,
            owner,
            size,
        })?;
        let copied = io::copy(&mut data.take(size), &mut self.inner)?;
        self.written += copied;
        if copied != size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                format!("{name} ended after {copied} of {size} bytes"),
            ));
        }
        let partial = size % BLOCK as u64;
        if partial != 0 {
            self.zeros(BLOCK as u64 - partial)?;
        }
        Ok(())
    }

    /// Writes the two zero end-of-archive blocks, pads the archive with NULs
    /// to a multiple of 10240 bytes and returns the inner writer.
    ///
    /// # Errors
    /// Fails on a write error.
    pub fn finish(mut self) -> io::Result<W> {
        self.zeros(2 * BLOCK as u64)?;
        let partial = self.written % RECORD;
        if partial != 0 {
            self.zeros(RECORD - partial)?;
        }
        Ok(self.inner)
    }

    fn header(&mut self, entry: &Entry<'_>) -> io::Result<()> {
        let block =
            header(entry).map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        self.inner.write_all(&block)?;
        self.written += BLOCK as u64;
        Ok(())
    }

    fn zeros(&mut self, count: u64) -> io::Result<()> {
        io::copy(&mut io::repeat(0).take(count), &mut self.inner)?;
        self.written += count;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult = Result<(), Box<dyn std::error::Error>>;

    fn field(block: &[u8], offset: usize, length: usize) -> &[u8] {
        &block[offset..offset + length]
    }

    #[test]
    fn a_directory_header_matches_python_tarfile() -> TestResult {
        let block = header(&Entry {
            name: "usr",
            kind: Kind::Directory,
            mode: 0o755,
            mtime: 0,
            owner: "root",
            size: 99,
        })?;
        assert_eq!(field(&block, 0, 5), b"usr/\0");
        assert_eq!(field(&block, 100, 8), b"0000755\0");
        assert_eq!(field(&block, 108, 8), b"0000000\0");
        assert_eq!(field(&block, 116, 8), b"0000000\0");
        assert_eq!(field(&block, 124, 12), b"00000000000\0");
        assert_eq!(field(&block, 136, 12), b"00000000000\0");
        assert_eq!(field(&block, 148, 8), b"010366\0 ");
        assert_eq!(field(&block, 156, 1), b"5");
        assert_eq!(field(&block, 257, 8), b"ustar\x0000");
        assert_eq!(field(&block, 265, 5), b"root\0");
        assert_eq!(field(&block, 297, 5), b"root\0");
        assert_eq!(field(&block, 329, 16), [0_u8; 16]);
        Ok(())
    }

    #[test]
    fn unrepresentable_entries_are_rejected() {
        let entry = Entry {
            name: "a",
            kind: Kind::File,
            mode: 0o644,
            mtime: 0,
            owner: "",
            size: 0,
        };
        let long = "n".repeat(100);
        let cases = [
            Entry {
                name: "é", ..entry
            },
            Entry {
                name: &long,
                kind: Kind::Directory,
                ..entry
            },
            Entry {
                name: "a\0b",
                ..entry
            },
            Entry {
                owner: "rööt",
                ..entry
            },
            Entry {
                mode: 0o10000,
                ..entry
            },
            Entry {
                size: OCTAL_11,
                ..entry
            },
            Entry {
                mtime: OCTAL_11,
                ..entry
            },
        ];
        for case in cases {
            assert!(header(&case).is_err(), "{case:?} was accepted");
        }
        assert!(
            header(&Entry {
                name: &long,
                ..entry
            })
            .is_ok()
        );
        assert!(
            header(&Entry {
                mtime: OCTAL_11 - 1,
                size: OCTAL_11 - 1,
                ..entry
            })
            .is_ok()
        );
    }

    #[test]
    fn files_are_padded_and_archives_end_on_a_record() -> TestResult {
        let mut writer = Writer::new(Vec::new());
        writer.directory("usr", 0o755, 0, "root")?;
        writer.file("usr/a", 0o644, 0, "root", 3, &mut &b"abcdef"[..])?;
        let archive = writer.finish()?;
        assert_eq!(archive.len(), 10240);
        assert_eq!(&archive[1024..1027], b"abc");
        assert!(archive[1027..].iter().all(|byte| *byte == 0));
        Ok(())
    }

    #[test]
    fn a_short_file_is_an_error() {
        let mut writer = Writer::new(Vec::new());
        let error = writer
            .file("a", 0o644, 0, "", 4, &mut &b"abc"[..])
            .err()
            .map(|error| error.kind());
        assert_eq!(error, Some(io::ErrorKind::UnexpectedEof));
    }
}
