//! Module: glassbox-inspect streamed source identity.
//! File: hashing.rs.
//! Responsibility: Hash no-follow regular files through bounded reads, emit sampled fingerprints above the full-hash threshold, and qualify source consistency.

use crate::reports::{
    Completeness, ConsistencyStatus, Diagnostic, DiagnosticSeverity, DiagnosticStage, DigestReport,
    FactSource, NativeUnit, QualifiedCount, SampledFingerprint, SnapshotGuarantee, SourceIdentity,
};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

/// Reusable hash buffer size; source retention never grows with file length.
pub const HASH_BUFFER_SIZE: usize = 64 * 1024;
/// Maximum byte length that receives a full SHA-256 under [`HashPolicy::SampledAboveThreshold`].
///
/// Calibrated to 128 MiB (`134_217_728` bytes) by the `V-IO-01` release benchmark gate
/// after 256 MiB streamed SHA-256 measured 481.229 ms median (> 200 ms budget).
pub const DEFAULT_FULL_HASH_MAX_BYTES: u64 = 128 * 1024 * 1024;
/// Versioned scheme identifier for domain-separated sampled file fingerprints.
pub const SAMPLED_FINGERPRINT_SCHEME: &str = "glassbox-sampled-sha256/v1";
/// Byte length of each head, middle, and tail window hashed by [`SAMPLED_FINGERPRINT_SCHEME`].
pub const SAMPLED_WINDOW_BYTES: u64 = 1024 * 1024;

/// Controls whether files larger than [`DEFAULT_FULL_HASH_MAX_BYTES`] emit a sampled
/// fingerprint or stream a full-file SHA-256 digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HashPolicy {
    /// Stream full SHA-256 up to the threshold and emit a sampled fingerprint above it.
    #[default]
    SampledAboveThreshold,
    /// Stream a full-file SHA-256 digest regardless of byte length.
    Full,
}
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "x86_64"
    )
))]
const O_CLOEXEC: i32 = 0x80000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const O_NOFOLLOW: i32 = 0x8000;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
const O_NOFOLLOW: i32 = 0x20000;
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "arm",
        target_arch = "aarch64",
        target_arch = "x86",
        target_arch = "x86_64"
    )
))]
const O_NONBLOCK: i32 = 0x800;

#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const O_NONBLOCK: i32 = 0x0004;
#[cfg(any(
    target_os = "macos",
    target_os = "ios",
    target_os = "freebsd",
    target_os = "dragonfly",
    target_os = "openbsd",
    target_os = "netbsd"
))]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(any(target_os = "macos", target_os = "ios"))]
const O_CLOEXEC: i32 = 0x0100_0000;
#[cfg(target_os = "freebsd")]
const O_CLOEXEC: i32 = 0x0010_0000;
#[cfg(target_os = "dragonfly")]
const O_CLOEXEC: i32 = 0x0002_0000;
#[cfg(target_os = "openbsd")]
const O_CLOEXEC: i32 = 0x0001_0000;
#[cfg(target_os = "netbsd")]
const O_CLOEXEC: i32 = 0x0040_0000;

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
    /// Initial opened-handle identity snapshot for downstream read-binding verification.
    pub(crate) observation: Option<IdentityObservation>,
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

/// Full identity snapshot bound across hashing, metadata dispatch, and extraction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdentityObservation {
    base: Observation,
    #[cfg(unix)]
    ctime: i64,
    #[cfg(unix)]
    ctime_nsec: i64,
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

