//! Module: glassbox-cli safe serial directory inventory.
//! File: inventory.rs.
//! Responsibility: Produce deterministic bounded schema v3 JSONL for local files without links or mount crossing.

use glassbox_inspect::reports::OperationStatus;
use std::io::Write;
use std::path::{Component, Path, PathBuf};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use glassbox_inspect::Inspector;
#[cfg(any(target_os = "linux", target_os = "macos"))]
use glassbox_inspect::reports::{
    Completeness, ConsistencyStatus, Diagnostic, DiagnosticSeverity, DiagnosticStage,
    InspectOptions, MetadataReport,
};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use serde_json::Value;

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::ffi::OsStrExt;
#[cfg(unix)]
use std::os::unix::ffi::OsStringExt;

/// Maximum directory recursion depth below the inventory root.
///
/// Limits simultaneously open non-tail ancestor descriptors and recursion stack
/// frames so deep directory trees emit a deterministic diagnostic instead of
/// risking `EMFILE` or stack overflow.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const MAX_DIRECTORY_DEPTH: usize = 256;

/// Darwin `<sys/stat.h>` file flag marking a cloud-evicted dataless placeholder.
///
/// Absent from `libc-0.2.191`, so defined explicitly for `fstatat` checks on
/// macOS and cross-platform unit tests.
#[cfg(any(target_os = "macos", test))]
const SF_DATALESS: u32 = 0x4000_0000;

/// Darwin `<sys/mount.h>` flag bit marking a filesystem mounted locally.
#[cfg(target_os = "macos")]
const MACOS_MNT_LOCAL: u32 = libc::MNT_LOCAL as u32;
#[cfg(all(test, not(target_os = "macos")))]
const MACOS_MNT_LOCAL: u32 = 0x0000_1000;

/// One Linux mountinfo record used for ancestry and local-filesystem checks.
#[cfg(any(target_os = "linux", test))]
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
#[cfg(any(target_os = "linux", test))]
#[derive(Clone, Debug, Default)]
struct MountTable {
    /// Records in kernel observation order; later overmounts win ties.
    mounts: Vec<Mount>,
}

#[cfg(any(target_os = "linux", test))]
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
#[cfg(any(target_os = "linux", test))]
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
#[cfg(any(target_os = "linux", test))]
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

/// Reports whether a macOS `fstatfs` descriptor represents an approved local filesystem.
///
/// Accepts Darwin `statfs.f_flags` and the NUL-trimmed `f_fstypename`. Returns
/// true only when `MNT_LOCAL` is set and the filesystem type is in the explicit
/// local allowlist (`apfs`, `hfs`, `msdos`, `exfat`, `udf`, `tmpfs`). Network,
/// FUSE, pseudo-filesystem, and unknown types fail closed without probing any
/// external volume.
#[cfg(any(target_os = "macos", test))]
fn known_local_macos_filesystem(flags: u32, filesystem: &str) -> bool {
    (flags & MACOS_MNT_LOCAL) != 0
        && matches!(
            filesystem,
            "apfs" | "hfs" | "msdos" | "exfat" | "udf" | "tmpfs"
        )
}

/// Reports whether a Darwin `stat.st_flags` word marks a cloud-evicted placeholder.
///
/// Accepts Darwin `st_flags`. Returns true when `SF_DATALESS` (`0x4000_0000`)
/// is set so inventory skips evicted iCloud/FileProvider placeholders before
/// calling `openat`, avoiding silent network hydration. No I/O occurs.
#[cfg(any(target_os = "macos", test))]
fn is_dataless_placeholder(st_flags: u32) -> bool {
    (st_flags & SF_DATALESS) != 0
}

