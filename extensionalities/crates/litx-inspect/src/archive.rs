use crate::{ArchiveEntry, InspectReport};
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Seek, SeekFrom},
    path::Path,
};
use thiserror::Error;
use zip::ZipArchive;

pub(crate) const MAX_ENTRIES: usize = 4096;
pub(crate) const MAX_ENTRY_BYTES: u64 = 16 << 20;
const MAX_TOTAL_BYTES: u64 = 256 << 20;

#[derive(Debug, Error)]
pub(crate) enum ArchiveError {
    #[error("{0}")]
    Limit(&'static str),
    #[error(transparent)]
    Zip(#[from] zip::result::ZipError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

pub(crate) struct Archive {
    inner: ZipArchive<File>,
    pub names: Vec<String>,
    read_bytes: u64,
}

impl Archive {
    pub fn open(path: &Path, report: &mut InspectReport) -> Result<Self, ArchiveError> {
        let mut file = File::open(path)?;
        if file.metadata()?.len() > MAX_TOTAL_BYTES {
            return Err(ArchiveError::Limit("ZIP physical byte cap exceeded"));
        }
        check_directory(&mut file)?;
        let mut inner = ZipArchive::new(file)?;
        if inner.len() > MAX_ENTRIES {
            return Err(ArchiveError::Limit("ZIP entry-count cap exceeded"));
        }
        let mut total = 0u64;
        let mut name_bytes = 0usize;
        let mut names = Vec::new();
        let mut seen = HashSet::new();
        for i in 0..inner.len() {
            let entry = inner.by_index_raw(i)?;
            let name = entry.name()?.into_owned();
            total = total
                .checked_add(entry.size())
                .ok_or(ArchiveError::Limit("ZIP size overflow"))?;
            if total > MAX_TOTAL_BYTES {
                return Err(ArchiveError::Limit(
                    "ZIP total-uncompressed-bytes cap exceeded",
                ));
            }
            name_bytes += name.len();
            if name.len() > 4096 || name_bytes > 1 << 20 {
                return Err(ArchiveError::Limit("ZIP entry-name byte cap exceeded"));
            }
            if !seen.insert(name.clone()) {
                return Err(ArchiveError::Limit(
                    "duplicate ZIP entry names are ambiguous",
                ));
            }
            if entry.enclosed_name().is_none() || name.contains('\\') {
                report.error(
                    "archive",
                    format!("unsafe entry name retained as data: {name:?}"),
                );
            }
            names.push(name.clone());
            let timestamp = entry.last_modified().map(|d| {
                format!(
                    "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                    d.year(),
                    d.month(),
                    d.day(),
                    d.hour(),
                    d.minute(),
                    d.second()
                )
            });
            report.entries.push(ArchiveEntry {
                name,
                size: entry.size(),
                compressed_size: entry.compressed_size(),
                timestamp,
            });
        }
        report.counts.entries = Some(names.len());
        Ok(Self {
            inner,
            names,
            read_bytes: 0,
        })
    }

    pub fn read(&mut self, name: &str) -> Result<Option<Vec<u8>>, ArchiveError> {
        if !self.names.iter().any(|n| n == name) {
            return Ok(None);
        }
        let entry = self.inner.by_name(name)?;
        if entry.size() > MAX_ENTRY_BYTES {
            return Err(ArchiveError::Limit("ZIP per-entry read cap exceeded"));
        }
        let mut bytes = Vec::new();
        entry.take(MAX_ENTRY_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_ENTRY_BYTES {
            return Err(ArchiveError::Limit("ZIP expanded entry exceeds read cap"));
        }
        self.read_bytes += bytes.len() as u64;
        if self.read_bytes > MAX_TOTAL_BYTES {
            return Err(ArchiveError::Limit("ZIP cumulative read cap exceeded"));
        }
        Ok(Some(bytes))
    }
}

fn check_directory(file: &mut File) -> Result<(), ArchiveError> {
    let tail_len = file.metadata()?.len().min(65_557) as usize;
    file.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = Vec::with_capacity(tail_len);
    file.read_to_end(&mut tail)?;
    let u16_at = |i: usize| u16::from_le_bytes([tail[i], tail[i + 1]]);
    let mut found = None;
    if tail.len() >= 22 {
        for i in (0..=tail.len() - 22).rev() {
            if &tail[i..i + 4] == b"PK\x05\x06" && i + 22 + u16_at(i + 20) as usize == tail.len() {
                found = Some(i);
                break;
            }
        }
    }
    let i = found.ok_or(ArchiveError::Limit(
        "ZIP missing or truncated end-of-directory",
    ))?;
    // ZIP64 has a separate count/size record; do not trust the classic fields.
    if i >= 20 && &tail[i - 20..i - 16] == b"PK\x06\x07" {
        return Err(ArchiveError::Limit(
            "ZIP64 directories require a bounded ZIP64 adapter",
        ));
    }
    if u16_at(i + 10) as usize > MAX_ENTRIES {
        return Err(ArchiveError::Limit(
            "ZIP entry-count cap exceeded before directory allocation",
        ));
    }
    let directory_bytes =
        u32::from_le_bytes(tail[i + 12..i + 16].try_into().expect("fixed EOCD slice"));
    if directory_bytes > 2 << 20 {
        return Err(ArchiveError::Limit("ZIP directory byte cap exceeded"));
    }
    if u16_at(i + 4) != 0 || u16_at(i + 6) != 0 || u16_at(i + 8) != u16_at(i + 10) {
        return Err(ArchiveError::Limit("multi-disk ZIP is unsupported"));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(())
}