/// Captures a full read-binding identity snapshot including Unix status-change timestamps.
///
/// Accepts filesystem metadata from an opened handle or no-follow query.
/// Returns [`IdentityObservation`]. Performs no I/O and cannot fail.
fn identity_observation(metadata: &std::fs::Metadata) -> IdentityObservation {
    #[cfg(unix)]
    use std::os::unix::fs::MetadataExt;
    IdentityObservation {
        base: observation(metadata),
        #[cfg(unix)]
        ctime: metadata.ctime(),
        #[cfg(unix)]
        ctime_nsec: metadata.ctime_nsec(),
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

/// Classification of path-resolution rejections so hashing preserves stable
/// diagnostic codes while enforcing pre-fold symlink checks.
enum PathReject {
    Path(std::io::Error),
    Symlink(std::io::Error),
    Open(std::io::Error),
}

impl PathReject {
    /// Unwraps the underlying `std::io::Error` regardless of rejection class.
    ///
    /// Accepts `self`. Returns the inner I/O error without performing I/O.
    fn into_io(self) -> std::io::Error {
        match self {
            Self::Path(error) | Self::Symlink(error) | Self::Open(error) => error,
        }
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

/// Walks raw caller-supplied path components before any `..` folding, rejects
/// every symbolic-link component (including links preceding `..`), rejects
/// parent traversal through missing or non-directory components, and returns
/// the verified folded absolute path.
///
/// Accepts a caller-supplied path. Returns the validated absolute path when
/// every traversed component is a non-link. Missing current directories or
/// root escapes return [`PathReject::Path`]; symbolic-link components return
/// [`PathReject::Symlink`]; parent traversal across a missing or non-directory
/// prefix and non-`NotFound` `symlink_metadata` errors return
/// [`PathReject::Open`].
fn resolve_no_symlink_path(path: &Path) -> Result<PathBuf, PathReject> {
    let supplied = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(PathReject::Path)?
            .join(path)
    };
    let mut current = PathBuf::new();
    let mut missing_prefix = false;
    let mut current_is_dir = true;
    for component in supplied.components() {
        match component {
            Component::RootDir | Component::Prefix(_) => {
                current.push(component.as_os_str());
                current_is_dir = true;
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if missing_prefix {
                    return Err(PathReject::Open(std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        format!(
                            "missing directory before parent traversal: {}",
                            current.display()
                        ),
                    )));
                }
                if !current_is_dir {
                    return Err(PathReject::Open(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        format!(
                            "non-directory component before parent traversal: {}",
                            current.display()
                        ),
                    )));
                }
                if !current.pop() {
                    return Err(PathReject::Path(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path escapes filesystem root",
                    )));
                }
                current_is_dir = true;
            }
            Component::Normal(part) => {
                current.push(part);
                if !missing_prefix {
                    match std::fs::symlink_metadata(&current) {
                        Ok(metadata) if metadata.file_type().is_symlink() => {
                            return Err(PathReject::Symlink(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                format!(
                                    "symbolic-link path component rejected: {}",
                                    current.display()
                                ),
                            )));
                        }
                        Ok(metadata) => {
                            current_is_dir = metadata.is_dir();
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            missing_prefix = true;
                            current_is_dir = false;
                        }
                        Err(error) => return Err(PathReject::Open(error)),
                    }
                }
            }
        }
    }
    reject_symlink_components(&current)?;
    Ok(current)
}

/// Rejects every existing symbolic-link component in an absolute path.
///
/// Accepts an absolute lexical path. Returns `Ok(())` when all existing
/// components are non-links. Symbolic-link components return
/// [`PathReject::Symlink`]; non-`NotFound` filesystem query failures return
/// [`PathReject::Open`]; a missing suffix is left for the subsequent no-follow
/// open to report.
fn reject_symlink_components(path: &Path) -> Result<(), PathReject> {
    let mut current = PathBuf::new();
    for component in path.components() {
        current.push(component.as_os_str());
        if matches!(component, Component::RootDir | Component::Prefix(_)) {
            continue;
        }
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(PathReject::Symlink(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "symbolic-link path component rejected: {}",
                        current.display()
                    ),
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(PathReject::Open(error)),
        }
    }
    Ok(())
}

/// Opens one regular file without following its final symbolic link.
///
/// Accepts a file path whose existing components were checked for links.
/// Returns the owned read handle. Open failures, including final-component
/// links and non-regular files, are returned without reading source bytes.
/// The nonblocking flag keeps opens of FIFOs and device nodes from blocking
/// the reader; the opened descriptor must still be re-checked by the caller
/// against regular files, for which the flag has no effect. The `O_NOFOLLOW`,
/// `O_NONBLOCK`, and `O_CLOEXEC` flags are applied on Linux (`arm`, `aarch64`,
/// `x86`, `x86_64`), macOS, iOS, FreeBSD, DragonFly, OpenBSD, and NetBSD; on
/// other hosts the open carries no custom flags, so callers rely on path-level
/// link rejection and the descriptor check alone. The open races with
/// concurrent path replacement; only the post-open descriptor check is
/// authoritative for what is read.
pub(crate) fn open_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(any(
        all(
            target_os = "linux",
            any(
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "x86",
                target_arch = "x86_64"
            )
        ),
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(O_NOFOLLOW | O_NONBLOCK | O_CLOEXEC);
    }
    options.open(path)
}

