//! Module: glassbox-inspect streamed source identity.
//! File: hashing.rs.
//! Responsibility: Hash no-follow regular files through bounded reads and qualify source consistency.

use crate::reports::{
    Completeness, ConsistencyStatus, Diagnostic, DiagnosticSeverity, DiagnosticStage, DigestReport,
    FactSource, NativeUnit, QualifiedCount, SnapshotGuarantee, SourceIdentity,
};
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

/// Reusable hash buffer size; source retention never grows with file length.
pub const HASH_BUFFER_SIZE: usize = 64 * 1024;
#[cfg(target_os = "linux")]
const O_CLOEXEC: i32 = 0x80000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const O_NOFOLLOW: i32 = 0x8000;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
const O_NOFOLLOW: i32 = 0x20000;
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0x800;

/// Result of one opened-handle hashing pass.
pub struct HashOutcome {
    /// Filesystem identity observed before and after streaming.
    pub identity: SourceIdentity,
    /// Digest facts; no digest is trusted after detected source mutation.
    pub digest: DigestReport,
    /// Input and hashing diagnostics for the metadata report.
    pub diagnostics: Vec<Diagnostic>,
    /// Whether format detection and metadata readers may proceed.
    pub input_valid: bool,
}

/// One filesystem observation tied to an opened file handle or no-follow path.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Observation {
    length: u64,
    modified: Option<std::time::SystemTime>,
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

/// Captures identity and modification facts from filesystem metadata.
///
/// Accepts metadata from an opened handle or a no-follow path query. Returns
/// the comparable observation. Unsupported modification timestamps become
/// None; this helper performs no I/O and has no additional failure states.
fn observation(metadata: &std::fs::Metadata) -> Observation {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    Observation {
        length: metadata.len(),
        modified: metadata.modified().ok(),
        #[cfg(unix)]
        device: metadata.dev(),
        #[cfg(unix)]
        inode: metadata.ino(),
    }
}

/// Formats an opaque host-local file identifier without hashing path text.
///
/// Accepts the initial opened-handle observation. Returns Unix device/inode
/// identity when available and None on other hosts. No I/O or errors occur.
fn opaque_id(_before: &Observation) -> Option<String> {
    #[cfg(unix)]
    {
        Some(format!("{:x}:{:x}", _before.device, _before.inode))
    }
    #[cfg(not(unix))]
    {
        None
    }
}

/// Constructs a content-free diagnostic for input or hashing.
///
/// Accepts stage, severity, stable code, and message. Returns the typed
/// diagnostic with no native-unit location. This helper cannot fail.
fn diagnostic(
    stage: DiagnosticStage,
    severity: DiagnosticSeverity,
    code: &str,
    message: impl ToString,
) -> Diagnostic {
    Diagnostic {
        stage,
        severity,
        code: code.to_owned(),
        message: message.to_string(),
        unit_index: None,
    }
}

/// Produces an absolute lexical path without resolving symbolic links.
///
/// Accepts a path and current working directory. Returns an absolute path with
/// CurDir and ParentDir components folded. Missing current directories and
/// excessive parent traversal return an I/O error; no source file is opened.
fn absolute_lexical(path: &Path) -> std::io::Result<PathBuf> {
    let supplied = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in supplied.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path escapes filesystem root",
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// Rejects every existing symbolic-link component in an absolute path.
///
/// Accepts an absolute lexical path. Returns unit when all existing components
/// are non-links. Filesystem query failures and links return errors; a missing
/// suffix is left for the subsequent no-follow open to report.
fn reject_symlink_components(path: &Path) -> std::io::Result<()> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "symbolic-link path component rejected: {}",
                        current.display()
                    ),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Opens one regular file without following its final symbolic link.
///
/// Accepts an absolute path whose existing components were checked for links.
/// Returns the owned read handle. Open failures, including final-component
/// links and non-regular files, are returned without reading source bytes.
/// The nonblocking flag keeps opens of FIFOs and device nodes from blocking
/// the reader; the opened descriptor must still be re-checked by the caller
/// against regular files, for which the flag has no effect. The nonblocking
/// and no-follow flags are Linux-only: on other Unix hosts the open carries
/// no custom flags, so callers rely on path-level link rejection and the
/// descriptor check alone. The open races with concurrent path replacement;
/// only the post-open descriptor check is authoritative for what is read.
pub(crate) fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC);
    }
    options.open(path)
}

