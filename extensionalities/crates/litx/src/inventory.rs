// Module: glassbox-cli safe serial directory inventory.
// File: inventory.rs.
// Responsibility: Produce deterministic bounded JSONL for local files without links or mount crossing.

use glassbox_inspect::Inspector;
use glassbox_inspect::reports::{
    Completeness, ConsistencyStatus, Diagnostic, DiagnosticSeverity, DiagnosticStage,
    InspectOptions, MetadataReport, OperationStatus,
};
use serde_json::Value;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

#[cfg(target_os = "linux")]
const O_CLOEXEC: i32 = 0x80000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const O_DIRECTORY: i32 = 0x4000;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
const O_DIRECTORY: i32 = 0x10000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const O_NOFOLLOW: i32 = 0x8000;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
const O_NOFOLLOW: i32 = 0x20000;
#[cfg(target_os = "linux")]
const O_NONBLOCK: i32 = 0x800;
// O_PATH is 010000000 octal on every Linux port in scope (asm-generic value).
#[cfg(target_os = "linux")]
const O_PATH: i32 = 0x200000;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn open(path: *const std::os::raw::c_char, flags: i32, ...) -> i32;
    fn openat(dirfd: i32, path: *const std::os::raw::c_char, flags: i32, ...) -> i32;
}

/// One Linux mountinfo record used for ancestry and local-filesystem checks.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Mount {
    /// Kernel mount identifier; bind mounts differ even on one st_dev.
    id: u64,
    /// Parent mount identifier retained for parser diagnostics and tests.
    parent_id: u64,
    /// Escaped-path-decoded absolute mount point.
    point: PathBuf,
    /// Filesystem type used by the local-root allowlist.
    filesystem: String,
}

/// Parsed mount table with longest-mount-point lookup.
#[derive(Clone, Debug, Default)]
struct MountTable {
    /// Records in kernel observation order; later overmounts win ties.
    mounts: Vec<Mount>,
}

impl MountTable {
    /// Parses Linux mountinfo including octal-escaped paths and bind identities.
    ///
    /// Accepts mountinfo text. Returns records with mount IDs, parent IDs,
    /// decoded mount points, and filesystem types. Malformed rows, missing
    /// separators, invalid IDs, and invalid escapes return an error before any
    /// directory traversal begins.
    fn parse(input: &str) -> Result<Self, String> {
        let mut mounts = Vec::new();
        for (index, line) in input.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split_whitespace().collect();
            let separator = fields
                .iter()
                .position(|field| *field == "-")
                .ok_or_else(|| format!("mountinfo row {} lacks separator", index + 1))?;
            if separator < 6 || fields.len() < separator + 4 {
                return Err(format!("mountinfo row {} is incomplete", index + 1));
            }
            let id = fields[0]
                .parse()
                .map_err(|_| format!("mountinfo row {} has invalid mount ID", index + 1))?;
            let parent_id = fields[1]
                .parse()
                .map_err(|_| format!("mountinfo row {} has invalid parent ID", index + 1))?;
            mounts.push(Mount {
                id,
                parent_id,
                point: PathBuf::from(decode_mount_path(fields[4])?),
                filesystem: fields[separator + 1].to_owned(),
            });
        }
        if mounts.is_empty() {
            return Err("mountinfo contains no mounts".into());
        }
        Ok(Self { mounts })
    }

    /// Reads and parses the current process's Linux mount table.
    ///
    /// Accepts no parameters. Returns an owned mount snapshot. Procfs read and
    /// UTF-8 failures are returned; no mounted subtree or network path is read.
    #[cfg(target_os = "linux")]
    fn current() -> Result<Self, String> {
        let input = std::fs::read_to_string("/proc/self/mountinfo")
            .map_err(|error| format!("read /proc/self/mountinfo: {error}"))?;
        Self::parse(&input)
    }

    /// Resolves the deepest mount record containing a path.
    ///
    /// Accepts an absolute lexical path. Returns the longest matching mount
    /// point, with later overmount records winning equal paths. Missing roots
    /// return None; no filesystem access occurs.
    fn find(&self, path: &Path) -> Option<&Mount> {
        self.mounts
            .iter()
            .filter(|mount| path.starts_with(&mount.point))
            .max_by_key(|mount| mount.point.components().count())
    }
}