/// Queries Darwin filesystem metadata for an already-opened descriptor.
///
/// Accepts an open directory or file handle. Returns the kernel `statfs`
/// record from `fstatfs` without re-resolving any filesystem path. Kernel
/// errors are returned directly.
#[cfg(target_os = "macos")]
fn fstatfs_handle(handle: &std::fs::File) -> std::io::Result<libc::statfs> {
    use std::os::fd::AsRawFd;
    let mut fs = std::mem::MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `handle` is an open descriptor and `fs` is valid writable
    // storage for `libc::statfs`; `fstatfs` initializes it on return 0.
    if unsafe { libc::fstatfs(handle.as_raw_fd(), fs.as_mut_ptr()) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fstatfs` succeeded (returned 0) and populated `fs`.
    Ok(unsafe { fs.assume_init() })
}

/// Extracts the two-word Darwin filesystem identifier from `libc::fsid_t`.
///
/// Accepts a Darwin `libc::fsid_t` value returned by `fstatfs`. Returns its
/// `[i32; 2]` value words for mount-boundary comparison. Never fails; relies on
/// the compile-time size assertion matching Darwin's `struct fsid { int32_t val[2]; }`.
#[cfg(target_os = "macos")]
fn fsid_value(fsid: libc::fsid_t) -> [i32; 2] {
    let mut value = [0i32; 2];
    const _: () = assert!(std::mem::size_of::<libc::fsid_t>() == std::mem::size_of::<[i32; 2]>());
    // SAFETY: `libc::fsid_t` is `#[repr(C)]` containing `[i32; 2]` (plus a
    // zero-sized `__non_exhaustive: ()` marker) with identical size and no
    // uninitialized padding; copying raw bytes extracts the two Darwin words
    // without relying on `libc`'s private field or `extra_traits`.
    unsafe {
        std::ptr::copy_nonoverlapping(
            (&raw const fsid).cast::<u8>(),
            value.as_mut_ptr().cast::<u8>(),
            std::mem::size_of::<[i32; 2]>(),
        );
    }
    value
}

/// Extracts the NUL-terminated filesystem type name from a Darwin `statfs`.
///
/// Accepts a `libc::statfs` record. Returns the lossy UTF-8 filesystem type
/// string (for example, `"apfs"`). Guarantees bounded inspection within the
/// 16-byte `f_fstypename` buffer even without a trailing NUL.
#[cfg(target_os = "macos")]
fn macos_fstype_name(fs: &libc::statfs) -> String {
    let bytes: Vec<u8> = fs
        .f_fstypename
        .iter()
        .take_while(|&&byte| byte != 0)
        .map(|&byte| byte.to_ne_bytes()[0])
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
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

/// Returns whether any non-symlink ancestor of `output` shares `root`'s Unix inode.
///
/// Accepts normalized absolute root and output paths. Returns true when `root`
/// exists as a non-symlink and `output` or any of its existing non-symlink
/// parent directories has the same `(st_dev, st_ino)` identity, blocking
/// case-insensitive APFS aliasing (for example, `./Docs/out` against `./docs`).
/// Missing paths or symlinks return false; no directory is opened or created.
#[cfg(unix)]
fn same_inode_ancestor(root: &Path, output: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    let Ok(root_meta) = std::fs::symlink_metadata(root) else {
        return false;
    };
    if root_meta.file_type().is_symlink() {
        return false;
    }
    let target = (root_meta.dev(), root_meta.ino());
    let mut cursor = Some(output);
    while let Some(dir) = cursor {
        if let Ok(meta) = std::fs::symlink_metadata(dir)
            && !meta.file_type().is_symlink()
            && (meta.dev(), meta.ino()) == target
        {
            return true;
        }
        cursor = dir.parent();
    }
    false
}

/// Returns whether a proposed artifact directory is inside an inventory root.
///
/// Accepts root and output paths. Returns true on lexical containment after
/// absolute normalization, Unix `(st_dev, st_ino)` ancestor aliasing (blocking
/// case-insensitive APFS paths), or conservative normalization failure. No path
/// is opened; callers reject the conflicting invocation before reading inputs.
pub(crate) fn output_inside_root(root: &Path, output: &Path) -> bool {
    match (absolute_lexical(root), absolute_lexical(output)) {
        (Ok(root), Ok(output)) => {
            if output.starts_with(&root) {
                return true;
            }
            #[cfg(unix)]
            if same_inode_ancestor(&root, &output) {
                return true;
            }
            false
        }
        _ => true,
    }
}

/// Rejects existing symbolic-link path components before opening a root.
///
/// Accepts an absolute lexical path. Returns unit when existing components are
/// non-links. Links and metadata errors are returned; missing suffixes are
/// reported by the following no-follow directory open.
#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// Clears the current thread's C `errno` slot before a `readdir` call.
///
/// Accepts no parameters. Returns unit after zeroing thread-local `errno` so a
/// null `readdir` return distinguishes end-of-directory from an OS error.
/// Never fails; delegates to the platform's libc thread-local accessor.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn clear_errno() {
    #[cfg(target_os = "linux")]
    // SAFETY: errno is cleared through the libc thread-local slot exactly
    // like the standard library, so a null result can be distinguished.
    unsafe {
        *libc::__errno_location() = 0;
    }
    #[cfg(target_os = "macos")]
    // SAFETY: errno is cleared through Darwin's libc thread-local slot so a
    // null readdir result can distinguish EOF from an OS error.
    unsafe {
        *libc::__error() = 0;
    }
}

/// Reads the current thread's C `errno` value after a null `readdir` result.
///
/// Accepts no parameters. Returns the current `libc::c_int` error code (zero
/// at end-of-directory). Never fails; reads only the calling thread's slot.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn read_errno() -> libc::c_int {
    #[cfg(target_os = "linux")]
    // SAFETY: errno is read only after the null result that may set it.
    unsafe {
        *libc::__errno_location()
    }
    #[cfg(target_os = "macos")]
    // SAFETY: errno is read from Darwin's thread-local slot after null readdir.
    unsafe {
        *libc::__error()
    }
}