/// Validates the no-follow regular-file boundary without opening a descriptor.
///
/// Accepts a caller-supplied path. Returns the lexical absolute path and byte
/// length when every existing component is a non-link and the final component
/// is a regular file. Link components, non-regular inputs such as directories
/// or FIFOs, and filesystem failures return errors. No descriptor is opened
/// and no source byte is read; like every path check it cannot freeze the
/// filesystem against races, so readers still own their own safety caps.
pub(crate) fn validate_regular_file(path: &Path) -> std::io::Result<(PathBuf, u64)> {
    let absolute = absolute_lexical(path)?;
    reject_symlink_components(&absolute)?;
    let metadata = std::fs::symlink_metadata(&absolute)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "input is not a regular file",
        ));
    }
    Ok((absolute, metadata.len()))
}

/// Streams SHA-256 through one 64 KiB buffer and rechecks handle and path identity.
///
/// Accepts a regular-file path. Returns source identity, a qualified digest,
/// diagnostics, and whether metadata reading can proceed. Open/read/metadata
/// failures and detected identity, length, modification, or path replacement
/// are reported; changed sources never retain trusted digests. These checks do
/// not create an immutable snapshot against adversarial concurrent writes.
pub fn hash_file(path: &Path) -> HashOutcome {
    hash_file_with_recheck(path, || {})
}