/// Decodes Linux mountinfo's octal escapes without UTF-8 path assumptions.
///
/// Accepts one escaped mountinfo path field. Returns an owned OS string.
/// Invalid trailing or non-octal escapes return errors; ordinary spaces and
/// backslashes encoded by the kernel round-trip exactly.
fn decode_mount_path(value: &str) -> Result<std::ffi::OsString, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] != b'\\' {
            decoded.push(bytes[index]);
            index += 1;
            continue;
        }
        if index + 3 >= bytes.len()
            || !bytes[index + 1..=index + 3]
                .iter()
                .all(|byte| (b'0'..=b'7').contains(byte))
        {
            return Err("invalid mountinfo path escape".into());
        }
        let number = (bytes[index + 1] - b'0') * 64
            + (bytes[index + 2] - b'0') * 8
            + (bytes[index + 3] - b'0');
        decoded.push(number);
        index += 4;
    }
    #[cfg(unix)]
    {
        Ok(std::ffi::OsString::from_vec(decoded))
    }
    #[cfg(not(unix))]
    {
        Ok(String::from_utf8_lossy(&decoded).into_owned().into())
    }
}

/// Reports whether an inventory root is on an explicitly known local filesystem.
///
/// Accepts a filesystem type. Returns true only for local block, memory, or
/// overlay filesystems. Network, FUSE, portal, and unknown types fail closed;
/// no endpoint or mount is probed.
fn known_local_filesystem(filesystem: &str) -> bool {
    matches!(
        filesystem,
        "ext2"
            | "ext3"
            | "ext4"
            | "xfs"
            | "btrfs"
            | "tmpfs"
            | "overlay"
            | "zfs"
            | "vfat"
            | "exfat"
            | "ntfs"
            | "ntfs3"
            | "ufs"
    )
}

/// Produces an absolute lexical path without resolving symbolic links.
///
/// Accepts a source or output path. Returns an absolute normalized path.
/// Current-directory and root-escape failures are returned before traversal.
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

/// Returns whether a proposed artifact directory is inside an inventory root.
///
/// Accepts root and output paths. Returns lexical containment after absolute
/// normalization, or a conservative true when normalization fails. No path is
/// opened; callers reject the conflicting invocation before reading inputs.
pub(crate) fn output_inside_root(root: &Path, output: &Path) -> bool {
    match (absolute_lexical(root), absolute_lexical(output)) {
        (Ok(root), Ok(output)) => output.starts_with(root),
        _ => true,
    }
}

/// Rejects existing symbolic-link path components before opening a root.
///
/// Accepts an absolute lexical path. Returns unit when existing components are
/// non-links. Links and metadata errors are returned; missing suffixes are
/// reported by the following no-follow directory open.
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

/// Opens a directory without following its final link.
///
/// Accepts an absolute directory path. Returns an anchored directory handle.
/// Missing paths, non-directories, links, and permission failures are returned
/// before enumeration.
#[cfg(target_os = "linux")]
fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: path is a valid NUL-terminated string and open copies it.
    let descriptor = unsafe { open(path.as_ptr(), O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC) };
    if descriptor < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: ownership of the new descriptor transfers to File exactly once.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }
}

/// Opens one directory entry relative to its anchored parent without links.
///
/// Accepts a parent directory handle, entry name, and whether a directory is
/// required. Returns an owned nonblocking handle and metadata. Replacement
/// entries of the wrong type, final symlinks, and open/metadata errors are
/// returned; regular-file hashing retains no borrowed path pointer.
#[cfg(target_os = "linux")]
fn open_entry(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename"))?;
    let flags = O_NOFOLLOW | O_CLOEXEC | O_NONBLOCK | if directory { O_DIRECTORY } else { 0 };
    // SAFETY: parent fd and name remain valid for the duration of openat.
    let descriptor = unsafe { openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: ownership of the new descriptor transfers to File exactly once.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    let metadata = file.metadata()?;
    if directory && !metadata.is_dir() || !directory && !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "directory entry changed type before open",
        ));
    }
    Ok((file, metadata))
}