/// Opens a directory without following its final link.
///
/// Accepts an absolute directory path. Returns an anchored directory handle.
/// Missing paths, non-directories, links, and permission failures are returned
/// before enumeration.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in path"))?;
    // SAFETY: path is a valid NUL-terminated string and open copies it.
    let descriptor = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_entry(
    parent: &std::fs::File,
    name: &std::ffi::OsStr,
    directory: bool,
) -> std::io::Result<(std::fs::File, std::fs::Metadata)> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename"))?;
    let flags = libc::O_NOFOLLOW
        | libc::O_CLOEXEC
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent fd and name remain valid for the duration of openat.
    let descriptor = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
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
#[cfg(any(target_os = "linux", target_os = "macos"))]
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
        clear_errno();
        // SAFETY: stream comes from fdopendir; the returned record stays valid
        // until the next readdir call and remains owned by the stream.
        let entry = unsafe { libc::readdir(stream) };
        if entry.is_null() {
            if read_errno() != 0 {
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
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: ownership of the newly returned descriptor transfers to File once.
    let file = unsafe { std::fs::File::from_raw_fd(descriptor) };
    file.metadata()
}

/// Stats one directory entry relative to an anchored parent without following links.
///
/// Accepts a parent directory handle and entry name. Returns the raw Darwin
/// `libc::stat` record obtained via `fstatat(..., AT_SYMLINK_NOFOLLOW)` without
/// opening the entry, so FIFOs, devices, sockets, and dataless cloud files are
/// never opened or hydrated during classification. NUL names and OS errors are
/// returned directly.
#[cfg(target_os = "macos")]
fn entry_stat(parent: &std::fs::File, name: &std::ffi::OsStr) -> std::io::Result<libc::stat> {
    use std::os::fd::AsRawFd;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename"))?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::zeroed();
    // SAFETY: `parent` is an open directory descriptor, `name` is a valid
    // NUL-terminated string, and `stat` is writable storage for `libc::stat`.
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fstatat` returned 0 and populated `stat`.
    Ok(unsafe { stat.assume_init() })
}

/// Platform-neutral classification of one directory entry before opening it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
enum EntryKind {
    /// Entry is a symbolic link and must never be followed.
    Symlink,
    /// Entry is a subdirectory eligible for recursive descent.
    Directory {
        #[cfg(target_os = "macos")]
        dev: u64,
    },
    /// Entry is a regular file eligible for inspection unless dataless on macOS.
    RegularFile {
        #[cfg(target_os = "macos")]
        dev: u64,
        #[cfg(target_os = "macos")]
        dataless: bool,
    },
    /// Entry is a special file (FIFO, socket, device) skipped without opening.
    Other,
}

/// Classifies one Linux directory entry through `O_PATH` without opening it.
///
/// Accepts an anchored parent handle and entry name. Returns an `EntryKind`
/// distinguishing symlinks, directories, regular files, and special entries.
/// `ELOOP` maps to `EntryKind::Symlink`; other OS errors are returned.
#[cfg(target_os = "linux")]
fn classify_entry(parent: &std::fs::File, name: &std::ffi::OsStr) -> std::io::Result<EntryKind> {
    match open_entry_metadata(parent, name) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                Ok(EntryKind::Symlink)
            } else if metadata.is_dir() {
                Ok(EntryKind::Directory {})
            } else if metadata.is_file() {
                Ok(EntryKind::RegularFile {})
            } else {
                Ok(EntryKind::Other)
            }
        }
        // ELOOP is Linux's link rejection; the O_PATH metadata open already
        // yields symlink handles, so this only guards unusual kernel ports.
        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => Ok(EntryKind::Symlink),
        Err(error) => Err(error),
    }
}

/// Classifies one macOS directory entry via `fstatat` without opening it.
///
/// Accepts an anchored parent handle and entry name. Returns an `EntryKind`
/// carrying Darwin `st_dev` and `SF_DATALESS` status for pre-open mount and
/// cloud-placeholder guards. NUL names and `fstatat` errors are returned.
#[cfg(target_os = "macos")]
fn classify_entry(parent: &std::fs::File, name: &std::ffi::OsStr) -> std::io::Result<EntryKind> {
    let stat = entry_stat(parent, name)?;
    let dev = stat.st_dev as u64;
    match stat.st_mode & libc::S_IFMT {
        libc::S_IFLNK => Ok(EntryKind::Symlink),
        libc::S_IFDIR => Ok(EntryKind::Directory { dev }),
        libc::S_IFREG => Ok(EntryKind::RegularFile {
            dev,
            dataless: is_dataless_placeholder(stat.st_flags),
        }),
        _ => Ok(EntryKind::Other),
    }
}

/// Linux root mount context backed by `/proc/self/mountinfo`.
#[cfg(target_os = "linux")]
#[derive(Default)]
struct RootMount {
    /// Kernel mount identifier that every visited entry must retain.
    root_mount: u64,
    /// Current-process mount snapshot, never a recursive mount scan.
    mounts: MountTable,
}

/// macOS root mount context backed by anchored `fstatfs` and `fstat` identity.
#[cfg(target_os = "macos")]
#[derive(Default)]
struct RootMount {
    /// Device number (`st_dev`) of the anchored inventory root.
    root_dev: u64,
    /// Darwin filesystem identifier (`statfs.f_fsid`) of the anchored root.
    root_fsid: [i32; 2],
}

/// Backpressured inventory traversal state; file reports are produced by shared workers.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct Inventory<'a> {
    /// Platform mount identity that every visited entry must retain.
    mount: RootMount,
    /// Opt-in recursion flag; false lists only immediate regular files.
    recursive: bool,
    /// Deterministic admission sequence shared by success and diagnostics.
    sequence: usize,
    /// Bounded producer that blocks traversal when outstanding responses reach N.
    producer: &'a mut crate::batch::BatchProducer,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl<'a> Inventory<'a> {
    /// Creates traversal state without reading file contents.
    ///
    /// Accepts root mount context, recursion flag, and bounded batch producer.
    /// Returns an empty-sequence enumerator. No directory or native reader runs.
    fn new(
        mount: RootMount,
        recursive: bool,
        producer: &'a mut crate::batch::BatchProducer,
    ) -> Self {
        Self {
            mount,
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
        report.metadata.digest.fingerprint = None;
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
    /// Accepts a path, opaque identity from the still-open anchored handle, and
    /// an optional opened file handle that stays pinned across queueing,
    /// inspection, and identity verification to prevent inode-number reuse.
    /// Returns true after bounded admission and false after cancellation.
    /// Invalid Unicode paths produce a failed diagnostic; no contents are read
    /// by traversal.
    fn file(
        &mut self,
        path: &Path,
        expected: Option<&str>,
        pinned_file: Option<std::fs::File>,
    ) -> Result<bool, String> {
        if path.to_str().is_none() {
            return self.error(
                path,
                "non_utf8_path_unsupported",
                "report JSON cannot preserve this non-Unicode path; file was not inspected",
                OperationStatus::Failed,
            );
        }
        self.sequence += 1;
        self.producer.request_with_file(
            Value::from(self.sequence),
            path.to_path_buf(),
            expected.map(str::to_owned),
            pinned_file,
        )
    }

    /// Checks one Linux path's mount identity against the root before any entry read.
    ///
    /// Accepts a candidate path. Returns Some(true) for root mount, Some(false)
    /// after a skip diagnostic, or None after cancellation. Unknown mounts fail
    /// closed with an operational diagnostic; no candidate subtree is entered.
    #[cfg(target_os = "linux")]
    fn same_mount(&mut self, path: &Path) -> Result<Option<bool>, String> {
        match self.mount.mounts.find(path) {
            Some(mount) if mount.id == self.mount.root_mount => Ok(Some(true)),
            Some(mount) => {
                let detail = format!(
                    "mount ID {} ({}) differs from root mount {}; subtree skipped",
                    mount.id, mount.filesystem, self.mount.root_mount
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

    /// Checks one macOS entry's device number against the root volume.
    ///
    /// Accepts a candidate path, observed `st_dev`, and diagnostic scope label
    /// (`"subtree"` or `"entry"`). Returns Some(true) on match, Some(false)
    /// after emitting `cross_mount_skipped`, or None on cancellation. Queue
    /// disconnect errors are returned directly.
    #[cfg(target_os = "macos")]
    fn same_mount_device(
        &mut self,
        path: &Path,
        dev: u64,
        label: &str,
    ) -> Result<Option<bool>, String> {
        if dev == self.mount.root_dev {
            return Ok(Some(true));
        }
        let detail = format!(
            "device {dev} differs from root device {}; {label} skipped",
            self.mount.root_dev
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

    /// Re-verifies an opened macOS subdirectory stays on the root mount.
    ///
    /// Accepts the subdirectory path, its opened handle, and `fstat` metadata.
    /// Returns Some(true) when both `st_dev` and `fstatfs.f_fsid` match the
    /// root, Some(false) after emitting a diagnostic, or None on cancellation.
    /// Queue disconnect errors are returned directly.
    #[cfg(target_os = "macos")]
    fn verify_opened_directory_mount(
        &mut self,
        path: &Path,
        child_handle: &std::fs::File,
        opened: &std::fs::Metadata,
    ) -> Result<Option<bool>, String> {
        use std::os::unix::fs::MetadataExt;
        let Some(same_dev) = self.same_mount_device(path, opened.dev(), "subtree")? else {
            return Ok(None);
        };
        if !same_dev {
            return Ok(Some(false));
        }
        match fstatfs_handle(child_handle) {
            Ok(fs) if fsid_value(fs.f_fsid) == self.mount.root_fsid => Ok(Some(true)),
            Ok(_) => {
                if !self.error(
                    path,
                    "cross_mount_skipped",
                    "filesystem identifier differs from root mount; subtree skipped",
                    OperationStatus::Partial,
                )? {
                    return Ok(None);
                }
                Ok(Some(false))
            }
            Err(error) => {
                if !self.error(
                    path,
                    "mount_identity_unavailable",
                    format!("fstatfs failed for directory: {error}"),
                    OperationStatus::Failed,
                )? {
                    return Ok(None);
                }
                Ok(Some(false))
            }
        }
    }

    /// Re-verifies an opened macOS regular file stays on the root device.
    ///
    /// Accepts the file path and `fstat` metadata from the opened handle.
    /// Returns Some(true) on match, Some(false) after emitting
    /// `cross_mount_skipped`, or None on cancellation. Queue errors return Err.
    #[cfg(target_os = "macos")]
    fn verify_opened_file_mount(
        &mut self,
        path: &Path,
        opened: &std::fs::Metadata,
    ) -> Result<Option<bool>, String> {
        use std::os::unix::fs::MetadataExt;
        self.same_mount_device(path, opened.dev(), "entry")
    }

    /// Visits one sorted directory level with bounded production and opt-in recursion.
    ///
    /// Accepts an absolute reporting path, an owned anchored no-follow
    /// directory handle, and the current recursion depth from the root (0 at
    /// root). Returns true after all entries or false when cancellation stops
    /// admission. Enumeration and per-entry metadata are read through the
    /// anchored handle, never through the path, so every entry resolves against
    /// the same inode the traversal opened. When recursing into the final
    /// sorted entry of a directory, the parent handle is dropped before descent
    /// so linear chains retain O(1) directory descriptors; subdirectories beyond
    /// `MAX_DIRECTORY_DEPTH` emit an ordered `directory_depth_exceeded` partial
    /// diagnostic instead of recursing. Read/open failures become ordered
    /// records and later entries continue; names alone are sorted and file
    /// bytes/reports are never accumulated by the traversal.
    fn directory(
        &mut self,
        path: &Path,
        handle: std::fs::File,
        depth: usize,
    ) -> Result<bool, String> {
        let mut handle = Some(handle);
        let mut names = match anchored_names(handle.as_ref().expect("directory handle is open")) {
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
        let total = names.len();
        for (index, name) in names.into_iter().enumerate() {
            let child = path.join(&name);
            let kind =
                match classify_entry(handle.as_ref().expect("directory handle is open"), &name) {
                    Ok(kind) => kind,
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
            match kind {
                EntryKind::Symlink => {
                    if !self.error(
                        &child,
                        "symlink_skipped",
                        "symbolic links are never followed",
                        OperationStatus::Partial,
                    )? {
                        return Ok(false);
                    }
                }
                EntryKind::Other => {}
                EntryKind::Directory {
                    #[cfg(target_os = "macos")]
                    dev,
                } => {
                    if !self.recursive {
                        continue;
                    }
                    #[cfg(target_os = "linux")]
                    let Some(same_mount) = self.same_mount(&child)? else {
                        return Ok(false);
                    };
                    #[cfg(target_os = "macos")]
                    let Some(same_mount) = self.same_mount_device(&child, dev, "subtree")? else {
                        return Ok(false);
                    };
                    if !same_mount {
                        continue;
                    }
                    if depth >= MAX_DIRECTORY_DEPTH {
                        let detail = format!(
                            "directory recursion depth exceeds maximum of {MAX_DIRECTORY_DEPTH}; subtree skipped"
                        );
                        if !self.error(
                            &child,
                            "directory_depth_exceeded",
                            detail,
                            OperationStatus::Partial,
                        )? {
                            return Ok(false);
                        }
                        continue;
                    }
                    match open_entry(
                        handle.as_ref().expect("directory handle is open"),
                        &name,
                        true,
                    ) {
                        Ok((child_handle, opened)) => {
                            #[cfg(target_os = "linux")]
                            let _ = opened;
                            #[cfg(target_os = "macos")]
                            {
                                let Some(same_mount) = self.verify_opened_directory_mount(
                                    &child,
                                    &child_handle,
                                    &opened,
                                )?
                                else {
                                    return Ok(false);
                                };
                                if !same_mount {
                                    continue;
                                }
                            }
                            if index + 1 == total {
                                drop(handle.take());
                            }
                            if !self.directory(&child, child_handle, depth + 1)? {
                                return Ok(false);
                            }
                        }
                        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                            if !self.error(
                                &child,
                                "symlink_skipped",
                                "symbolic links are never followed",
                                OperationStatus::Partial,
                            )? {
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
                }
                EntryKind::RegularFile {
                    #[cfg(target_os = "macos")]
                    dev,
                    #[cfg(target_os = "macos")]
                    dataless,
                } => {
                    #[cfg(target_os = "macos")]
                    if dataless {
                        if !self.error(
                            &child,
                            "cloud_placeholder_skipped",
                            "cloud-evicted dataless placeholder skipped without hydrating",
                            OperationStatus::Partial,
                        )? {
                            return Ok(false);
                        }
                        continue;
                    }
                    #[cfg(target_os = "linux")]
                    let Some(same_mount) = self.same_mount(&child)? else {
                        return Ok(false);
                    };
                    #[cfg(target_os = "macos")]
                    let Some(same_mount) = self.same_mount_device(&child, dev, "entry")? else {
                        return Ok(false);
                    };
                    if !same_mount {
                        continue;
                    }
                    match open_entry(
                        handle.as_ref().expect("directory handle is open"),
                        &name,
                        false,
                    ) {
                        Ok((file, opened)) => {
                            #[cfg(target_os = "macos")]
                            {
                                let Some(same_mount) =
                                    self.verify_opened_file_mount(&child, &opened)?
                                else {
                                    return Ok(false);
                                };
                                if !same_mount {
                                    continue;
                                }
                            }
                            use std::os::unix::fs::MetadataExt;
                            let expected = format!("{:x}:{:x}", opened.dev(), opened.ino());
                            if !self.file(&child, Some(&expected), Some(file))? {
                                return Ok(false);
                            }
                        }
                        Err(error) if error.raw_os_error() == Some(libc::ELOOP) => {
                            if !self.error(
                                &child,
                                "symlink_skipped",
                                "symbolic links are never followed",
                                OperationStatus::Partial,
                            )? {
                                return Ok(false);
                            }
                        }
                        Err(error) => {
                            if !self.error(
                                &child,
                                "file_open_failed",
                                error,
                                OperationStatus::Failed,
                            )? {
                                return Ok(false);
                            }
                        }
                    }
                }
            }
        }
        Ok(true)
    }
}

/// Produces safe Linux inventory requests while retaining the current mount snapshot.
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
            let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
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
            let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
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
    let mount = RootMount {
        root_mount: root_record.as_ref().map_or(0, |mount| mount.id),
        mounts,
    };
    let mut inventory = Inventory::new(mount, recursive, producer);
    match root_record {
        Some(mount) if !known_local_filesystem(&mount.filesystem) => {
            let detail = format!(
                "inventory root filesystem '{}' is not an approved local filesystem",
                mount.filesystem
            );
            inventory.error(
                &root,
                "non_local_root_rejected",
                detail,
                OperationStatus::Failed,
            )?;
        }
        None => {
            inventory.error(
                &root,
                "mount_identity_unavailable",
                "inventory root is absent from mountinfo",
                OperationStatus::Failed,
            )?;
        }
        Some(_) => match open_directory(&root) {
            Ok(handle) => {
                inventory.directory(&root, handle, 0)?;
            }
            Err(error) => {
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

/// Produces safe macOS inventory requests anchored to an `fstatfs`-verified local root.
///
/// Accepts root, recursion flag, and bounded producer. Returns unit after full
/// traversal or cancellation. Rejects symlink components before opening the root
/// via `open_directory`, verifies local filesystem status through `fstatfs` on
/// the opened descriptor before enumerating any entries, and turns validation or
/// open errors into one failed record; producer disconnect errors return Err.
#[cfg(target_os = "macos")]
fn produce(
    root: &Path,
    recursive: bool,
    producer: &mut crate::batch::BatchProducer,
) -> Result<(), String> {
    use std::os::unix::fs::MetadataExt;
    let root = match absolute_lexical(root).and_then(|root| {
        reject_symlink_components(&root)?;
        Ok(root)
    }) {
        Ok(root) => root,
        Err(error) => {
            let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
            inventory.error(
                root,
                "inventory_root_rejected",
                error,
                OperationStatus::Failed,
            )?;
            return Ok(());
        }
    };
    let handle = match open_directory(&root) {
        Ok(handle) => handle,
        Err(error) => {
            let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
            inventory.error(
                &root,
                "directory_open_failed",
                error,
                OperationStatus::Failed,
            )?;
            return Ok(());
        }
    };
    let fs = match fstatfs_handle(&handle) {
        Ok(fs) => fs,
        Err(error) => {
            let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
            inventory.error(
                &root,
                "mount_identity_unavailable",
                format!("fstatfs failed for inventory root: {error}"),
                OperationStatus::Failed,
            )?;
            return Ok(());
        }
    };
    let root_meta = match handle.metadata() {
        Ok(meta) => meta,
        Err(error) => {
            let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
            inventory.error(
                &root,
                "mount_identity_unavailable",
                format!("fstat failed for inventory root: {error}"),
                OperationStatus::Failed,
            )?;
            return Ok(());
        }
    };
    let filesystem = macos_fstype_name(&fs);
    if !known_local_macos_filesystem(fs.f_flags, &filesystem) {
        let detail =
            format!("inventory root filesystem '{filesystem}' is not an approved local filesystem");
        let mut inventory = Inventory::new(RootMount::default(), recursive, producer);
        inventory.error(
            &root,
            "non_local_root_rejected",
            detail,
            OperationStatus::Failed,
        )?;
        return Ok(());
    }
    let mount = RootMount {
        root_dev: root_meta.dev(),
        root_fsid: fsid_value(fs.f_fsid),
    };
    let mut inventory = Inventory::new(mount, recursive, producer);
    inventory.directory(&root, handle, 0)?;
    Ok(())
}

/// Runs deterministic ordered inventory with bounded shared file/OCR workers.
///
/// Accepts root, recursion flag, positive jobs, optional external artifact
/// directory, `full_hash` override, `verbose` provenance flag, and JSONL
/// writer. Returns aggregate status after all schema v3 responses flush and
/// workers join. Output/input transport errors are returned directly.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn run<W: Write>(
    root: &Path,
    recursive: bool,
    jobs: usize,
    output_dir: Option<&Path>,
    full_hash: bool,
    verbose: bool,
    mut output: W,
) -> std::io::Result<OperationStatus> {
    crate::batch::run(
        jobs,
        move |producer| produce(root, recursive, producer),
        |budget, _cancellation| {
            let mut inspector = Inspector::with_shared_budget(budget).with_full_hash(full_hash);
            let options = InspectOptions::default();
            move |mut request| {
                let sequence = request.sequence as usize;
                let expected = request.expected.clone();
                let pinned = request.pinned_file.take();
                let mut report = inspector.inspect(&request.path, &options);
                Inventory::check_identity(&mut report, expected.as_deref());
                drop(pinned);
                if let Some(directory) = output_dir {
                    crate::output::persist_metadata(&mut report, directory, sequence, verbose);
                }
                let status = report.metadata.status;
                crate::batch::BatchCompletion::report(
                    request,
                    status,
                    serde_json::to_value(report.to_v3(verbose)),
                )
            }
        },
        &mut output,
    )
    .map(|outcome| outcome.status)
    .map_err(std::io::Error::other)
}

/// Reports unsupported directory inventory without touching the host filesystem.
///
/// Accepts root, recursion flag, jobs, artifact directory, `full_hash`
/// override, `verbose` flag, and output writer. Returns an I/O unsupported
/// error; no paths, mounts, network endpoints, or output files are accessed on
/// unsupported hosts.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn run<W: Write>(
    _root: &Path,
    _recursive: bool,
    _jobs: usize,
    _output_dir: Option<&Path>,
    _full_hash: bool,
    _verbose: bool,
    _output: W,
) -> std::io::Result<OperationStatus> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "mount-safe directory inventory currently requires Linux or macOS",
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
        assert_eq!(root.filesystem, "ext4");
        assert_eq!(bind.id, 100);
        assert_eq!(bind.parent_id, 35);
        assert_eq!(bind.filesystem, "ext4");
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
        let file_mount = table.find(Path::new("/data/file")).unwrap();
        assert_eq!(file_mount.id, 37);
        assert_eq!(file_mount.filesystem, "ext4");
        assert_eq!(table.find(Path::new("/data")).unwrap().id, 37);
    }

    /// Checks macOS local-filesystem allowlist enforces `MNT_LOCAL` and known types.
    ///
    /// Accepts no parameters. Returns unit after checking approved local types,
    /// cleared `MNT_LOCAL` flags, and rejected network, FUSE, pseudo, and
    /// unknown filesystem names. Unexpected acceptance or rejection panics.
    #[test]
    fn macos_local_filesystem_allowlist_requires_mnt_local_and_known_type() {
        for filesystem in ["apfs", "hfs", "msdos", "exfat", "udf", "tmpfs"] {
            assert!(
                known_local_macos_filesystem(MACOS_MNT_LOCAL, filesystem),
                "expected local allowlist acceptance for {filesystem}"
            );
            assert!(
                !known_local_macos_filesystem(0, filesystem),
                "cleared MNT_LOCAL must reject {filesystem}"
            );
        }
        assert!(known_local_macos_filesystem(
            MACOS_MNT_LOCAL | 0x0000_0001,
            "apfs"
        ));
        for filesystem in [
            "smbfs", "nfs", "afpfs", "webdav", "macfuse", "osxfuse", "fusefs", "lifs", "devfs",
            "autofs", "synthfs", "nullfs", "unknown",
        ] {
            assert!(
                !known_local_macos_filesystem(MACOS_MNT_LOCAL, filesystem),
                "non-allowlisted filesystem must fail closed: {filesystem}"
            );
        }
    }

    /// Checks Darwin `SF_DATALESS` placeholder detection on raw `st_flags`.
    ///
    /// Accepts no parameters. Returns unit after checking zero, non-dataless,
    /// exact `SF_DATALESS`, and combined flag words. Misclassification panics;
    /// no filesystem I/O occurs.
    #[test]
    fn dataless_placeholder_flag_is_detected() {
        assert!(!is_dataless_placeholder(0));
        assert!(!is_dataless_placeholder(0x0000_0001));
        assert!(is_dataless_placeholder(0x4000_0000));
        assert!(is_dataless_placeholder(0x4000_0001));
    }

    /// Checks `output_inside_root` lexical and Unix `(st_dev, st_ino)` containment.
    ///
    /// Accepts no parameters. Returns unit after checking nested existing,
    /// nested non-existent, sibling, and case-alias paths under a scratch
    /// directory. Incorrect containment results panic; the scratch tree is
    /// removed afterward.
    #[cfg(unix)]
    #[test]
    fn output_inside_root_checks_lexical_and_unix_inode_ancestors() {
        use std::os::unix::fs::MetadataExt;
        let base_parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let base = base_parent.join(format!("inside-root-{}-{stamp}", std::process::id()));
        let root_dir = base.join("docs");
        let nested_existing = root_dir.join("out");
        let sibling_dir = base.join("other");
        std::fs::create_dir_all(&nested_existing).expect("nested directory created");
        std::fs::create_dir_all(&sibling_dir).expect("sibling directory created");
        let base = std::fs::canonicalize(&base).expect("base directory canonicalized");
        let root_dir = base.join("docs");
        let nested_existing = root_dir.join("out");
        let nested_missing = root_dir.join("missing/nested");
        let sibling_dir = base.join("other");

        assert!(output_inside_root(&root_dir, &root_dir));
        assert!(output_inside_root(&root_dir, &nested_existing));
        assert!(output_inside_root(&root_dir, &nested_missing));
        assert!(!output_inside_root(&root_dir, &sibling_dir));

        assert!(same_inode_ancestor(&root_dir, &root_dir));
        assert!(same_inode_ancestor(&root_dir, &nested_existing));
        assert!(same_inode_ancestor(&root_dir, &nested_missing));
        assert!(!same_inode_ancestor(&root_dir, &sibling_dir));

        let upper_root = base.join("DOCS");
        let upper_out = upper_root.join("new-out");
        let root_meta = std::fs::symlink_metadata(&root_dir).expect("root metadata");
        let case_insensitive = std::fs::symlink_metadata(&upper_root)
            .map(|meta| (meta.dev(), meta.ino()) == (root_meta.dev(), root_meta.ino()))
            .unwrap_or(false);
        assert_eq!(output_inside_root(&root_dir, &upper_out), case_insensitive);

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Checks anchored enumeration reads every name through the open handle.
    ///
    /// Accepts no parameters. Returns unit after enumerating one scratch
    /// directory containing nested, spaced, and linked entries. A missing
    /// entry, a leaked "." or ".." record, or a path-based read panic fails
    /// the anchored-traversal contract; the scratch tree is removed after.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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

    /// Checks enumeration returns short and NAME_MAX names without overreads.
    ///
    /// Accepts no parameters. Returns unit after enumerating one scratch
    /// directory holding a one-byte name (the shortest kernel record) and a
    /// 255-byte NAME_MAX name (the longest record). A missing or truncated
    /// name fails the raw d_name access contract; the scratch tree is removed
    /// after the assertions.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn anchored_enumeration_reads_short_and_long_names() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("anchored-len-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory created");
        std::fs::write(directory.join("a"), b"short").expect("short name created");
        let long_name = "x".repeat(255);
        std::fs::write(directory.join(&long_name), b"long").expect("long name created");
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
            ]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks Linux enumeration preserves arbitrary non-UTF-8 byte filenames.
    ///
    /// Accepts no parameters. Returns unit after enumerating one scratch
    /// directory holding a raw non-UTF-8 filename (`[0xff, 0xfe, b'z']`). A
    /// mis-decoded byte sequence fails the raw d_name contract; gated to Linux
    /// because macOS APFS rejects non-UTF-8 filenames at creation.
    #[cfg(target_os = "linux")]
    #[test]
    fn anchored_enumeration_reads_non_utf8_names() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("anchored-bytes-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory created");
        let raw_name = std::ffi::OsStr::from_bytes(&[0xff, 0xfe, b'z']);
        std::fs::write(directory.join(raw_name), b"bytes").expect("non-UTF-8 name created");
        let handle = open_directory(&directory).expect("scratch directory opened");
        let names = anchored_names(&handle).expect("anchored enumeration succeeds");
        drop(handle);
        assert_eq!(
            names,
            vec![std::ffi::OsString::from_vec(vec![0xff, 0xfe, b'z'])]
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Counts open file descriptors resolving to one absolute path on Linux.
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

    /// Counts open file descriptors resolving to one absolute path on macOS.
    ///
    /// Accepts a canonical absolute path. Returns the number of currently open
    /// descriptors whose `fcntl(fd, F_GETPATH, ...)` result equals `path`,
    /// scanning numeric descriptor entries in `/dev/fd`. Unreadable entries or
    /// non-vnode descriptors are skipped without failing the caller.
    #[cfg(target_os = "macos")]
    fn count_descriptors_for(path: &Path) -> usize {
        let Ok(entries) = std::fs::read_dir("/dev/fd") else {
            return 0;
        };
        let mut count = 0;
        for entry in entries.flatten() {
            let Some(fd) = entry
                .file_name()
                .to_str()
                .and_then(|name| name.parse::<libc::c_int>().ok())
            else {
                continue;
            };
            let mut buf = [0u8; libc::MAXPATHLEN as usize];
            // SAFETY: `F_GETPATH` writes a NUL-terminated path of at most
            // `MAXPATHLEN` bytes into `buf` when returning != -1.
            if unsafe { libc::fcntl(fd, libc::F_GETPATH, buf.as_mut_ptr().cast::<libc::c_char>()) }
                == -1
            {
                continue;
            }
            let len = buf.iter().position(|&byte| byte == 0).unwrap_or(buf.len());
            if Path::new(std::ffi::OsStr::from_bytes(&buf[..len])) == path {
                count += 1;
            }
        }
        count
    }

    /// Checks a failed enumeration closes its duplicate and keeps the caller's
    /// handle, and that a successful enumeration releases the stream.
    ///
    /// Accepts no parameters. Returns unit after one fdopendir failure on a
    /// regular-file handle and one successful directory enumeration. A leaked
    /// duplicated descriptor, a closed caller handle, or a stream left open
    /// after success fails the descriptor-ownership contract; the scratch
    /// tree is removed after the assertions.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
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

    /// Checks tail recursion drops parent directory handles while pinning regular files.
    ///
    /// Accepts no parameters. Returns unit after traversing a single-child
    /// directory chain through `produce` and inspecting open descriptors inside
    /// the worker. Retained parent directory descriptors or an unpinned regular
    /// file fail the assertions; the scratch tree is removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn tail_recursion_drops_parent_directory_fds_and_pins_regular_file() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let base = root.join(format!("tail-fd-{}-{stamp}", std::process::id()));
        let sub_a = base.join("a");
        let sub_b = sub_a.join("b");
        let sub_c = sub_b.join("c");
        std::fs::create_dir_all(&sub_c).expect("nested chain created");
        let leaf = sub_c.join("leaf.txt");
        std::fs::write(&leaf, b"pinned leaf").expect("leaf file created");

        let canonical_base = std::fs::canonicalize(&base).expect("base canonicalized");
        let canonical_a = std::fs::canonicalize(&sub_a).expect("sub_a canonicalized");
        let canonical_b = std::fs::canonicalize(&sub_b).expect("sub_b canonicalized");
        let canonical_leaf = std::fs::canonicalize(&leaf).expect("leaf canonicalized");

        let mut output = Vec::new();
        let observed = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let observed_worker = std::sync::Arc::clone(&observed);
        let outcome = crate::batch::run(
            1,
            move |producer| produce(&canonical_base, true, producer),
            |_budget, _cancellation| {
                let observed_worker = std::sync::Arc::clone(&observed_worker);
                let canonical_a = canonical_a.clone();
                let canonical_b = canonical_b.clone();
                let canonical_leaf = canonical_leaf.clone();
                let base_check = std::fs::canonicalize(&base).expect("base canonicalized");
                move |mut request| {
                    observed_worker.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    assert_eq!(request.path, canonical_leaf);
                    assert_eq!(
                        count_descriptors_for(&base_check),
                        0,
                        "root directory FD must be dropped before tail recursion"
                    );
                    assert_eq!(
                        count_descriptors_for(&canonical_a),
                        0,
                        "intermediate directory FD must be dropped before tail recursion"
                    );
                    assert_eq!(
                        count_descriptors_for(&canonical_b),
                        0,
                        "parent directory FD must be dropped before tail recursion"
                    );
                    assert!(
                        request.pinned_file.is_some(),
                        "regular-file handle must stay pinned on BatchRequest"
                    );
                    assert!(
                        count_descriptors_for(&canonical_leaf) >= 1,
                        "pinned regular file must remain open during worker execution"
                    );
                    drop(request.pinned_file.take());
                    assert_eq!(
                        count_descriptors_for(&canonical_leaf),
                        0,
                        "dropping the pinned handle must release the descriptor"
                    );
                    crate::batch::BatchCompletion::report(
                        request,
                        OperationStatus::Complete,
                        Ok(serde_json::json!({"ok": true})),
                    )
                }
            },
            &mut output,
        )
        .expect("tail-recursive inventory batch succeeds");
        assert_eq!(outcome.status, OperationStatus::Complete);
        assert_eq!(observed.load(std::sync::atomic::Ordering::SeqCst), 1);
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Checks recursion enforces `MAX_DIRECTORY_DEPTH` with ordered partial diagnostics.
    ///
    /// Accepts no parameters. Returns unit after running `inventory::run` over
    /// a chain of `MAX_DIRECTORY_DEPTH` nested directories containing sibling
    /// files and one deeper subdirectory. Missing leaf files, traversed depth
    /// beyond the cap, wrong ordering, or wrong error code fail the assertions;
    /// the scratch tree is removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn recursion_enforces_max_directory_depth_in_sorted_order() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let base = root.join(format!("depth-limit-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&base).expect("base directory created");
        let base = std::fs::canonicalize(&base).expect("base directory canonicalized");
        let mut deepest = base.clone();
        for _ in 0..MAX_DIRECTORY_DEPTH {
            deepest.push("d");
        }
        let too_deep = deepest.join("m_too_deep");
        std::fs::create_dir_all(&too_deep).expect("deep chain created");
        let at_limit = deepest.join("a_at_limit.txt");
        let after_limit = deepest.join("z_after.txt");
        let skipped_leaf = too_deep.join("never_visited.txt");
        std::fs::write(&at_limit, b"at limit").expect("leaf at limit created");
        std::fs::write(&after_limit, b"after limit").expect("sibling file created");
        std::fs::write(&skipped_leaf, b"too deep").expect("deep leaf created");

        let mut output = Vec::new();
        let status = run(&base, true, 2, None, false, false, &mut output)
            .expect("inventory run completes without transport error");
        assert_eq!(status, OperationStatus::Partial);

        let lines: Vec<Value> = String::from_utf8(output)
            .expect("valid UTF-8 JSONL")
            .lines()
            .map(|line| serde_json::from_str(line).expect("valid JSON line"))
            .collect();
        assert_eq!(lines.len(), 3, "expected 2 files + 1 depth diagnostic");
        assert_eq!(lines[0]["sequence"], 1);
        assert_eq!(lines[0]["status"], "complete");
        assert_eq!(lines[0]["path"], at_limit.to_str().unwrap());
        assert_eq!(lines[1]["sequence"], 2);
        assert_eq!(lines[1]["status"], "partial");
        assert_eq!(lines[1]["path"], too_deep.to_str().unwrap());
        assert_eq!(lines[1]["error"]["code"], "directory_depth_exceeded");
        assert_eq!(lines[2]["sequence"], 3);
        assert_eq!(lines[2]["status"], "complete");
        assert_eq!(lines[2]["path"], after_limit.to_str().unwrap());

        let _ = std::fs::remove_dir_all(&base);
    }
}