/// Validates the no-follow regular-file boundary without opening a descriptor.
///
/// Accepts a caller-supplied path. Returns the validated symlink-free absolute
/// path and byte length when every traversed component (including components
/// preceding `..`) is a non-link and the final component is a regular file.
/// Link components, non-regular inputs such as directories or FIFOs, and
/// filesystem failures return errors. No descriptor is opened and no source
/// byte is read; like every path check it cannot freeze the filesystem against
/// races, so readers still own their own safety caps.
pub(crate) fn validate_regular_file(path: &Path) -> std::io::Result<(PathBuf, u64)> {
    let absolute = resolve_no_symlink_path(path).map_err(PathReject::into_io)?;
    let metadata = std::fs::symlink_metadata(&absolute)?;
    if !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "input is not a regular file",
        ));
    }
    Ok((absolute, metadata.len()))
}

/// Verifies that `path` still resolves without symbolic links to the exact
/// regular-file identity recorded in `expected`.
///
/// Accepts a validated path and the initial [`IdentityObservation`] captured
/// during hashing. Returns `true` only when every path component remains a
/// non-link, no-follow opening succeeds on a regular file, and both the opened
/// handle metadata and path `symlink_metadata` match `expected` (length,
/// modification time, and Unix device, inode, and status-change timestamp).
/// Returns `false` on any symlink, open, metadata, or identity mismatch.
pub(crate) fn verify_identity(path: &Path, expected: &IdentityObservation) -> bool {
    let Ok(validated) = resolve_no_symlink_path(path) else {
        return false;
    };
    let Ok(file) = open_no_follow(&validated) else {
        return false;
    };
    let Ok(handle_metadata) = file.metadata() else {
        return false;
    };
    if !handle_metadata.is_file() || identity_observation(&handle_metadata) != *expected {
        return false;
    }
    std::fs::symlink_metadata(&validated)
        .ok()
        .filter(|metadata| metadata.is_file())
        .is_some_and(|metadata| identity_observation(&metadata) == *expected)
}

/// Computes the ordered head, middle, and tail window offsets for sampled hashing.
///
/// Accepts the observed byte length of a regular file that is at least
/// `3 * SAMPLED_WINDOW_BYTES`. Returns `[0, (bytes - 1 MiB) / 2, bytes - 1 MiB]`
/// so the three 1 MiB windows never overlap. Performs no I/O and cannot fail
/// when the minimum length precondition holds.
fn sampled_window_offsets(bytes: u64) -> [u64; 3] {
    let tail = bytes - SAMPLED_WINDOW_BYTES;
    let middle = tail / 2;
    [0, middle, tail]
}

/// Streams a full-file SHA-256 digest through a bounded 64 KiB read buffer.
///
/// Accepts an opened regular file handle positioned at byte zero. Returns the
/// updated SHA-256 hasher, total bytes read, and any I/O error encountered.
/// Read errors stop streaming immediately and return the partial byte count.
fn stream_full_sha256(file: &mut std::fs::File) -> (Sha256, u64, Option<std::io::Error>) {
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
    (hasher, total, read_error)
}

/// Streams a domain-separated `glassbox-sampled-sha256/v1` fingerprint through a
/// bounded 64 KiB buffer.
///
/// Accepts an opened regular file handle and its observed byte length (which must
/// be at least `3 * SAMPLED_WINDOW_BYTES`). Feeds the ASCII scheme tag, a single
/// `0x00` separator, the big-endian byte length, and the three 1 MiB head/middle/tail
/// windows prefixed by big-endian offset and length. Returns the updated hasher,
/// total file bytes read, and any seek or read error encountered.
fn stream_sampled_sha256(
    file: &mut std::fs::File,
    bytes: u64,
) -> (Sha256, u64, Option<std::io::Error>) {
    let mut hasher = Sha256::new();
    hasher.update(SAMPLED_FINGERPRINT_SCHEME.as_bytes());
    hasher.update([0x00_u8]);
    hasher.update(bytes.to_be_bytes());
    let mut buffer = [0_u8; HASH_BUFFER_SIZE];
    let mut total = 0_u64;
    for offset in sampled_window_offsets(bytes) {
        hasher.update(offset.to_be_bytes());
        hasher.update(SAMPLED_WINDOW_BYTES.to_be_bytes());
        if let Err(error) = file.seek(SeekFrom::Start(offset)) {
            return (hasher, total, Some(error));
        }
        let mut remaining = SAMPLED_WINDOW_BYTES;
        while remaining > 0 {
            let chunk = (remaining as usize).min(HASH_BUFFER_SIZE);
            match file.read_exact(&mut buffer[..chunk]) {
                Ok(()) => {
                    hasher.update(&buffer[..chunk]);
                    total = total.saturating_add(chunk as u64);
                    remaining -= chunk as u64;
                }
                Err(error) => {
                    return (hasher, total, Some(error));
                }
            }
        }
    }
    (hasher, total, None)
}