/// Enumerates one directory's entry names through its anchored handle.
///
/// Accepts a parent directory handle already opened without following links.
/// Returns every entry name except "." and ".." in kernel order, obtained by
/// fdopendir on a duplicated descriptor so the traversal never re-resolves the
/// directory path. The duplicated descriptor stays RAII-owned until fdopendir
/// succeeds and is released exactly once on every path, so a failed call leaks
/// no duplicate and the caller's handle stays open for per-entry opens. Each
/// record's name is read through a raw field pointer so short kernel records
/// never read out of bounds; enumeration errors are returned after the
/// descriptor is closed.
#[cfg(target_os = "linux")]
fn anchored_names(handle: &std::fs::File) -> std::io::Result<Vec<std::ffi::OsString>> {
    use std::os::fd::AsRawFd;
    let owned = handle.try_clone()?;
    // SAFETY: fdopendir consumes the duplicated descriptor only on success;
    // until then `owned` keeps ownership and closes it if this call fails.
    let stream = unsafe { libc::fdopendir(owned.as_raw_fd()) };
    if stream.is_null() {
        return Err(std::io::Error::last_os_error());
    }
    // fdopendir adopted the descriptor, so the RAII handle must not close it
    // a second time; the stream closes it exactly once in closedir.
    std::mem::forget(owned);
    let mut names = Vec::new();
    loop {
        // SAFETY: errno is cleared through the libc thread-local slot exactly
        // like the standard library, so a null result can be distinguished.
        unsafe { *libc::__errno_location() = 0 };
        // SAFETY: stream comes from fdopendir; the returned record stays valid
        // until the next readdir call and remains owned by the stream.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            // SAFETY: errno is read only after the null result that may set it.
            if unsafe { *libc::__errno_location() } != 0 {
                let error = std::io::Error::last_os_error();
                // SAFETY: stream ownership ends exactly once on every path.
                let _ = unsafe { libc::closedir(stream) };
                return Err(error);
            }
            break;
        }
        // SAFETY: addr_of! computes the field address without creating a
        // whole-record reference, so no dereferenceability is asserted over
        // records shorter than the full libc dirent layout; the kernel
        // NUL-terminates d_name within the record it returned.
        let name = unsafe { std::ptr::addr_of!((*entry).d_name) };
        // SAFETY: d_name is NUL-terminated by the kernel readdir contract.
        let bytes = unsafe { std::ffi::CStr::from_ptr(name.cast()) }.to_bytes();
        let name = std::ffi::OsString::from_vec(bytes.to_vec());
        if name != "." && name != ".." {
            names.push(name);
        }
    }
    // SAFETY: stream came from fdopendir and is closed exactly once here.
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(names)
}

/// Stats one directory entry through the anchored parent without side effects.
///
/// Accepts a parent directory handle and entry name. Returns the entry's own
/// metadata, including symlink types. O_PATH with O_NOFOLLOW never opens the
/// entry itself, so devices, sockets, and pipes cannot block or run driver
/// code while the traversal classifies them; open and metadata errors are
/// returned without touching entry contents.
#[cfg(target_os = "linux")]
fn open_entry_metadata(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
) -> std::io::Result<std::fs::Metadata> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename"))?;
    // SAFETY: parent descriptor and NUL-terminated name stay valid for the call.
    let descriptor = unsafe {
        openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            O_PATH | O_NOFOLLOW | O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: ownership of the newly returned descriptor transfers to File once.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    file.metadata()
}

/// Backpressured inventory traversal state; file reports are produced by shared workers.
struct Inventory<'a> {
    /// Absolute no-follow root used for mount and output containment.
    root: PathBuf,
    /// Kernel mount identity that every visited entry must retain.
    root_mount: u64,
    /// Current-process mount snapshot, never a recursive mount scan.
    mounts: MountTable,
    /// Opt-in recursion flag; false lists only immediate regular files.
    recursive: bool,
    /// Deterministic admission sequence shared by success and diagnostics.
    sequence: usize,
    /// Bounded producer that blocks traversal when outstanding responses reach N.
    producer: &'a mut crate::batch::BatchProducer,
}

