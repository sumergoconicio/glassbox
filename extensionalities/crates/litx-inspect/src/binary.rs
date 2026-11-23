//! Bounded random access: container payloads are skipped, never eagerly loaded.
use crate::xml;
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

pub(crate) struct Source {
    file: File,
    pub len: u64,
    read: usize,
}
impl Source {
    /// Opens one validated regular file through the shared no-follow guard.
    ///
    /// Accepts a source path. Returns a bounded random-access reader after the
    /// shared guard rejects every symbolic-link component and non-regular
    /// input, opens the final component without following links or blocking
    /// on FIFOs and device nodes (Linux custom flags), and re-checks the
    /// opened descriptor itself against regular files. Path validation and
    /// the open race with concurrent path replacement; the post-open
    /// descriptor check is authoritative for what is actually read, and the
    /// physical byte cap stays enforced. On non-Linux Unix the open carries
    /// no custom flags, so the guard relies on path-level link rejection and
    /// the descriptor check alone.
    pub fn open(path: &Path) -> xml::Result<Self> {
        let (absolute, _) = crate::hashing::validate_regular_file(path)?;
        let file = crate::hashing::open_no_follow(&absolute)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err("input is not a regular file".into());
        }
        let len = metadata.len();
        if len > 256 << 20 {
            return Err("container physical byte cap exceeded".into());
        }
        Ok(Self { file, len, read: 0 })
    }
    pub fn range(&mut self, offset: u64, size: usize) -> xml::Result<Vec<u8>> {
        if size > 16 << 20
            || offset
                .checked_add(size as u64)
                .is_none_or(|end| end > self.len)
        {
            return Err("container truncated or per-range byte cap exceeded".into());
        }
        self.read = self
            .read
            .checked_add(size)
            .ok_or("container read overflow")?;
        if self.read > 64 << 20 {
            return Err("container cumulative read byte cap exceeded".into());
        }
        self.file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; size];
        self.file.read_exact(&mut bytes)?;
        Ok(bytes)
    }
}

pub(crate) fn slice(bytes: &[u8], offset: usize, len: usize) -> xml::Result<&[u8]> {
    bytes
        .get(offset..offset.checked_add(len).ok_or("binary offset overflow")?)
        .ok_or_else(|| "truncated binary field".into())
}
pub(crate) fn be16(b: &[u8], i: usize) -> xml::Result<u16> {
    Ok(u16::from_be_bytes(slice(b, i, 2)?.try_into()?))
}
pub(crate) fn be32(b: &[u8], i: usize) -> xml::Result<u32> {
    Ok(u32::from_be_bytes(slice(b, i, 4)?.try_into()?))
}