/// Hashes a regular file under [`HashPolicy::SampledAboveThreshold`] and rechecks
/// handle and path identity.
///
/// Accepts a regular-file path. Returns source identity, either a full SHA-256
/// digest (at or below [`DEFAULT_FULL_HASH_MAX_BYTES`]) or a sampled
/// `glassbox-sampled-sha256/v1` fingerprint (above threshold), diagnostics, and
/// whether metadata reading can proceed. Open/read/metadata failures and detected
/// identity, length, modification, or path replacement withhold both digest and
/// fingerprint; these checks do not create an immutable snapshot against
/// adversarial concurrent writes.
pub fn hash_file(path: &Path) -> HashOutcome {
    hash_file_with_policy(path, HashPolicy::SampledAboveThreshold)
}

/// Hashes a regular file under an explicit [`HashPolicy`] and rechecks handle
/// and path identity.
///
/// Accepts a regular-file path and [`HashPolicy`]. Returns source identity,
/// digest or sampled fingerprint facts, diagnostics, and whether metadata
/// reading can proceed. Under [`HashPolicy::Full`], every file streams a full
/// SHA-256 digest regardless of byte length; open/read/metadata failures and
/// concurrent source changes withhold both digest and fingerprint.
pub fn hash_file_with_policy(path: &Path, policy: HashPolicy) -> HashOutcome {
    hash_file_with_policy_and_threshold(path, policy, DEFAULT_FULL_HASH_MAX_BYTES)
}

/// Hashes a regular file using an explicit [`HashPolicy`] and byte threshold.
///
/// Accepts a regular-file path, [`HashPolicy`], and maximum byte length eligible
/// for full SHA-256 under [`HashPolicy::SampledAboveThreshold`]. Files smaller
/// than `3 * SAMPLED_WINDOW_BYTES` always fall back to full SHA-256 so sampled
/// windows never overlap. Returns the ordinary [`HashOutcome`] with the same
/// failure and mutation withholding behavior as [`hash_file_with_policy`].
pub(crate) fn hash_file_with_policy_and_threshold(
    path: &Path,
    policy: HashPolicy,
    full_hash_max_bytes: u64,
) -> HashOutcome {
    hash_file_with_recheck(path, policy, full_hash_max_bytes, || {})
}

/// Runs hashing with one deterministic pre-recheck callback for mutation tests.
///
/// Accepts a source path, [`HashPolicy`], full-hash byte threshold, and a
/// closure invoked after reading and before the final identity observations.
/// Returns the ordinary hash outcome. All production I/O failures and mutation
/// outcomes match [`hash_file_with_policy`]; the callback cannot fail.
fn hash_file_with_recheck(
    path: &Path,
    policy: HashPolicy,
    full_hash_max_bytes: u64,
    before_recheck: impl FnOnce(),
) -> HashOutcome {
    hash_file_with_hooks(path, policy, full_hash_max_bytes, || {}, before_recheck)
}