impl<'a> Inventory<'a> {
    /// Creates traversal state without reading file contents.
    ///
    /// Accepts root/mount context, recursion flag, and bounded batch producer.
    /// Returns an empty-sequence enumerator. No directory or native reader runs.
    fn new(
        root: PathBuf,
        root_mount: u64,
        mounts: MountTable,
        recursive: bool,
        producer: &'a mut crate::batch::BatchProducer,
    ) -> Self {
        Self {
            root,
            root_mount,
            mounts,
            recursive,
            sequence: 0,
            producer,
        }
    }

    /// Emits one traversal diagnostic in deterministic admission order.
    ///
    /// Accepts path, stable error code, message, and partial-vs-failed status.
    /// Returns true after bounded admission and false after cancellation. Queue
    /// failures are returned; non-Unicode paths are lossy only in diagnostics.
    fn error(
        &mut self,
        path: &Path,
        code: &str,
        message: impl ToString,
        status: OperationStatus,
    ) -> Result<bool, String> {
        self.sequence += 1;
        self.producer.complete(crate::batch::BatchCompletion::error(
            Value::from(self.sequence),
            path,
            status,
            "inventory",
            code,
            message,
        ))
    }

    /// Removes digest trust when enumeration and inspection observed different files.
    ///
    /// Accepts a mutable report and opaque identity captured from the anchored
    /// entry handle. Returns unit after source-changed diagnostics on mismatch.
    /// Stable identities remain best-effort, not an adversarial snapshot guarantee.
    fn check_identity(report: &mut MetadataReport, expected: Option<&str>) {
        if expected.is_none() || report.metadata.identity.opaque_file_id.as_deref() == expected {
            return;
        }
        report.metadata.identity.consistency = ConsistencyStatus::SourceChanged;
        report.metadata.digest.digest = None;
        report.metadata.digest.consistency = ConsistencyStatus::SourceChanged;
        report.metadata.digest.source_identity = ConsistencyStatus::SourceChanged;
        report.metadata.diagnostics.push(Diagnostic {
            stage: DiagnosticStage::Hashing,
            severity: DiagnosticSeverity::Warning,
            code: "inventory_source_replaced".into(),
            message: "anchored enumeration and inspection observed different file identities; digest withheld".into(),
            unit_index: None,
        });
        if report.metadata.status != OperationStatus::Failed {
            report.metadata.status = OperationStatus::Partial;
        }
        report.metadata.completeness = Completeness::Partial;
    }

    /// Admits one regular file for inspection under the shared file/OCR budget.
    ///
    /// Accepts a path and opaque identity from the still-open anchored handle.
    /// Returns true after bounded admission and false after cancellation. Invalid
    /// Unicode paths produce a failed diagnostic; no contents are read by traversal.
    fn file(&mut self, path: &Path, expected: Option<&str>) -> Result<bool, String> {
        if path.to_str().is_none() {
            return self.error(
                path,
                "non_utf8_path_unsupported",
                "report JSON cannot preserve this non-Unicode path; file was not inspected",
                OperationStatus::Failed,
            );
        }
        self.sequence += 1;
        self.producer.request(
            Value::from(self.sequence),
            path.to_path_buf(),
            expected.map(str::to_owned),
        )
    }