/// Runs hashing with one deterministic pre-recheck callback for mutation tests.
///
/// Accepts a source path and closure invoked after streaming and before the
/// final observations. Returns the ordinary hash outcome. All production I/O
/// failures and mutation outcomes match hash_file; the callback cannot fail.
fn hash_file_with_recheck(path: &Path, before_recheck: impl FnOnce()) -> HashOutcome {
    let mut identity = SourceIdentity {
        path: absolute_lexical(path).unwrap_or_else(|_| path.to_path_buf()),
        ..SourceIdentity::default()
    };
    let mut digest = DigestReport::default();
    let mut diagnostics = Vec::new();
    let absolute = match absolute_lexical(path) {
        Ok(path) => path,
        Err(error) => {
            diagnostics.push(diagnostic(
                DiagnosticStage::Input,
                DiagnosticSeverity::Error,
                "input_path_failed",
                error,
            ));
            return HashOutcome {
                identity,
                digest,
                diagnostics,
                input_valid: false,
            };
        }
    };
    identity.path = absolute.clone();
    if let Err(error) = reject_symlink_components(&absolute) {
        diagnostics.push(diagnostic(
            DiagnosticStage::Input,
            DiagnosticSeverity::Error,
            "input_symlink_rejected",
            error,
        ));
        return HashOutcome {
            identity,
            digest,
            diagnostics,
            input_valid: false,
        };
    }
    let mut file = match open_no_follow(&absolute) {
        Ok(file) => file,
        Err(error) => {
            diagnostics.push(diagnostic(
                DiagnosticStage::Input,
                DiagnosticSeverity::Error,
                "input_open_failed",
                error,
            ));
            return HashOutcome {
                identity,
                digest,
                diagnostics,
                input_valid: false,
            };
        }
    };
    let before_metadata = match file.metadata() {
        Ok(metadata) => metadata,
        Err(error) => {
            diagnostics.push(diagnostic(
                DiagnosticStage::Input,
                DiagnosticSeverity::Error,
                "input_metadata_failed",
                error,
            ));
            return HashOutcome {
                identity,
                digest,
                diagnostics,
                input_valid: false,
            };
        }
    };
    if !before_metadata.is_file() {
        diagnostics.push(diagnostic(
            DiagnosticStage::Input,
            DiagnosticSeverity::Error,
            "input_not_regular_file",
            "input is not a regular file",
        ));
        return HashOutcome {
            identity,
            digest,
            diagnostics,
            input_valid: false,
        };
    }
    let before = observation(&before_metadata);
    identity.byte_size = Some(before.length);
    identity.opaque_file_id = opaque_id(&before);
    let initial_path_matches = std::fs::symlink_metadata(&absolute)
        .ok()
        .filter(|metadata| metadata.is_file())
        .is_some_and(|metadata| observation(&metadata) == before);
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    let mut total = 0_u64;
    let mut read_error = None;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                hasher.update(&buffer[..count]);
                total = total.saturating_add(count as u64);
            }
            Err(error) => {
                read_error = Some(error);
                break;
            }
        }
    }
    before_recheck();
    let after = file.metadata().map(|metadata| observation(&metadata)).ok();
    let recheck_failed = after.is_none();
    let path_matches = std::fs::symlink_metadata(&absolute)
        .ok()
        .filter(|metadata| metadata.is_file())
        .is_some_and(|metadata| observation(&metadata) == before);
    let changed = !initial_path_matches || after.as_ref() != Some(&before) || !path_matches;
    identity.consistency = if changed {
        ConsistencyStatus::SourceChanged
    } else {
        ConsistencyStatus::Consistent
    };
    digest.source_identity = identity.consistency;
    digest.bytes_hashed = QualifiedCount {
        unit: NativeUnit::Byte,
        value: Some(total),
        source: FactSource::ByteScan,
        completeness: if read_error.is_some() {
            Completeness::Partial
        } else {
            Completeness::Exact
        },
    };
    if let Some(error) = read_error {
        diagnostics.push(diagnostic(
            DiagnosticStage::Hashing,
            DiagnosticSeverity::Error,
            "hashing_failed",
            error,
        ));
        digest.consistency = ConsistencyStatus::Unavailable;
    } else if changed {
        diagnostics.push(diagnostic(
            DiagnosticStage::Hashing,
            DiagnosticSeverity::Warning,
            if recheck_failed {
                "source_recheck_failed"
            } else {
                "source_changed_during_hash"
            },
            if recheck_failed {
                "opened-handle metadata could not be rechecked; digest withheld"
            } else {
                "source identity, length, modification, or path identity changed during hashing; digest withheld"
            },
        ));
        digest.consistency = ConsistencyStatus::SourceChanged;
    } else {
        digest.digest = Some(format!("{:x}", hasher.finalize()));
        digest.consistency = ConsistencyStatus::Consistent;
    }
    digest.snapshot_guarantee = SnapshotGuarantee::NotImmutable;
    HashOutcome {
        identity,
        digest,
        diagnostics,
        input_valid: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Creates a unique ignored hashing fixture directory.
    ///
    /// Accepts a label. Returns a new target path. Creation failures panic;
    /// no prior evidence or user file is removed or overwritten.
    fn workdir(label: &str) -> PathBuf {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/tests")
            .join(format!(
                "hashing-{label}-{}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Proves an in-place truncate/rewrite cannot retain a trusted digest.
    ///
    /// Accepts no parameters. Returns unit after deterministic mutation at the
    /// pre-recheck boundary. Test filesystem failures panic.
    #[test]
    fn concurrent_rewrite_withholds_digest() {
        let directory = workdir("rewrite");
        let path = directory.join("mutable.txt");
        std::fs::write(&path, vec![b'a'; HASH_BUFFER_SIZE * 2]).unwrap();
        let outcome = hash_file_with_recheck(&path, || {
            std::fs::write(&path, b"replacement").unwrap();
        });
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
    }

    /// Proves path replacement is detected even while the opened inode remains stable.
    ///
    /// Accepts no parameters. Returns unit after swapping the path to a second
    /// regular file. Test filesystem failures panic; no unknown file is removed.
    #[test]
    fn concurrent_path_swap_withholds_digest() {
        let directory = workdir("swap");
        let path = directory.join("mutable.txt");
        let replacement = directory.join("replacement.txt");
        std::fs::write(&path, vec![b'a'; HASH_BUFFER_SIZE * 2]).unwrap();
        std::fs::write(&replacement, b"other").unwrap();
        let outcome = hash_file_with_recheck(&path, || {
            std::fs::rename(&replacement, &path).unwrap();
        });
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert!(outcome.digest.digest.is_none());
    }

    /// Proves a FIFO input cannot block the open path before rejection.
    ///
    /// Accepts no parameters. Returns unit after creating a named pipe with no
    /// writer and hashing it. A blocked open fails through the timeout channel
    /// instead of hanging; without `mkfifo` the invariant is left untested and
    /// the test returns. Test filesystem failures panic.
    #[cfg(target_os = "linux")]
    #[test]
    fn fifo_input_is_rejected_without_blocking() {
        let directory = workdir("fifo");
        let path = directory.join("bounded.fifo");
        let Ok(status) = std::process::Command::new("mkfifo").arg(&path).status() else {
            // mkfifo is unavailable in this environment; the descriptor-level
            // regular-file check remains the rejecting boundary.
            return;
        };
        assert!(status.success(), "mkfifo did not create the fixture pipe");
        let (sender, receiver) = std::sync::mpsc::channel();
        let moved = path.clone();
        std::thread::spawn(move || sender.send(hash_file(&moved)).unwrap());
        let outcome = receiver
            .recv_timeout(std::time::Duration::from_secs(30))
            .expect("opening a FIFO must not block; O_NONBLOCK is required");
        assert!(!outcome.input_valid);
        assert!(outcome.digest.digest.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "input_not_regular_file")
        );
    }
}