/// Runs hashing with deterministic pre-read and pre-recheck callbacks for mutation tests.
///
/// Accepts a source path, [`HashPolicy`], full-hash byte threshold, a pre-read
/// closure invoked after capturing initial handle/path metadata, and a
/// pre-recheck closure invoked after reading and before final identity checks.
/// Returns the ordinary hash outcome. In production callers both closures are
/// no-ops; full-hash length disagreement with `before.length` is treated as
/// source change and withholds any digest.
fn hash_file_with_hooks(
    path: &Path,
    policy: HashPolicy,
    full_hash_max_bytes: u64,
    before_read: impl FnOnce(),
    before_recheck: impl FnOnce(),
) -> HashOutcome {
    let mut identity = SourceIdentity {
        path: absolute_lexical(path).unwrap_or_else(|_| path.to_path_buf()),
        ..SourceIdentity::default()
    };
    let mut digest = DigestReport::default();
    let mut diagnostics = Vec::new();
    let absolute = match resolve_no_symlink_path(path) {
        Ok(path) => path,
        Err(PathReject::Path(error)) => {
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
                observation: None,
            };
        }
        Err(PathReject::Symlink(error)) => {
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
                observation: None,
            };
        }
        Err(PathReject::Open(error)) => {
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
                observation: None,
            };
        }
    };
    identity.path = absolute.clone();
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
                observation: None,
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
                observation: None,
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
            observation: None,
        };
    }
    let before_identity = identity_observation(&before_metadata);
    let before = &before_identity.base;
    identity.byte_size = Some(before.length);
    identity.opaque_file_id = opaque_id(before);
    let initial_path_matches = std::fs::symlink_metadata(&absolute)
        .ok()
        .filter(|metadata| metadata.is_file())
        .is_some_and(|metadata| identity_observation(&metadata) == before_identity);
    let sampled = policy == HashPolicy::SampledAboveThreshold
        && before.length > full_hash_max_bytes
        && before.length >= 3 * SAMPLED_WINDOW_BYTES;
    before_read();
    let (hasher, total, read_error) = if sampled {
        stream_sampled_sha256(&mut file, before.length)
    } else {
        stream_full_sha256(&mut file)
    };
    before_recheck();
    let after = file
        .metadata()
        .map(|metadata| identity_observation(&metadata))
        .ok();
    let recheck_failed = after.is_none();
    let path_matches = std::fs::symlink_metadata(&absolute)
        .ok()
        .filter(|metadata| metadata.is_file())
        .is_some_and(|metadata| identity_observation(&metadata) == before_identity);
    let length_mismatch = !sampled && read_error.is_none() && total != before.length;
    let changed = !initial_path_matches
        || after.as_ref() != Some(&before_identity)
        || !path_matches
        || length_mismatch;
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
        completeness: if read_error.is_some() || sampled {
            Completeness::Partial
        } else {
            Completeness::Exact
        },
    };
    let truncated_by_concurrent_change = sampled
        && changed
        && read_error
            .as_ref()
            .is_some_and(|error| error.kind() == std::io::ErrorKind::UnexpectedEof);
    if let Some(error) = read_error.filter(|_| !truncated_by_concurrent_change) {
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
    } else if sampled {
        digest.fingerprint = Some(SampledFingerprint {
            scheme: SAMPLED_FINGERPRINT_SCHEME.to_owned(),
            value: format!("{:x}", hasher.finalize()),
        });
        digest.consistency = ConsistencyStatus::Consistent;
        diagnostics.push(diagnostic(
            DiagnosticStage::Hashing,
            DiagnosticSeverity::Info,
            "fingerprint_sampled",
            "change detection used size and modification time only.",
        ));
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
        observation: Some(before_identity),
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

    /// Creates a sparse test file of the requested byte length with distinct
    /// non-zero bytes at its head, middle, and tail windows.
    ///
    /// Accepts target path and total byte length (at least `3 * SAMPLED_WINDOW_BYTES`).
    /// Returns unit after flushing the file. Filesystem errors panic.
    fn create_sampled_fixture(path: &Path, length: u64) {
        use std::io::Write;
        let mut file = std::fs::File::create(path).unwrap();
        file.set_len(length).unwrap();
        for (index, offset) in sampled_window_offsets(length).into_iter().enumerate() {
            file.seek(SeekFrom::Start(offset)).unwrap();
            let marker = [0x11_u8.wrapping_add(index as u8); 4096];
            file.write_all(&marker).unwrap();
        }
        file.sync_data().unwrap();
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
        let outcome = hash_file_with_recheck(
            &path,
            HashPolicy::SampledAboveThreshold,
            DEFAULT_FULL_HASH_MAX_BYTES,
            || {
                std::fs::write(&path, b"replacement").unwrap();
            },
        );
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "source_changed_during_hash")
        );
        std::fs::remove_dir_all(&directory).ok();
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
        let outcome = hash_file_with_recheck(
            &path,
            HashPolicy::SampledAboveThreshold,
            DEFAULT_FULL_HASH_MAX_BYTES,
            || {
                std::fs::rename(&replacement, &path).unwrap();
            },
        );
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves concurrent rewrite in sampled mode withholds both digest and fingerprint.
    ///
    /// Accepts no parameters. Returns unit after mutating an above-threshold fixture
    /// at the pre-recheck boundary. Test filesystem failures panic.
    #[test]
    fn concurrent_rewrite_withholds_sampled_fingerprint() {
        let directory = workdir("sampled-rewrite");
        let path = directory.join("large.bin");
        let threshold = 4 * SAMPLED_WINDOW_BYTES;
        create_sampled_fixture(&path, threshold + 1);
        let outcome =
            hash_file_with_recheck(&path, HashPolicy::SampledAboveThreshold, threshold, || {
                std::fs::write(&path, b"mutated").unwrap();
            });
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "source_changed_during_hash")
        );
        assert!(
            !outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "fingerprint_sampled")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves concurrent path swap in sampled mode withholds both digest and fingerprint.
    ///
    /// Accepts no parameters. Returns unit after replacing the path at the pre-recheck
    /// boundary. Test filesystem failures panic.
    #[test]
    fn concurrent_path_swap_withholds_sampled_fingerprint() {
        let directory = workdir("sampled-swap");
        let path = directory.join("large.bin");
        let replacement = directory.join("replacement.bin");
        let threshold = 4 * SAMPLED_WINDOW_BYTES;
        create_sampled_fixture(&path, threshold + 1);
        std::fs::write(&replacement, b"other").unwrap();
        let outcome =
            hash_file_with_recheck(&path, HashPolicy::SampledAboveThreshold, threshold, || {
                std::fs::rename(&replacement, &path).unwrap();
            });
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "source_changed_during_hash")
        );
        assert!(
            !outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "fingerprint_sampled")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves threshold boundary switches cleanly between full SHA-256 and sampled fingerprint.
    ///
    /// Accepts no parameters. Returns unit after checking `T`, `T + 1`, and sub-3-MiB fallback.
    /// Unexpected digest/fingerprint presence or diagnostics panic.
    #[test]
    fn threshold_boundary_and_small_threshold_fallback() {
        let directory = workdir("threshold-unit");
        let threshold = 4 * SAMPLED_WINDOW_BYTES;
        let at_limit = directory.join("at-limit.bin");
        let over_limit = directory.join("over-limit.bin");
        let tiny = directory.join("tiny.bin");
        create_sampled_fixture(&at_limit, threshold);
        create_sampled_fixture(&over_limit, threshold + 1);
        std::fs::write(&tiny, b"small-file").unwrap();

        let exact = hash_file_with_policy_and_threshold(
            &at_limit,
            HashPolicy::SampledAboveThreshold,
            threshold,
        );
        assert!(exact.digest.digest.is_some());
        assert!(exact.digest.fingerprint.is_none());
        assert_eq!(exact.digest.bytes_hashed.value, Some(threshold));
        assert_eq!(exact.digest.bytes_hashed.completeness, Completeness::Exact);
        assert!(
            !exact
                .diagnostics
                .iter()
                .any(|d| d.code == "fingerprint_sampled")
        );

        let sampled = hash_file_with_policy_and_threshold(
            &over_limit,
            HashPolicy::SampledAboveThreshold,
            threshold,
        );
        assert!(sampled.digest.digest.is_none());
        let fp = sampled
            .digest
            .fingerprint
            .as_ref()
            .expect("sampled fingerprint expected above threshold");
        assert_eq!(fp.scheme, SAMPLED_FINGERPRINT_SCHEME);
        assert_eq!(fp.value.len(), 64);
        assert_eq!(
            sampled.digest.bytes_hashed.value,
            Some(3 * SAMPLED_WINDOW_BYTES)
        );
        assert_eq!(
            sampled.digest.bytes_hashed.completeness,
            Completeness::Partial
        );
        assert!(sampled.diagnostics.iter().any(|d| {
            d.code == "fingerprint_sampled"
                && d.severity == DiagnosticSeverity::Info
                && d.message == "change detection used size and modification time only."
        }));

        // Files smaller than 3 * SAMPLED_WINDOW_BYTES always fall back to full SHA-256
        // even if a test passes a threshold below 3 MiB.
        let fallback =
            hash_file_with_policy_and_threshold(&tiny, HashPolicy::SampledAboveThreshold, 1);
        assert!(fallback.digest.digest.is_some());
        assert!(fallback.digest.fingerprint.is_none());
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves a FIFO input cannot block the open path before rejection.
    ///
    /// Accepts no parameters. Returns unit after creating a named pipe with no
    /// writer and hashing it. A blocked open fails through the timeout channel
    /// instead of hanging; without `mkfifo` the invariant is left untested and
    /// the test returns. Test filesystem failures panic.
    #[cfg(any(
        all(
            target_os = "linux",
            any(
                target_arch = "arm",
                target_arch = "aarch64",
                target_arch = "x86",
                target_arch = "x86_64"
            )
        ),
        target_os = "macos",
        target_os = "ios",
        target_os = "freebsd",
        target_os = "dragonfly",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
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
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "input_not_regular_file")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves full-hash byte-count disagreement with `st_size` withholds any digest
    /// even when before/after handle and path metadata match identically.
    ///
    /// Accepts no parameters. Returns unit after simulating a same-tick transient
    /// resize whose length and mtime are restored before recheck. Test filesystem
    /// failures panic.
    #[test]
    fn full_hash_length_mismatch_withholds_digest_even_when_metadata_matches() {
        let directory = workdir("length-mismatch");
        let path = directory.join("transient.bin");
        let fixed_mtime = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::write(&path, vec![b'z'; 64]).unwrap();
        {
            let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
            file.set_modified(fixed_mtime).unwrap();
        }
        let initial_obs = observation(&std::fs::symlink_metadata(&path).unwrap());
        assert_eq!(initial_obs.length, 64);

        let before_read_path = path.clone();
        let before_recheck_path = path.clone();
        let outcome = hash_file_with_hooks(
            &path,
            HashPolicy::Full,
            DEFAULT_FULL_HASH_MAX_BYTES,
            move || {
                // Shrink in place before reading so full-hash reads 16 bytes instead of 64.
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&before_read_path)
                    .unwrap();
                file.set_len(16).unwrap();
            },
            move || {
                // Restore original length and mtime so after == before and path_matches == true.
                let file = std::fs::OpenOptions::new()
                    .write(true)
                    .open(&before_recheck_path)
                    .unwrap();
                file.set_len(64).unwrap();
                file.set_modified(fixed_mtime).unwrap();
                assert_eq!(
                    observation(&std::fs::symlink_metadata(&before_recheck_path).unwrap()),
                    initial_obs
                );
            },
        );
        assert!(outcome.input_valid);
        assert_eq!(outcome.digest.bytes_hashed.value, Some(16));
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "source_changed_during_hash")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves Linux virtual regular files reporting `st_size == 0` while streaming
    /// non-empty bytes are never reported as `Consistent`.
    ///
    /// Accepts no parameters. Returns unit after checking `/proc/version` when
    /// available with zero reported metadata length. Unexpected consistency or
    /// retained digests panic.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_zero_st_size_virtual_file_withholds_digest() {
        let path = Path::new("/proc/version");
        let Ok(metadata) = std::fs::symlink_metadata(path) else {
            return;
        };
        if !metadata.is_file() || metadata.len() != 0 {
            return;
        }
        let outcome = hash_file(path);
        assert!(outcome.input_valid);
        assert!(outcome.digest.bytes_hashed.value.unwrap_or(0) > 0);
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "source_changed_during_hash")
        );
    }

    /// Proves `validate_regular_file` and `hash_file` reject a symlinked
    /// directory component before any `..` lexical folding (GB-AUD-15).
    ///
    /// Accepts no parameters. Returns unit after creating `symlink_dir/../real.txt`
    /// and asserting rejection by both helpers. Unexpected acceptance panics.
    #[cfg(unix)]
    #[test]
    fn symlink_before_parent_dir_is_rejected_before_folding() {
        let directory = workdir("symlink-dotdot");
        let real_dir = directory.join("real_dir");
        let symlink_dir = directory.join("symlink_dir");
        let real_file = directory.join("real.txt");
        std::fs::create_dir(&real_dir).unwrap();
        std::fs::write(&real_file, b"hello").unwrap();
        std::os::unix::fs::symlink(&real_dir, &symlink_dir).unwrap();

        let traversal = symlink_dir.join("..").join("real.txt");
        assert!(
            validate_regular_file(&traversal).is_err(),
            "validate_regular_file must reject symlink_dir/../real.txt"
        );
        let outcome = hash_file(&traversal);
        assert!(!outcome.input_valid);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.observation.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "input_symlink_rejected")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves `verify_identity` detects both atomic rename replacement and
    /// in-place mutation after initial observation (GB-AUD-15).
    ///
    /// Accepts no parameters. Returns unit after checking match before mutation,
    /// mismatch after in-place write, and mismatch after atomic rename.
    #[test]
    fn verify_identity_detects_replacement_and_modification() {
        let directory = workdir("verify-identity");
        let path = directory.join("target.txt");
        let replacement = directory.join("replacement.txt");
        std::fs::write(&path, b"version-1").unwrap();
        std::fs::write(&replacement, b"version-2").unwrap();

        let initial = hash_file(&path);
        let obs = initial.observation.expect("initial observation expected");
        assert!(verify_identity(&initial.identity.path, &obs));

        // In-place rewrite must fail identity verification.
        std::fs::write(&path, b"version-1-mutated").unwrap();
        assert!(!verify_identity(&initial.identity.path, &obs));

        // Re-hash after rewrite, then atomically replace via rename.
        let second = hash_file(&path);
        let second_obs = second.observation.expect("second observation expected");
        assert!(verify_identity(&second.identity.path, &second_obs));
        std::fs::rename(&replacement, &path).unwrap();
        assert!(!verify_identity(&second.identity.path, &second_obs));

        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves `resolve_no_symlink_path`, `validate_regular_file`, and
    /// `hash_file` reject parent traversal (`..`) after a non-directory file.
    ///
    /// Accepts no parameters. Returns unit after creating `regular.txt` and
    /// asserting that `regular.txt/../regular.txt` fails with `InvalidInput`.
    #[test]
    fn parent_dir_after_non_directory_is_rejected() {
        let directory = workdir("nondir-dotdot");
        let regular = directory.join("regular.txt");
        std::fs::write(&regular, b"hello").unwrap();

        let traversal = regular.join("..").join("regular.txt");
        let error = validate_regular_file(&traversal)
            .expect_err("validate_regular_file must reject file/../file");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let outcome = hash_file(&traversal);
        assert!(!outcome.input_valid);
        assert!(outcome.digest.digest.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "input_open_failed")
        );
        std::fs::remove_dir_all(&directory).ok();
    }

    /// Proves a Unix `ctime` metadata mutation during hashing (with unchanged
    /// byte length and `mtime`) triggers `ConsistencyStatus::SourceChanged` and
    /// emits `source_changed_during_hash`.
    ///
    /// Accepts no parameters. Returns unit after changing file permissions from
    /// `0o644` to `0o640` in `before_recheck`, verifying `mtime` and length stay
    /// identical while `ctime` changes, restoring permissions, and removing the
    /// fixture directory. Unexpected consistency or retained digests panic.
    #[cfg(unix)]
    #[test]
    fn unix_ctime_change_during_hash_withholds_digest() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let directory = workdir("ctime-change");
        let path = directory.join("metadata-only.txt");
        std::fs::write(&path, b"stable-content-bytes").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        let before_meta = std::fs::symlink_metadata(&path).unwrap();
        let before_obs = observation(&before_meta);
        let before_ctime = (before_meta.ctime(), before_meta.ctime_nsec());

        let hook_path = path.clone();
        let outcome = hash_file_with_recheck(
            &path,
            HashPolicy::Full,
            DEFAULT_FULL_HASH_MAX_BYTES,
            move || {
                // Wait past coarse filesystem clock granularity so chmod updates ctime.
                let mut mode = 0o640;
                for _ in 0..50 {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    std::fs::set_permissions(&hook_path, std::fs::Permissions::from_mode(mode))
                        .unwrap();
                    let meta = std::fs::symlink_metadata(&hook_path).unwrap();
                    if (meta.ctime(), meta.ctime_nsec()) != before_ctime {
                        break;
                    }
                    mode = if mode == 0o640 { 0o644 } else { 0o640 };
                }
                let after_meta = std::fs::symlink_metadata(&hook_path).unwrap();
                assert_eq!(
                    observation(&after_meta),
                    before_obs,
                    "chmod must leave length, mtime, dev, and ino unchanged"
                );
                assert_ne!(
                    (after_meta.ctime(), after_meta.ctime_nsec()),
                    before_ctime,
                    "chmod must advance Unix ctime"
                );
            },
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).ok();
        std::fs::remove_dir_all(&directory).ok();

        assert!(outcome.input_valid);
        assert_eq!(
            outcome.identity.consistency,
            ConsistencyStatus::SourceChanged
        );
        assert_eq!(outcome.digest.consistency, ConsistencyStatus::SourceChanged);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "source_changed_during_hash")
        );
    }

    /// Proves non-symlink `symlink_metadata` errors (such as `ENOTDIR` from
    /// traversing through a regular file) classify as `PathReject::Open` and
    /// emit `input_open_failed` instead of `input_symlink_rejected`.
    ///
    /// Accepts no parameters. Returns unit after checking `file.txt/child`
    /// through both `reject_symlink_components` and `hash_file`. Misclassified
    /// rejection variants or diagnostics panic.
    #[test]
    fn non_symlink_metadata_error_classifies_as_open_failure() {
        let directory = workdir("enotdir-child");
        let regular = directory.join("file.txt");
        std::fs::write(&regular, b"hello").unwrap();
        let child = regular.join("child");

        assert!(
            matches!(reject_symlink_components(&child), Err(PathReject::Open(_))),
            "reject_symlink_components must map ENOTDIR to PathReject::Open"
        );
        let outcome = hash_file(&child);
        std::fs::remove_dir_all(&directory).ok();

        assert!(!outcome.input_valid);
        assert!(outcome.digest.digest.is_none());
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "input_open_failed"),
            "expected input_open_failed diagnostic, got: {:?}",
            outcome.diagnostics
        );
        assert!(
            !outcome
                .diagnostics
                .iter()
                .any(|d| d.code == "input_symlink_rejected"),
            "non-symlink metadata error must not emit input_symlink_rejected: {:?}",
            outcome.diagnostics
        );
    }
}