    /// Checks one path's mount identity against the root before any entry read.
    ///
    /// Accepts a candidate path. Returns Some(true) for root mount, Some(false)
    /// after a skip diagnostic, or None after cancellation. Unknown mounts fail
    /// closed with an operational diagnostic; no candidate subtree is entered.
    fn same_mount(&mut self, path: &Path) -> Result<Option<bool>, String> {
        match self.mounts.find(path) {
            Some(mount) if mount.id == self.root_mount => Ok(Some(true)),
            Some(mount) => {
                let detail = format!(
                    "mount ID {} ({}) differs from root mount {}; subtree skipped",
                    mount.id, mount.filesystem, self.root_mount
                );
                if !self.error(
                    path,
                    "cross_mount_skipped",
                    detail,
                    OperationStatus::Partial,
                )? {
                    return Ok(None);
                }
                Ok(Some(false))
            }
            None => {
                if !self.error(
                    path,
                    "mount_identity_unavailable",
                    "no mountinfo record contains this path; entry skipped",
                    OperationStatus::Failed,
                )? {
                    return Ok(None);
                }
                Ok(Some(false))
            }
        }
    }

    /// Visits one sorted directory level with bounded production and opt-in recursion.
    ///
    /// Accepts an absolute reporting path and anchored no-follow directory handle.
    /// Returns true after all entries or false when cancellation stops admission.
    /// Enumeration and per-entry metadata are read through the anchored handle,
    /// never through the path, so every entry resolves against the same inode
    /// the traversal opened. Read/open failures become ordered records and later
    /// entries continue; names alone are sorted and file bytes/reports are never
    /// accumulated by the traversal.
    #[cfg(target_os = "linux")]
    fn directory(&mut self, path: &Path, handle: &std::fs::File) -> Result<bool, String> {
        let mut names = match anchored_names(handle) {
            Ok(names) => names,
            Err(error) => {
                return self.error(
                    path,
                    "directory_read_failed",
                    error,
                    OperationStatus::Failed,
                );
            }
        };
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        for name in names {
            let child = path.join(&name);
            let metadata = match open_entry_metadata(handle, &name) {
                Ok(metadata) => metadata,
                // ELOOP is Linux's link rejection; the O_PATH metadata open
                // already yields symlink handles, so this only guards unusual
                // kernel ports and preserves the link skip.
                Err(error) if error.raw_os_error() == Some(40 /* ELOOP */) => {
                    if !self.error(
                        &child,
                        "symlink_skipped",
                        "symbolic links are never followed",
                        OperationStatus::Partial,
                    )? {
                        return Ok(false);
                    }
                    continue;
                }
                Err(error) => {
                    if !self.error(
                        &child,
                        "entry_metadata_failed",
                        error,
                        OperationStatus::Failed,
                    )? {
                        return Ok(false);
                    }
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                if !self.error(
                    &child,
                    "symlink_skipped",
                    "symbolic links are never followed",
                    OperationStatus::Partial,
                )? {
                    return Ok(false);
                }
                continue;
            }
            if metadata.is_dir() {
                if !self.recursive {
                    continue;
                }
                let Some(same_mount) = self.same_mount(&child)? else {
                    return Ok(false);
                };
                if !same_mount {
                    continue;
                }
                match open_entry(handle, &name, true) {
                    Ok((child_handle, _)) => {
                        if !self.directory(&child, &child_handle)? {
                            return Ok(false);
                        }
                    }
                    Err(error) => {
                        if !self.error(
                            &child,
                            "directory_open_failed",
                            error,
                            OperationStatus::Failed,
                        )? {
                            return Ok(false);
                        }
                    }
                }
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            let Some(same_mount) = self.same_mount(&child)? else {
                return Ok(false);
            };
            if !same_mount {
                continue;
            }
            match open_entry(handle, &name, false) {
                Ok((_file, opened)) => {
                    #[cfg(unix)]
                    let expected = {
                        use std::os::unix::fs::MetadataExt;
                        Some(format!("{:x}:{:x}", opened.dev(), opened.ino()))
                    };
                    #[cfg(not(unix))]
                    let expected = None;
                    if !self.file(&child, expected.as_deref())? {
                        return Ok(false);
                    }
                }
                Err(error) => {
                    if !self.error(&child, "file_open_failed", error, OperationStatus::Failed)? {
                        return Ok(false);
                    }
                }
            }
        }
        Ok(true)
    }
}

/// Produces safe inventory requests while retaining the current mount snapshot.
///
/// Accepts root, recursion flag, and bounded producer. Returns unit after full
/// traversal or cancellation. Unsafe roots and mountinfo errors become one failed
/// record; producer disconnect errors are returned without further filesystem work.
#[cfg(target_os = "linux")]
fn produce(
    root: &Path,
    recursive: bool,
    producer: &mut crate::batch::BatchProducer,
) -> Result<(), String> {
    let root = match absolute_lexical(root).and_then(|root| {
        reject_symlink_components(&root)?;
        Ok(root)
    }) {
        Ok(root) => root,
        Err(error) => {
            let mut inventory = Inventory::new(
                root.to_path_buf(),
                0,
                MountTable::default(),
                recursive,
                producer,
            );
            inventory.error(
                root,
                "inventory_root_rejected",
                error,
                OperationStatus::Failed,
            )?;
            return Ok(());
        }
    };
    let mounts = match MountTable::current() {
        Ok(mounts) => mounts,
        Err(error) => {
            let mut inventory = Inventory::new(root, 0, MountTable::default(), recursive, producer);
            inventory.error(
                Path::new(""),
                "mountinfo_unavailable",
                error,
                OperationStatus::Failed,
            )?;
            return Ok(());
        }
    };
    let root_record = mounts.find(&root).cloned();
    let mut inventory = Inventory::new(
        root,
        root_record.as_ref().map_or(0, |mount| mount.id),
        mounts,
        recursive,
        producer,
    );
    match root_record {
        Some(mount) if !known_local_filesystem(&mount.filesystem) => {
            let detail = format!(
                "inventory root filesystem '{}' is not an approved local filesystem",
                mount.filesystem
            );
            let root = inventory.root.clone();
            inventory.error(
                &root,
                "non_local_root_rejected",
                detail,
                OperationStatus::Failed,
            )?;
        }
        None => {
            let root = inventory.root.clone();
            inventory.error(
                &root,
                "mount_identity_unavailable",
                "inventory root is absent from mountinfo",
                OperationStatus::Failed,
            )?;
        }
        Some(_) => match open_directory(&inventory.root) {
            Ok(handle) => {
                let root = inventory.root.clone();
                inventory.directory(&root, &handle)?;
            }
            Err(error) => {
                let root = inventory.root.clone();
                inventory.error(
                    &root,
                    "directory_open_failed",
                    error,
                    OperationStatus::Failed,
                )?;
            }
        },
    }
    Ok(())
}

/// Runs deterministic ordered inventory with bounded shared file/OCR workers.
///
/// Accepts root, recursion flag, positive jobs, optional external artifact
/// directory, and JSONL writer. Returns aggregate status after all responses
/// flush and workers join. Output/input transport errors are returned directly.
#[cfg(target_os = "linux")]
pub(crate) fn run<W: Write>(
    root: &Path,
    recursive: bool,
    jobs: usize,
    output_dir: Option<&Path>,
    mut output: W,
) -> std::io::Result<OperationStatus> {
    crate::batch::run(
        jobs,
        move |producer| produce(root, recursive, producer),
        |budget, _cancellation| {
            let mut inspector = Inspector::with_shared_budget(budget);
            let options = InspectOptions::default();
            move |request| {
                let sequence = request.sequence as usize;
                let expected = request.expected.clone();
                let mut report = inspector.inspect(&request.path, &options);
                Inventory::check_identity(&mut report, expected.as_deref());
                if let Some(directory) = output_dir {
                    crate::output::persist_metadata(&mut report, directory, sequence);
                }
                let status = report.metadata.status;
                crate::batch::BatchCompletion::report(request, status, serde_json::to_value(report))
            }
        },
        &mut output,
    )
    .map(|outcome| outcome.status)
    .map_err(std::io::Error::other)
}

/// Reports unsupported directory inventory without touching the host filesystem.
///
/// Accepts root, recursion flag, jobs, artifact directory, and output writer.
/// Returns an I/O unsupported error; no paths, mounts, network endpoints, or
/// output files are accessed on non-Linux hosts.
#[cfg(not(target_os = "linux"))]
pub(crate) fn run<W: Write>(
    _root: &Path,
    _recursive: bool,
    _jobs: usize,
    _output_dir: Option<&Path>,
    _output: W,
) -> std::io::Result<OperationStatus> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mount-safe directory inventory currently requires Linux mountinfo",
    ))
}
#[cfg(test)]
mod tests {
    use super::*;

    /// Checks escaped mount paths and same-device bind mount identities.
    ///
    /// Accepts no parameters. Returns unit after parsing synthetic mountinfo.
    /// Parser errors or incorrect mount selection panic; no live mount is made.
    #[test]
    fn mountinfo_distinguishes_same_device_bind_and_escaped_paths() {
        let table = MountTable::parse(
            "35 2 259:2 / / rw - ext4 /dev/root rw\n\
             100 35 259:2 /tmp/source /tmp/space\\040bind rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        let root = table.find(Path::new("/tmp/source")).unwrap();
        let bind = table.find(Path::new("/tmp/space bind")).unwrap();
        assert_eq!(root.id, 35);
        assert_eq!(bind.id, 100);
        assert_eq!(bind.parent_id, 35);
        assert_ne!(root.id, bind.id);
    }

    /// Checks network and unknown root filesystems fail closed.
    ///
    /// Accepts no parameters. Returns unit after synthetic NFS/FUSE/local
    /// classifications. No network or NAS endpoint is accessed.
    #[test]
    fn network_and_unknown_filesystems_are_not_local_roots() {
        for filesystem in [
            "nfs",
            "nfs4",
            "cifs",
            "smb3",
            "fuse.sshfs",
            "fuse.portal",
            "unknown",
        ] {
            assert!(!known_local_filesystem(filesystem), "{filesystem}");
        }
        assert!(known_local_filesystem("ext4"));
        assert!(known_local_filesystem("overlay"));
    }

    /// Checks malformed mount records and escapes are rejected before traversal.
    ///
    /// Accepts no parameters. Returns unit after malformed parser inputs.
    /// Unexpected parser success panics; no filesystem operation occurs.
    #[test]
    fn malformed_mountinfo_fails_closed() {
        assert!(MountTable::parse("not mountinfo").is_err());
        assert!(MountTable::parse("35 2 259:2 / /tmp/bad\\04x rw - ext4 /dev/root rw\n").is_err());
    }

    /// Checks equal mount points select the later overmount record.
    ///
    /// Accepts no parameters. Returns unit after synthetic overmount lookup.
    /// Incorrect longest-path or overmount selection panics without host I/O.
    #[test]
    fn later_overmount_wins_equal_mount_points() {
        let table = MountTable::parse(
            "35 2 259:2 / / rw - ext4 /dev/root rw\n\
             36 35 0:1 /data /data rw - nfs server:/data rw\n\
             37 35 259:2 /data /data rw - ext4 /dev/root rw\n",
        )
        .unwrap();
        assert_eq!(table.find(Path::new("/data/file")).unwrap().id, 37);
        assert_eq!(table.find(Path::new("/data")).unwrap().id, 37);
    }

    /// Checks anchored enumeration reads every name through the open handle.
    ///
    /// Accepts no parameters. Returns unit after enumerating one scratch
    /// directory containing nested, spaced, and linked entries. A missing
    /// entry, a leaked "." or ".." record, or a path-based read panic fails
    /// the anchored-traversal contract; the scratch tree is removed after.
    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_enumeration_reports_all_names_without_dot_entries() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("anchored-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory created");
        std::fs::write(directory.join("one.txt"), b"alpha").expect("file created");
        std::fs::create_dir(directory.join("nested")).expect("subdirectory created");
        std::fs::write(directory.join("nested/deep.txt"), b"deep").expect("nested file created");
        std::fs::write(directory.join("two ;$(x).txt"), b"spaced").expect("spaced name created");
        std::os::unix::fs::symlink(directory.join("one.txt"), directory.join("link"))
            .expect("symlink created");
        let handle = open_directory(&directory).expect("scratch directory opened");
        let names = anchored_names(&handle).expect("anchored enumeration succeeds");
        drop(handle);
        let mut observed: Vec<String> = names
            .iter()
            .map(|name| name.to_string_lossy().into())
            .collect();
        observed.sort();
        assert_eq!(
            observed,
            vec![
                "link".to_string(),
                "nested".to_string(),
                "one.txt".to_string(),
                "two ;$(x).txt".to_string()
            ]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks enumeration returns short and non-UTF-8 names without overreads.
    ///
    /// Accepts no parameters. Returns unit after enumerating one scratch
    /// directory holding a one-byte name (the shortest kernel record), a
    /// 255-byte NAME_MAX name (the longest record), and a non-UTF-8 name.
    /// A missing name or a mis-decoded byte sequence fails the raw d_name
    /// access contract; the scratch tree is removed after the assertions.
    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_enumeration_reads_short_and_non_utf8_names() {
        use std::os::unix::ffi::OsStrExt;
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("anchored-bytes-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory created");
        std::fs::write(directory.join("a"), b"short").expect("short name created");
        let long_name = "x".repeat(255);
        std::fs::write(directory.join(&long_name), b"long").expect("long name created");
        let raw_name = std::ffi::OsStr::from_bytes(&[0xff, 0xfe, b'z']);
        std::fs::write(directory.join(raw_name), b"bytes").expect("non-UTF-8 name created");
        let handle = open_directory(&directory).expect("scratch directory opened");
        let names = anchored_names(&handle).expect("anchored enumeration succeeds");
        drop(handle);
        let mut observed = names;
        observed.sort();
        assert_eq!(
            observed,
            vec![
                std::ffi::OsString::from("a"),
                std::ffi::OsString::from(long_name),
                std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'z']),
            ]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Counts open file descriptors resolving to one absolute path.
    ///
    /// Accepts a canonical absolute path. Returns the number of currently open
    /// descriptors pointing at it, read from procfs; a missing procfs mount
    /// reports zero without failing the caller.
    #[cfg(target_os = "linux")]
    fn count_descriptors_for(path: &Path) -> usize {
        std::fs::read_dir("/proc/self/fd")
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry
                            .path()
                            .read_link()
                            .map(|target| target == path)
                            .unwrap_or(false)
                    })
                    .count()
            })
            .unwrap_or(0)
    }

    /// Checks a failed enumeration closes its duplicate and keeps the caller's
    /// handle, and that a successful enumeration releases the stream.
    ///
    /// Accepts no parameters. Returns unit after one fdopendir failure on a
    /// regular-file handle and one successful directory enumeration. A leaked
    /// duplicated descriptor, a closed caller handle, or a stream left open
    /// after success fails the descriptor-ownership contract; the scratch
    /// tree is removed after the assertions.
    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_enumeration_owns_duplicate_on_fdopendir_failure() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("anchored-ownership-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory created");
        let file = directory.join("file.txt");
        std::fs::write(&file, b"caller").expect("scratch file created");
        let directory =
            std::fs::canonicalize(&directory).expect("scratch directory path canonicalized");
        let file = std::fs::canonicalize(&file).expect("scratch file path canonicalized");
        let mut handle = std::fs::File::open(&file).expect("caller handle opened");
        let error = anchored_names(&handle).expect_err("fdopendir rejects a non-directory");
        assert_eq!(error.raw_os_error(), Some(libc::ENOTDIR));
        assert_eq!(
            count_descriptors_for(&file),
            1,
            "the failed call must close its duplicated descriptor"
        );
        let mut content = String::new();
        std::io::Read::read_to_string(&mut handle, &mut content)
            .expect("the caller's handle stays usable after the failure");
        assert_eq!(content, "caller");
        drop(handle);
        let directory_handle = open_directory(&directory).expect("scratch directory opened");
        let names = anchored_names(&directory_handle).expect("anchored enumeration succeeds");
        drop(directory_handle);
        assert_eq!(names.len(), 1, "only the scratch file is reported");
        assert_eq!(
            count_descriptors_for(&directory),
            0,
            "closedir must close the duplicated descriptor after success"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }
}
