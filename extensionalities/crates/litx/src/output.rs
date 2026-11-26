//! Module: glassbox-cli private artifact publication.
//! File: output.rs.
//! Responsibility: Save schema v3 metadata, extraction, and media artifacts through no-overwrite, no-follow, complete-file and streaming publication.

use glassbox_inspect::reports::{
    Artifact, ArtifactKind, Completeness, Diagnostic, DiagnosticSeverity, DiagnosticStage,
    DigestReport, DocumentMetadata, ExtractReport, MetadataReport, OperationStatus,
};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use std::os::unix::ffi::OsStrExt;

/// One fully encoded artifact awaiting private temporary-file publication.
#[cfg_attr(not(any(target_os = "linux", target_os = "macos")), allow(dead_code))]
struct PendingArtifact {
    /// Public path and typed artifact facts included in the v3 report.
    artifact: Artifact,
    /// Complete bytes written before any public directory entry is created.
    bytes: Vec<u8>,
}

/// Artifact failure with any already published complete files preserved.
struct OutputFailure {
    /// Operational diagnostic safe for the report and stderr.
    message: String,
    /// Fully written files linked before a later publication or sync failure.
    published: Vec<Artifact>,
}

/// Converts an arbitrary OS error into an artifact publication failure.
///
/// Accepts an operation context, optional already published artifacts, and the
/// original error. Returns an OutputFailure with no source-body content. This
/// helper performs no filesystem changes and cannot fail.
fn failure(context: &str, published: Vec<Artifact>, error: impl ToString) -> OutputFailure {
    OutputFailure {
        message: format!("{context}: {}", error.to_string()),
        published,
    }
}

/// Produces an absolute lexical output path without resolving symbolic links.
///
/// Accepts a user-supplied directory. Returns an absolute normalized path.
/// Missing current directories and root escapes return errors; no directory is
/// created and no existing path is modified.
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
                        "output path escapes filesystem root",
                    ));
                }
            }
            Component::Normal(part) => normalized.push(part),
        }
    }
    Ok(normalized)
}

/// Opens one single-component child directory relative to an anchored parent descriptor without following symlinks.
///
/// Accepts the parent directory descriptor and a NUL-terminated single normal
/// component name. Returns an owned `File` handle for the child directory on
/// success. Returns the underlying OS error (such as `ENOENT`, `ELOOP`,
/// `ENOTDIR`, or `EACCES`) when the entry is missing, is a symbolic link, is
/// not a directory, or cannot be opened; performs no mutations.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_dir_at(parent_fd: i32, name: &std::ffi::CStr) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: parent_fd is an open anchored directory descriptor and name is
    // NUL-terminated; O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC rejects links and
    // non-directories atomically without inheriting the fd across exec.
    let descriptor = unsafe {
        libc::openat(
            parent_fd,
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: this function owns the newly returned unique descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }
}

/// Classifies an anchored `openat` directory failure into clear `InvalidInput` symlink/non-dir diagnostics.
///
/// Accepts the anchored parent descriptor, the single-component child name,
/// the accumulated display path, and the raw `openat` error. Returns
/// `InvalidInput` with a symlink or non-directory message when `openat`
/// failed with `ELOOP` or `ENOTDIR` (inspecting via anchored `fstatat` with
/// `AT_SYMLINK_NOFOLLOW` on `ENOTDIR` so symlink-to-file entries report as
/// symlinks on both Linux and macOS); passes all other OS errors through
/// unchanged. Performs no writes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn classify_anchored_dir_open_error(
    parent_fd: i32,
    name: &std::ffi::CStr,
    walked: &Path,
    error: std::io::Error,
) -> std::io::Error {
    match error.raw_os_error() {
        Some(libc::ELOOP) => std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "symbolic-link output component rejected: {}",
                walked.display()
            ),
        ),
        Some(libc::ENOTDIR) => {
            let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: parent_fd is an open directory descriptor, name is
            // NUL-terminated, and st is a valid writable libc::stat pointer.
            let is_symlink = unsafe {
                libc::fstatat(
                    parent_fd,
                    name.as_ptr(),
                    st.as_mut_ptr(),
                    libc::AT_SYMLINK_NOFOLLOW,
                ) == 0
                    && (st.assume_init().st_mode & libc::S_IFMT) == libc::S_IFLNK
            };
            if is_symlink {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "symbolic-link output component rejected: {}",
                        walked.display()
                    ),
                )
            } else {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("output component is not a directory: {}", walked.display()),
                )
            }
        }
        _ => error,
    }
}

/// Walks an absolute normalized directory path from `/` using anchored descriptors, creating missing components `0700`.
///
/// Accepts an absolute lexically normalized directory path and
/// `require_private_final_dir` (`true` for `--output-dir` / `StagedWriter` to
/// enforce `mode & 0o077 == 0` via `fstat` on the final anchored descriptor;
/// `false` for `-o PATH` single-file publication so existing `0755` parents
/// remain permitted). Returns the owned anchored `File` descriptor of the
/// final directory so callers never re-open the directory by path. Rejects
/// non-absolute or unnormalized paths, NUL bytes, symbolic-link components
/// (`ELOOP`), and non-directory components (`ENOTDIR`) with `InvalidInput`;
/// creates missing components via `mkdirat(..., 0o700)` plus `fchmod(..., 0o700)`
/// (tolerating concurrent `EEXIST` directory creators while rejecting race-window
/// symlinks/files); rejects non-private final directories with `PermissionDenied`
/// when `require_private_final_dir` is `true`; propagates all other OS errors.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_anchored_directory(
    path: &Path,
    require_private_final_dir: bool,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd, FromRawFd};

    if !path.is_absolute() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "anchored output directory path must be absolute",
        ));
    }

    let root_cstr = std::ffi::CString::new("/").expect("static root path has no NUL");
    // SAFETY: root_cstr is NUL-terminated; O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC
    // opens `/` as an anchored directory handle without following symlinks.
    let root_fd = unsafe {
        libc::open(
            root_cstr.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: this function owns the newly returned unique root descriptor.
    let mut current = unsafe { std::fs::File::from_raw_fd(root_fd) };
    let mut walked = PathBuf::from("/");

    for component in path.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::ParentDir | Component::Prefix(_) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "unnormalized output directory path rejected: {}",
                        path.display()
                    ),
                ));
            }
            Component::Normal(part) => {
                walked.push(part);
                let name = std::ffi::CString::new(part.as_bytes()).map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "NUL in output directory component",
                    )
                })?;
                let next = match open_dir_at(current.as_raw_fd(), &name) {
                    Ok(handle) => handle,
                    Err(error) if error.raw_os_error() == Some(libc::ENOENT) => {
                        // SAFETY: current descriptor is valid and name is NUL-terminated;
                        // explicit libc::mode_t cast satisfies both Linux and Darwin ABIs.
                        let mkdir_rc = unsafe {
                            libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o700 as libc::mode_t)
                        };
                        let created_by_us = if mkdir_rc == 0 {
                            true
                        } else {
                            let mkdir_err = std::io::Error::last_os_error();
                            if mkdir_err.raw_os_error() == Some(libc::EEXIST) {
                                false
                            } else {
                                return Err(mkdir_err);
                            }
                        };
                        let handle =
                            open_dir_at(current.as_raw_fd(), &name).map_err(|open_err| {
                                classify_anchored_dir_open_error(
                                    current.as_raw_fd(),
                                    &name,
                                    &walked,
                                    open_err,
                                )
                            })?;
                        if created_by_us {
                            // SAFETY: handle is an open directory descriptor created by
                            // this call; fchmod restores 0700 without path re-lookup so
                            // umask cannot strip owner bits or leave group/other bits.
                            let chmod_rc =
                                unsafe { libc::fchmod(handle.as_raw_fd(), 0o700 as libc::mode_t) };
                            if chmod_rc != 0 {
                                return Err(std::io::Error::last_os_error());
                            }
                        }
                        handle
                    }
                    Err(error) => {
                        return Err(classify_anchored_dir_open_error(
                            current.as_raw_fd(),
                            &name,
                            &walked,
                            error,
                        ));
                    }
                };
                current = next;
            }
        }
    }

    if require_private_final_dir {
        use std::os::unix::fs::PermissionsExt;
        // Inspect the anchored descriptor directly via fstat (zero path re-lookup).
        let mode = current.metadata()?.permissions().mode();
        if mode & 0o077 != 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "existing output directory must have mode 0700 or stricter: {}",
                    path.display()
                ),
            ));
        }
    }

    Ok(current)
}

/// Checks via anchored `fstatat(AT_SYMLINK_NOFOLLOW)` that no symlink or existing entry occupies the target name.
///
/// Accepts the anchored parent directory descriptor, a validated single-component
/// NUL-terminated target filename, and the display path for diagnostics. Returns
/// `Ok(())` only when the entry does not exist (`ENOENT`). Returns
/// `InvalidInput` when the entry is a symbolic link (live or dangling),
/// `AlreadyExists` when any other entry already occupies the name, or the
/// underlying OS error on any other `fstatat` failure. Performs no writes.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn ensure_target_absent(
    parent_fd: i32,
    target_name: &std::ffi::CStr,
    target_path: &Path,
) -> std::io::Result<()> {
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: parent_fd is an open anchored directory descriptor, target_name
    // is NUL-terminated, and st is a valid writable libc::stat pointer;
    // AT_SYMLINK_NOFOLLOW inspects the entry itself without following symlinks.
    let rc = unsafe {
        libc::fstatat(
            parent_fd,
            target_name.as_ptr(),
            st.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        // SAFETY: fstatat returned 0, so st is initialized.
        let st = unsafe { st.assume_init() };
        if (st.st_mode & libc::S_IFMT) == libc::S_IFLNK {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "symbolic-link output target rejected: {}",
                    target_path.display()
                ),
            ));
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!("output file already exists: {}", target_path.display()),
        ));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(())
    } else {
        Err(error)
    }
}

/// Walks every directory component, rejecting symlinks or non-directories and creating missing ones privately.
///
/// Accepts an absolute normalized directory path. Returns unit after every
/// component exists as a real directory. Any symbolic-link or non-directory
/// component returns `InvalidInput`; missing components are created atomically
/// with mode `0700` via [`create_private_directory`]; already-existing
/// directories retain their existing permissions (including ordinary `0755`
/// user directories). All filesystem errors are returned without modifying
/// existing entries.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn ensure_directory_tree(path: &Path) -> std::io::Result<()> {
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
                        "symbolic-link output component rejected: {}",
                        current.display()
                    ),
                ));
            }
            Ok(metadata) if !metadata.is_dir() => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!("output component is not a directory: {}", current.display()),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                create_private_directory(&current)?;
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Creates missing output components privately and rejects links or public roots.
///
/// Accepts an output directory path. Returns unit after ensuring each
/// component is a real directory (using an anchored descriptor walk from `/`
/// on Linux and macOS). Missing directories are created atomically with 0700
/// and concurrent creators of the same component are tolerated by
/// revalidation; an existing final directory must already forbid group/other
/// access. Existing ancestor permissions are preserved, and all filesystem
/// errors are returned.
fn prepare_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        let normalized = absolute_lexical(path)?;
        let _handle = open_anchored_directory(&normalized, true)?;
        Ok(())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        ensure_directory_tree(path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::symlink_metadata(path)?.permissions().mode();
            if mode & 0o077 != 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    format!(
                        "existing output directory must have mode 0700 or stricter: {}",
                        path.display()
                    ),
                ));
            }
        }
        Ok(())
    }
}

/// Creates one missing output component privately, tolerating concurrent creators.
///
/// Accepts the absolute missing component path. Returns unit after one atomic
/// no-replace creation or revalidation of a concurrently created entry. A lost
/// creation race is resolved by revalidation: a concurrently created real
/// directory is accepted unchanged, while a link or file squatted into the
/// race window is rejected with the original creation error. Creation pins
/// mode 0700 before any entry becomes visible, and the explicit permission
/// restore keeps umask-stripped owner bits intact.
#[cfg(any(test, not(any(target_os = "linux", target_os = "macos"))))]
fn create_private_directory(path: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    let created = {
        use std::os::unix::fs::DirBuilderExt;
        let mut builder = std::fs::DirBuilder::new();
        builder.mode(0o700);
        builder.create(path)
    };
    #[cfg(not(unix))]
    let created = std::fs::create_dir(path);
    match created {
        Ok(()) => {
            #[cfg(unix)]
            std::fs::set_permissions(path, std::os::unix::fs::PermissionsExt::from_mode(0o700))?;
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            match std::fs::symlink_metadata(path) {
                Ok(metadata) if metadata.is_dir() => Ok(()),
                _ => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

/// Validates or creates the private output directory once, before workers start.
///
/// Accepts the user-supplied output directory. Returns the absolute validated
/// path after every missing component exists as a real private directory.
/// Concurrent creators of the same components are tolerated through atomic
/// creation and revalidation; symbolic links, non-directory components, root
/// escapes, and group/other-accessible final directories are rejected without
/// modifying any existing entry. No output file is written and no existing
/// name is ever replaced.
pub(crate) fn initialize_output_directory(directory: &Path) -> std::io::Result<PathBuf> {
    let directory = absolute_lexical(directory)?;
    prepare_directory(&directory)?;
    Ok(directory)
}

/// Opens the final output directory without following a symbolic link.
///
/// Accepts a validated absolute directory path. Returns an owned directory
/// handle anchored for private temporary file creation. Missing paths, links,
/// and host open errors are returned without creating output files.
#[cfg(all(any(target_os = "linux", target_os = "macos"), test))]
fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let path_string = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in output path"))?;
    // SAFETY: path_string is NUL-terminated and alive for the call; open copies it.
    let descriptor = unsafe {
        libc::open(
            path_string.as_ptr(),
            libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: this function owns the newly returned unique descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }
}

/// One prepared artifact awaiting its final public name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
enum PreparedFile {
    /// Unnamed temporary inode created through O_TMPFILE and held open until
    /// one of the descriptor linking strategies publishes it.
    #[cfg(target_os = "linux")]
    Anonymous(std::fs::File),
    /// Private staged temporary name holding the complete bytes. On Linux this
    /// is used when the kernel or filesystem rejects anonymous O_TMPFILE
    /// creation; on macOS it is the primary publication path. The guard removes
    /// the staged name on drop unless publication disarms it.
    Staged(StagedName),
}

/// Opens one new anonymous temporary inode inside the output directory.
///
/// Accepts the anchored directory descriptor and the NUL-terminated directory
/// path. Returns the open writable 0600 unnamed inode; kernels and filesystems
/// that reject O_TMPFILE return their error so the caller can select the
/// staged fallback.
#[cfg(target_os = "linux")]
fn open_anonymous(directory_fd: i32, dot: &std::ffi::CStr) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    // SAFETY: dot is NUL-terminated and O_TMPFILE creates a new unnamed inode;
    // passing mode as c_uint satisfies variadic openat ABI promotion rules.
    let descriptor = unsafe {
        libc::openat(
            directory_fd,
            dot.as_ptr(),
            libc::O_WRONLY | libc::O_TMPFILE | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if descriptor < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: the caller owns the newly returned unique descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }
}

/// Reports whether anonymous temporary creation is unavailable on this host.
///
/// Accepts the raw O_TMPFILE openat error. Returns true for the error codes
/// Linux uses when the kernel or filesystem rejects O_TMPFILE — EOPNOTSUPP,
/// EISDIR, EINVAL, and old-kernel ENOENT — so the private staged no-replace
/// fallback can take over. A genuine ENOENT resurfaces because the staged
/// creation then fails with the same error; unrelated failures such as
/// EACCES, EEXIST, EMFILE, or ENOSPC stay with the caller as hard errors.
#[cfg(target_os = "linux")]
fn anonymous_creation_unsupported(error: &std::io::Error) -> bool {
    // ENOTSUP is an alias of EOPNOTSUPP on Linux, so one code covers both.
    const EOPNOTSUPP: libc::c_int = libc::EOPNOTSUPP;
    const EISDIR: libc::c_int = libc::EISDIR;
    const EINVAL: libc::c_int = libc::EINVAL;
    const ENOENT: libc::c_int = libc::ENOENT;
    matches!(
        error.raw_os_error(),
        Some(EOPNOTSUPP) | Some(EISDIR) | Some(EINVAL) | Some(ENOENT)
    )
}

/// Creates, writes, and links temporary files without overwriting.
///
/// Accepts an anchored output-directory handle and complete pending artifacts.
/// Returns unit after every file is written and synced, linked with atomic
/// no-replace semantics, and the directory is synced. Anonymous O_TMPFILE
/// inodes are preferred; when the kernel or filesystem rejects anonymous
/// creation, preparation falls back to private staged names that publish with
/// the same no-replace and cleanup guarantees. Any error leaves unnamed
/// temporary inodes to automatic close cleanup and removes every staged name;
/// already linked complete files are listed in `published` and are never
/// truncated or removed.
#[cfg(target_os = "linux")]
fn publish_prepared(
    directory: &std::fs::File,
    pending: Vec<PendingArtifact>,
    published: &mut Vec<Artifact>,
) -> std::io::Result<()> {
    publish_prepared_with(directory, pending, published, open_anonymous, open_staged)
}

/// Creates, writes, and links temporary files through injected openers.
///
/// Accepts an anchored output-directory handle, complete pending artifacts,
/// the publication ledger, the anonymous-inode opener, and the staged-name
/// opener. Returns unit after every file is written and synced, linked with
/// atomic no-replace semantics, and the directory is synced. Anonymous
/// O_TMPFILE inodes are preferred; when the kernel or filesystem rejects
/// anonymous creation, preparation falls back to private staged names that
/// publish with the same no-replace and cleanup guarantees. Any error leaves
/// unnamed temporary inodes to automatic close cleanup and removes every
/// staged name through its guard; already linked complete files are listed in
/// `published` and are never truncated or removed. Injecting the openers lets
/// tests emulate unsupported or failing creation without any global state.
#[cfg(target_os = "linux")]
fn publish_prepared_with<A, S>(
    directory: &std::fs::File,
    pending: Vec<PendingArtifact>,
    published: &mut Vec<Artifact>,
    mut open_anonymous: A,
    mut open_staged: S,
) -> std::io::Result<()>
where
    A: FnMut(i32, &std::ffi::CStr) -> std::io::Result<std::fs::File>,
    S: FnMut(i32) -> std::io::Result<StagedName>,
{
    use std::os::fd::AsRawFd;
    let directory_fd = directory.as_raw_fd();
    let dot = std::ffi::CString::new(".").unwrap();
    let mut anonymous_usable = true;
    let mut temporary: Vec<(PreparedFile, Artifact, Vec<u8>)> = Vec::new();
    for item in pending {
        let prepared = if anonymous_usable {
            match open_anonymous(directory_fd, &dot) {
                Ok(mut file) => {
                    file.write_all(&item.bytes)?;
                    file.sync_all()?;
                    PreparedFile::Anonymous(file)
                }
                Err(error) => {
                    if !anonymous_creation_unsupported(&error) {
                        return Err(error);
                    }
                    anonymous_usable = false;
                    prepare_staged_with(directory_fd, &item.bytes, &mut open_staged)?
                }
            }
        } else {
            prepare_staged_with(directory_fd, &item.bytes, &mut open_staged)?
        };
        // Bytes stay resident until linking succeeds so the staged fallback can
        // rewrite them when neither descriptor link strategy is available.
        temporary.push((prepared, item.artifact, item.bytes));
    }
    publish_prepared_files(directory_fd, temporary, published)?;
    directory.sync_all()
}

/// Creates, writes, and links private staged temporary files without overwriting.
///
/// Accepts an anchored output-directory handle, complete pending artifacts, and
/// the publication ledger. Returns unit after every file is written and synced
/// into a private 0600 staged name, linked atomically with no-replace
/// semantics, its staged name removed, and the directory synced. Any failure
/// removes every remaining staged name through its RAII guard; already linked
/// complete files stay listed in `published` and are never truncated or removed.
#[cfg(target_os = "macos")]
fn publish_prepared(
    directory: &std::fs::File,
    pending: Vec<PendingArtifact>,
    published: &mut Vec<Artifact>,
) -> std::io::Result<()> {
    publish_staged_with(directory, pending, published, open_staged)
}

/// Creates, writes, and publishes private staged files through an injected opener.
///
/// Accepts an anchored output-directory handle, complete pending artifacts,
/// the publication ledger, and the staged-name opener. Returns unit after every
/// file is written and synced into a private staged name, linked atomically
/// with no-replace semantics, its staged name removed, and the directory
/// synced. Any preparation, link, or cleanup error removes every remaining
/// staged name through its RAII guard; already linked complete files stay
/// listed in `published` and are never truncated or removed. Injecting the
/// opener lets tests verify cleanup on staged preparation failure without
/// global state.
#[cfg(any(target_os = "macos", all(target_os = "linux", test)))]
fn publish_staged_with<S>(
    directory: &std::fs::File,
    pending: Vec<PendingArtifact>,
    published: &mut Vec<Artifact>,
    mut open_staged: S,
) -> std::io::Result<()>
where
    S: FnMut(i32) -> std::io::Result<StagedName>,
{
    use std::os::fd::AsRawFd;
    let directory_fd = directory.as_raw_fd();
    let mut temporary: Vec<(PreparedFile, Artifact, Vec<u8>)> = Vec::new();
    for item in pending {
        let prepared = prepare_staged_with(directory_fd, &item.bytes, &mut open_staged)?;
        temporary.push((prepared, item.artifact, Vec::new()));
    }
    publish_prepared_files(directory_fd, temporary, published)?;
    directory.sync_all()
}

/// Publishes prepared artifacts and leaves no staged name behind.
///
/// Accepts the anchored directory descriptor, prepared artifacts with their
/// complete bytes, and the publication ledger. Returns unit after each file
/// is linked under its final name with no-replace semantics; on Linux,
/// anonymous files use the strongest descriptor linking strategy and staged
/// files link directly, while on macOS all files publish through staged names.
/// Every staged name is owned by a guard that removes it on drop, so an invalid
/// filename on the popped artifact, a failed link, or an abandoned pending
/// artifact cannot leak a temporary name; artifacts linked before a failure
/// stay listed in `published`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish_prepared_files(
    directory_fd: i32,
    prepared: Vec<(PreparedFile, Artifact, Vec<u8>)>,
    published: &mut Vec<Artifact>,
) -> std::io::Result<()> {
    let mut items: std::collections::VecDeque<(PreparedFile, Artifact, Vec<u8>)> = prepared.into();
    #[cfg(target_os = "linux")]
    let mut empty_path_usable = true;
    while let Some((prepared, artifact, bytes)) = items.pop_front() {
        let file_name = artifact.path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing filename")
        })?;
        let name = std::ffi::CString::new(file_name.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename")
        })?;
        #[cfg(target_os = "linux")]
        match prepared {
            PreparedFile::Anonymous(file) => {
                publish_one(directory_fd, &file, &bytes, &name, &mut empty_path_usable)?;
            }
            PreparedFile::Staged(staged) => {
                staged.publish(&name)?;
            }
        }
        #[cfg(target_os = "macos")]
        {
            let _ = (directory_fd, bytes);
            let PreparedFile::Staged(staged) = prepared;
            staged.publish(&name)?;
        }
        published.push(artifact);
    }
    Ok(())
}

/// Reports whether an AT_EMPTY_PATH link failed only for missing privilege.
///
/// Accepts the raw linkat error. Returns true for EPERM, EACCES, and ENOENT,
/// the error signatures Linux below 6.10 uses when an ordinary user links an
/// anonymous temporary without CAP_DAC_READ_SEARCH; other errors stay with
/// the caller so real rejections such as EEXIST are never masked.
#[cfg(target_os = "linux")]
fn empty_path_denied(error: &std::io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(1 /* EPERM */) | Some(2 /* ENOENT */) | Some(13 /* EACCES */)
    )
}

/// Links one anonymous temporary file through linkat with AT_EMPTY_PATH.
///
/// Accepts the open temporary handle, anchored directory descriptor, and final
/// name. Returns unit after the descriptor's inode appears at the name. The
/// kernel below 6.10 denies this call to ordinary users; the caller falls back.
#[cfg(target_os = "linux")]
fn link_empty_path(
    file: &std::fs::File,
    directory_fd: i32,
    name: &std::ffi::CStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let empty = std::ffi::CString::new("").unwrap();
    // SAFETY: both descriptors are valid; the empty path with AT_EMPTY_PATH
    // selects the temporary descriptor; linkat never replaces an existing name.
    let result = unsafe {
        libc::linkat(
            file.as_raw_fd(),
            empty.as_ptr(),
            directory_fd,
            name.as_ptr(),
            libc::AT_EMPTY_PATH,
        )
    };
    if result != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Links one anonymous temporary file through its procfs descriptor path.
///
/// Accepts the open temporary handle, anchored directory descriptor, and final
/// name. Returns unit after the anonymous inode appears at the name. This is
/// the documented ordinary-user publication path on Linux below 6.10, where
/// AT_EMPTY_PATH linking requires CAP_DAC_READ_SEARCH; a hidden or missing
/// procfs mount returns an error for the staged fallback.
#[cfg(target_os = "linux")]
fn link_procfs(
    file: &std::fs::File,
    directory_fd: i32,
    name: &std::ffi::CStr,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let descriptor = std::ffi::CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in descriptor"))?;
    // SAFETY: the descriptor path is NUL-terminated and absolute so the old
    // directory argument is unused; AT_SYMLINK_FOLLOW resolves the procfs link
    // to the anonymous inode and linkat never replaces an existing name.
    let result = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            descriptor.as_ptr(),
            directory_fd,
            name.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if result != 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Builds one unique private staged temporary name.
///
/// Accepts no parameters. Returns a NUL-terminated hidden name mixing the
/// process ID, a monotonic counter, and the current nanosecond clock so
/// concurrent writers never share a staged name.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn staged_name() -> std::io::Result<std::ffi::CString> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    std::ffi::CString::new(format!(
        ".glassbox-tmp-{}-{}-{nanos}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ))
    .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in staged name"))
}

/// RAII owner of one private staged temporary name.
///
/// Holds the open staged file, the anchored directory descriptor, the staged
/// name, and the armed flag. Dropping an armed guard removes the staged name,
/// so no code path — an early return, a failed validation, a failed link, or
/// an abandoned pending artifact — can leak a temporary name; publication
/// disarms the guard only after the staged name is gone.
#[cfg(any(target_os = "linux", target_os = "macos"))]
struct StagedName {
    /// Open 0600 handle to the staged file.
    file: std::fs::File,
    /// Anchored descriptor of the directory holding the staged name.
    directory_fd: i32,
    /// The staged name itself.
    name: std::ffi::CString,
    /// Whether dropping the guard must still remove the staged name.
    armed: bool,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl StagedName {
    /// Writes and syncs the complete bytes into the staged file.
    ///
    /// Accepts the complete bytes. Returns unit after every byte is synced;
    /// any failure returns its error and the guard removes the staged name.
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<()> {
        self.file
            .write_all(bytes)
            .and_then(|_| self.file.sync_all())
    }

    /// Links the staged inode to the final name and removes the staged name.
    ///
    /// Accepts the final name. Returns unit after the staged inode appears
    /// exactly once under the final name with no-replace semantics and the
    /// staged name is removed. A link failure returns the error while the
    /// guard removes the staged name; an existing final name fails with
    /// AlreadyExists without changing its bytes; a failed staged-name removal
    /// after a successful link is returned as the error.
    fn publish(mut self, name: &std::ffi::CStr) -> std::io::Result<()> {
        // SAFETY: both descriptors are valid; linkat never replaces an
        // existing name, keeping no-overwrite semantics intact.
        let result = unsafe {
            libc::linkat(
                self.directory_fd,
                self.name.as_ptr(),
                self.directory_fd,
                name.as_ptr(),
                0,
            )
        };
        if result != 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: directory_fd is valid and flags 0 remove a non-directory name.
        let removed = unsafe { libc::unlinkat(self.directory_fd, self.name.as_ptr(), 0) } == 0;
        self.armed = false;
        if !removed {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Drop for StagedName {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: directory_fd stays valid for the guard's lifetime and
            // flags 0 remove a non-directory name; removal is best effort
            // because the original failure is the reported error.
            let _ = unsafe { libc::unlinkat(self.directory_fd, self.name.as_ptr(), 0) };
        }
    }
}

/// Creates and opens one private staged temporary name for writing.
///
/// Accepts the anchored directory descriptor. Returns the armed guard holding
/// the open 0600 handle and its staged name after an O_CREAT with O_EXCL and
/// O_NOFOLLOW creation, so a squatted link is never followed, concurrent
/// writers never share a staged name, and the staged name is removed whenever
/// the guard drops without publication.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn open_staged(directory_fd: i32) -> std::io::Result<StagedName> {
    use std::os::fd::FromRawFd;
    let name = staged_name()?;
    // SAFETY: directory_fd is valid; O_CREAT with O_EXCL and O_NOFOLLOW either
    // creates the staged name once with mode 0600 or fails without following a
    // squatted link. Passing mode as c_uint satisfies Darwin ARM64 variadic ABI.
    let descriptor = unsafe {
        libc::openat(
            directory_fd,
            name.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600 as libc::c_uint,
        )
    };
    if descriptor < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: this guard owns the newly returned unique descriptor.
    Ok(StagedName {
        file: unsafe { std::fs::File::from_raw_fd(descriptor) },
        directory_fd,
        name,
        armed: true,
    })
}

/// Prepares one private staged name and writes the complete bytes.
///
/// Accepts the anchored directory descriptor, the complete bytes, and the
/// staged-name opener. Returns the armed staged guard after the bytes are
/// written and synced; any failure removes the staged name through the guard.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn prepare_staged_with<G>(
    directory_fd: i32,
    bytes: &[u8],
    mut open_staged: G,
) -> std::io::Result<PreparedFile>
where
    G: FnMut(i32) -> std::io::Result<StagedName>,
{
    let mut staged = open_staged(directory_fd)?;
    staged.write(bytes)?;
    Ok(PreparedFile::Staged(staged))
}

/// Publishes complete bytes through a private staged temporary name.
///
/// Accepts the anchored directory descriptor, final name, and complete bytes.
/// Returns unit after the bytes are written and synced into one 0600 no-follow
/// creation-checked temporary name, linked to the final name atomically with
/// no-replace semantics, and the staged name is removed on every path. A write,
/// link, or cleanup failure is returned; an existing final name fails with
/// AlreadyExists without changing its bytes.
#[cfg(any(target_os = "linux", all(target_os = "macos", test)))]
fn link_staging(directory_fd: i32, name: &std::ffi::CStr, bytes: &[u8]) -> std::io::Result<()> {
    let mut staged = open_staged(directory_fd)?;
    staged.write(bytes)?;
    staged.publish(name)
}

/// Links one prepared artifact through the strongest available strategy.
///
/// Accepts the anchored directory descriptor, anonymous temporary handle,
/// complete bytes, final name, and the shared AT_EMPTY_PATH usability flag.
/// Returns unit after the artifact appears exactly once under the final name.
/// AT_EMPTY_PATH is preferred and cached; when the kernel denies it to ordinary
/// users, the procfs descriptor path is tried, and the staged no-replace
/// fallback covers hosts without procfs. No strategy ever replaces a name.
#[cfg(target_os = "linux")]
fn publish_one(
    directory_fd: i32,
    file: &std::fs::File,
    bytes: &[u8],
    name: &std::ffi::CStr,
    empty_path_usable: &mut bool,
) -> std::io::Result<()> {
    publish_one_with_procfs(
        directory_fd,
        file,
        bytes,
        name,
        empty_path_usable,
        link_procfs,
        link_staging,
    )
}

/// Links one prepared artifact through injected procfs and staging linkers.
///
/// Accepts the anchored directory descriptor, anonymous temporary handle,
/// complete bytes, final name, shared `AT_EMPTY_PATH` usability flag, a procfs
/// linker closure, and a staging fallback linker closure. Returns unit after
/// the artifact appears under the final name. When `empty_path_usable` is true,
/// `AT_EMPTY_PATH` is tried first; otherwise or after missing-privilege denial,
/// `procfs_link` runs. `AlreadyExists` from `procfs_link` short-circuits
/// immediately without invoking `staging_link`; any other `procfs_link` error
/// falls back to `staging_link` and preserves both error causes if staging also
/// fails.
#[cfg(target_os = "linux")]
fn publish_one_with_procfs<F, G>(
    directory_fd: i32,
    file: &std::fs::File,
    bytes: &[u8],
    name: &std::ffi::CStr,
    empty_path_usable: &mut bool,
    mut procfs_link: F,
    mut staging_link: G,
) -> std::io::Result<()>
where
    F: FnMut(&std::fs::File, i32, &std::ffi::CStr) -> std::io::Result<()>,
    G: FnMut(i32, &std::ffi::CStr, &[u8]) -> std::io::Result<()>,
{
    if *empty_path_usable {
        match link_empty_path(file, directory_fd, name) {
            Ok(()) => return Ok(()),
            Err(error) if empty_path_denied(&error) => *empty_path_usable = false,
            Err(error) => return Err(error),
        }
    }
    match procfs_link(file, directory_fd, name) {
        Ok(()) => Ok(()),
        Err(procfs) if procfs.kind() == std::io::ErrorKind::AlreadyExists => Err(procfs),
        Err(procfs) => staging_link(directory_fd, name, bytes).map_err(|staged| {
            std::io::Error::new(
                staged.kind(),
                format!("descriptor link unavailable (procfs: {procfs}; staged: {staged})"),
            )
        }),
    }
}

/// Publishes complete v3 artifacts into one private no-follow output directory.
///
/// Accepts a destination directory and fully serialized artifact payloads.
/// Returns published typed paths on success after walking from `/` through
/// anchored no-follow descriptors via [`open_anchored_directory`]. Directory,
/// link, serialization, write, collision, and sync failures return
/// OutputFailure; concurrent or existing names are never replaced and no named
/// temporary files are leaked.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn publish(
    directory: &Path,
    pending: Vec<PendingArtifact>,
) -> Result<Vec<Artifact>, OutputFailure> {
    let directory = absolute_lexical(directory)
        .map_err(|error| failure("resolve output directory", vec![], error))?;
    let handle = open_anchored_directory(&directory, true)
        .map_err(|error| failure("prepare output directory", vec![], error))?;
    let pending = pending
        .into_iter()
        .map(|mut item| {
            item.artifact.path = directory.join(item.artifact.path.file_name().unwrap());
            item
        })
        .collect();
    let mut published = Vec::new();
    publish_prepared(&handle, pending, &mut published)
        .map_err(|error| failure("publish artifact", published.clone(), error))?;
    Ok(published)
}

/// Reports unsupported private artifact publication on unsupported hosts without writes.
///
/// Accepts a directory and pending artifacts. Returns an operational failure;
/// no destination is created, truncated, or followed. Native Linux and macOS
/// publication are the verified implementations.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn publish(
    _directory: &Path,
    _pending: Vec<PendingArtifact>,
) -> Result<Vec<Artifact>, OutputFailure> {
    Err(OutputFailure {
        message: "safe private artifact publication is currently supported on Linux and macOS only"
            .into(),
        published: vec![],
    })
}

/// Validates a single normal filename component and converts it to a C string.
///
/// Accepts a candidate filename as an [`std::ffi::OsStr`]. Returns a
/// NUL-terminated [`std::ffi::CString`] guaranteed to represent exactly one
/// normal path component without path separators (`/`, `\`) or interior NUL
/// bytes. Empty names, `.`, `..`, root or prefix paths, multi-component paths,
/// and NUL-containing names return `InvalidInput`.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn validate_filename_cstr(name: &std::ffi::OsStr) -> std::io::Result<std::ffi::CString> {
    let raw = name.as_bytes();
    if raw.is_empty() || raw.contains(&b'/') || raw.contains(&b'\\') {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output filename must be a single non-empty normal component",
        ));
    }
    let mut components = Path::new(name).components();
    match (components.next(), components.next()) {
        (Some(Component::Normal(_)), None) => {}
        _ => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "output filename must be a single normal component",
            ));
        }
    }
    std::ffi::CString::new(raw)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename"))
}

/// Publishes one complete file atomically to an explicit destination path without overwriting.
///
/// Accepts a user-supplied output file path and the complete payload bytes.
/// Returns the absolute lexically normalized destination path after walking
/// from `/` through anchored no-follow descriptors (creating any missing
/// parent directories with mode `0700` while permitting existing ordinary
/// `0755` parent directories), verifying target absence via anchored
/// `fstatat(AT_SYMLINK_NOFOLLOW)` before staging, writing and syncing the
/// bytes into a private `0600` staged file inside the anchored parent
/// descriptor, linking it atomically with no-overwrite semantics, unlinking
/// the staged temporary name, and syncing the parent directory. Empty paths,
/// root or directory-like paths, non-normal filenames, symbolic-link parent or
/// target components, non-directory parents, existing targets (`AlreadyExists`),
/// and I/O failures return an error without modifying existing files or
/// creating/leaking temporary names.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) fn publish_single_file(path: &Path, bytes: &[u8]) -> std::io::Result<PathBuf> {
    use std::os::fd::AsRawFd;

    let raw = path.as_os_str().as_bytes();
    if raw.is_empty()
        || raw == b"."
        || raw == b".."
        || raw.ends_with(b"/")
        || raw.ends_with(b"\\")
        || raw.ends_with(b"/.")
        || raw.ends_with(b"/..")
        || raw.ends_with(b"\\.")
        || raw.ends_with(b"\\..")
        || !matches!(path.components().next_back(), Some(Component::Normal(_)))
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output file path must end with a normal filename",
        ));
    }

    let normalized = absolute_lexical(path)?;
    let parent = normalized
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "output file path must have a parent directory",
            )
        })?;
    let file_name = normalized.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "output file path is missing a filename",
        )
    })?;
    let target_name = validate_filename_cstr(file_name)?;

    let parent_handle = open_anchored_directory(parent, false)?;
    ensure_target_absent(parent_handle.as_raw_fd(), &target_name, &normalized)?;

    let mut staged = open_staged(parent_handle.as_raw_fd())?;
    staged.write(bytes)?;
    staged.publish(&target_name)?;
    parent_handle.sync_all()?;
    Ok(normalized)
}

/// Reports unsupported single-file private publication on unsupported hosts without writes.
///
/// Accepts a destination file path and payload bytes. Returns an `Unsupported`
/// I/O error without creating, truncating, or following any filesystem path.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) fn publish_single_file(_path: &Path, _bytes: &[u8]) -> std::io::Result<PathBuf> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "safe private artifact publication is currently supported on Linux and macOS only",
    ))
}

/// RAII streaming writer for private no-overwrite artifact publication inside `--output-dir`.
///
/// Holds an armed `0600` staged temporary file anchored to an owned `0700`
/// output-directory descriptor so callers can stream large payloads in O(1)
/// memory and seek to patch headers (such as RIFF/WAV lengths) before atomic
/// publication. Dropping a `StagedWriter` without calling [`Self::publish`]
/// unlinks the staged temporary name before closing the directory handle.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) struct StagedWriter {
    /// Armed staged temporary file; MUST be declared before `directory` so
    /// Rust's declaration-order drop unlinks through the still-open descriptor.
    staged: StagedName,
    /// Anchored parent directory handle kept open for the lifetime of `staged`.
    directory: std::fs::File,
    /// Validated NUL-terminated final filename inside `directory`.
    target_name: std::ffi::CString,
    /// Absolute destination path returned on successful publication.
    final_path: PathBuf,
}

/// Stub streaming writer for unsupported hosts that fails closed without writes.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) struct StagedWriter {
    _private: (),
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl StagedWriter {
    /// Validates a private output directory via anchored walk and opens one armed `0600` staged file.
    ///
    /// Accepts the user-supplied private output directory (`--output-dir`) and
    /// a single normal target filename. Returns an open [`StagedWriter`] ready
    /// for streaming writes and seeks after walking from `/` through anchored
    /// no-follow descriptors (enforcing `mode & 0o077 == 0` via `fstat` on the
    /// anchored descriptor) and verifying target absence via anchored
    /// `fstatat(AT_SYMLINK_NOFOLLOW)` before staging. Invalid filenames,
    /// symbolic-link or non-directory components, symbolic-link targets
    /// (`InvalidInput`), pre-existing targets (`AlreadyExists`),
    /// group/other-accessible existing output directories (`PermissionDenied`),
    /// and OS creation errors are returned without publishing any target name
    /// or creating/leaking temporary files.
    pub(crate) fn create(output_dir: &Path, filename: &str) -> std::io::Result<Self> {
        use std::os::fd::AsRawFd;

        let target_name = validate_filename_cstr(std::ffi::OsStr::new(filename))?;
        let directory_path = absolute_lexical(output_dir)?;
        let directory = open_anchored_directory(&directory_path, true)?;
        let final_path = directory_path.join(filename);
        ensure_target_absent(directory.as_raw_fd(), &target_name, &final_path)?;
        let staged = open_staged(directory.as_raw_fd())?;
        Ok(Self {
            staged,
            directory,
            target_name,
            final_path,
        })
    }

    /// Flushes, syncs, and atomically publishes the staged file under its final name.
    ///
    /// Accepts ownership of `self`. Returns the absolute published [`PathBuf`]
    /// after flushing and syncing the staged inode, linking it atomically with
    /// no-overwrite semantics, unlinking the temporary name, and syncing the
    /// parent directory. Collisions (`AlreadyExists`), sync failures, or link
    /// failures return an error while automatically removing the staged
    /// temporary file and leaving any existing target untouched.
    pub(crate) fn publish(mut self) -> std::io::Result<PathBuf> {
        self.staged.file.flush()?;
        self.staged.file.sync_all()?;
        let Self {
            staged,
            directory,
            target_name,
            final_path,
        } = self;
        staged.publish(&target_name)?;
        directory.sync_all()?;
        Ok(final_path)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl StagedWriter {
    /// Fails closed on unsupported hosts without creating directories or temporary files.
    ///
    /// Accepts an output directory and target filename. Always returns an
    /// `Unsupported` I/O error.
    pub(crate) fn create(_output_dir: &Path, _filename: &str) -> std::io::Result<Self> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe private artifact publication is currently supported on Linux and macOS only",
        ))
    }

    /// Fails closed on unsupported hosts without touching the filesystem.
    ///
    /// Accepts ownership of `self`. Always returns an `Unsupported` I/O error.
    pub(crate) fn publish(self) -> std::io::Result<PathBuf> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe private artifact publication is currently supported on Linux and macOS only",
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Write for StagedWriter {
    /// Writes a chunk of bytes into the underlying `0600` staged file.
    ///
    /// Accepts a byte slice to write at the current file offset. Returns the
    /// number of bytes written or propagates the underlying OS write error.
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.staged.file.write(buf)
    }

    /// Flushes any buffered data on the underlying staged file handle.
    ///
    /// Accepts no parameters. Returns unit on success or propagates OS flush errors.
    fn flush(&mut self) -> std::io::Result<()> {
        self.staged.file.flush()
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Write for StagedWriter {
    /// Fails closed on unsupported hosts without writing bytes.
    ///
    /// Accepts a byte slice. Always returns an `Unsupported` I/O error.
    fn write(&mut self, _buf: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe private artifact publication is currently supported on Linux and macOS only",
        ))
    }

    /// Fails closed on unsupported hosts without flushing.
    ///
    /// Accepts no parameters. Always returns an `Unsupported` I/O error.
    fn flush(&mut self) -> std::io::Result<()> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe private artifact publication is currently supported on Linux and macOS only",
        ))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
impl Seek for StagedWriter {
    /// Repositions the write cursor within the staged file (e.g., to patch WAV headers).
    ///
    /// Accepts a [`SeekFrom`] offset. Returns the new byte offset from the
    /// start of the staged file or propagates the underlying OS seek error.
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.staged.file.seek(pos)
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
impl Seek for StagedWriter {
    /// Fails closed on unsupported hosts without seeking.
    ///
    /// Accepts a [`SeekFrom`] offset. Always returns an `Unsupported` I/O error.
    fn seek(&mut self, _pos: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "safe private artifact publication is currently supported on Linux and macOS only",
        ))
    }
}

/// Maps arbitrary filename text into safe ASCII component characters capped at 48 chars.
///
/// Accepts raw filename or stem text. Returns an ASCII string containing only
/// alphanumerics, `.`, `_`, or `-`, truncated to at most 48 characters.
/// Performs no I/O and cannot fail.
fn sanitize_ascii_component(raw: &str) -> String {
    raw.chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect()
}

/// Sanitizes a source file stem for deterministic media artifact filenames.
///
/// Accepts a source file path. Returns a non-empty ASCII stem of at most 48
/// characters containing only alphanumerics, `.`, `_`, or `-`, with all
/// leading `.` characters stripped so generated names can never be hidden
/// entries, `.`/`..`, or path-traversal segments; falls back to `"media"` when
/// no usable stem remains. Performs no I/O and cannot fail.
fn sanitized_media_stem(source: &Path) -> String {
    let raw = source
        .file_stem()
        .map(|stem| stem.to_string_lossy())
        .unwrap_or_default();
    let stem: String = raw
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .skip_while(|&character| character == '.')
        .take(48)
        .collect();
    if stem.is_empty() {
        "media".to_owned()
    } else {
        stem
    }
}

/// Sanitizes a media artifact extension to lowercase ASCII alphanumerics.
///
/// Accepts a caller-supplied extension (with or without leading `.`) and a
/// safe ASCII fallback extension. Returns a non-empty lowercase ASCII
/// alphanumeric extension of at most 16 characters, or `fallback` when no
/// ASCII alphanumeric characters remain. Performs no I/O and cannot fail.
fn sanitized_extension(extension: &str, fallback: &str) -> String {
    let cleaned: String = extension
        .trim_start_matches('.')
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|character| character.to_ascii_lowercase())
        .take(16)
        .collect();
    if cleaned.is_empty() {
        fallback.to_owned()
    } else {
        cleaned
    }
}

/// Builds the deterministic `--output-dir` filename for a video contact sheet.
///
/// Accepts the source media path and the desired image extension (e.g., `"jpg"`
/// or `"png"`). Returns `<sanitized-stem>-contact-sheet.<extension>` containing
/// no path separators, NUL bytes, or leading dots. Performs no I/O and cannot
/// fail.
pub(crate) fn contact_sheet_filename(source: &Path, extension: &str) -> String {
    format!(
        "{}-contact-sheet.{}",
        sanitized_media_stem(source),
        sanitized_extension(extension, "jpg")
    )
}

/// Builds the deterministic `--output-dir` filename for an extracted audio file.
///
/// Accepts the source media path and the desired audio extension (e.g., `"wav"`,
/// `"flac"`, `"m4a"`, or `"opus"`). Returns `<sanitized-stem>-extract.<extension>`
/// containing no path separators, NUL bytes, or leading dots. Performs no I/O
/// and cannot fail.
pub(crate) fn audio_extract_filename(source: &Path, extension: &str) -> String {
    format!(
        "{}-extract.{}",
        sanitized_media_stem(source),
        sanitized_extension(extension, "wav")
    )
}

/// Sanitizes a source basename and adds sequence ordering.
///
/// Accepts source identity and a deterministic per-run sequence. Returns an
/// ASCII basename at most 59 characters before identity and extension. The
/// report retains the original source path unchanged.
fn artifact_stem(identity: &glassbox_inspect::reports::SourceIdentity, sequence: usize) -> String {
    let source = identity
        .path
        .file_name()
        .map(|name| name.to_string_lossy())
        .unwrap_or_else(|| "document".into());
    let mut basename = sanitize_ascii_component(&source);
    if basename.is_empty() || basename.chars().all(|character| character == '.') {
        basename = "document".into();
    }
    format!("{basename}-{sequence:06}")
}

/// Selects a trusted digest prefix, sampled fingerprint stem, or `unhashed` fallback.
///
/// Accepts a [`DigestReport`]. Returns the first twelve characters of the full
/// SHA-256 hex digest when consistent and non-empty, `fp-<first 12 chars>` when
/// consistent with a sampled fingerprint, or `"unhashed"` on hash failure,
/// missing digest/fingerprint, or concurrent source mutation. Performs no I/O
/// and cannot panic.
fn digest_prefix(digest: &DigestReport) -> String {
    if digest.consistency != glassbox_inspect::reports::ConsistencyStatus::Consistent {
        return "unhashed".to_owned();
    }
    if let Some(hex) = digest.digest.as_deref().filter(|hex| !hex.is_empty()) {
        return hex.chars().take(12).collect();
    }
    if let Some(fingerprint) = digest
        .fingerprint
        .as_ref()
        .filter(|fp| !fp.value.is_empty())
    {
        let prefix: String = fingerprint.value.chars().take(12).collect();
        return format!("fp-{prefix}");
    }
    "unhashed".to_owned()
}

/// Adds artifact outcomes and converts publication errors into failed reports.
///
/// Accepts mutable shared metadata, the artifact insertion point, publication
/// result, and operation-specific error code. Returns unit after retaining only
/// actually published paths. Existing artifacts are preserved; failures add a
/// structured error and set failed/partial metadata without hiding prior facts.
fn finish_publication(
    metadata: &mut DocumentMetadata,
    inserted_at: usize,
    result: Result<Vec<Artifact>, OutputFailure>,
    code: &str,
) {
    match result {
        Ok(published) => {
            metadata.artifacts.truncate(inserted_at);
            metadata.artifacts.extend(published);
        }
        Err(error) => {
            metadata.artifacts.truncate(inserted_at);
            metadata.artifacts.extend(error.published);
            metadata.diagnostics.push(Diagnostic {
                stage: DiagnosticStage::Artifact,
                severity: DiagnosticSeverity::Error,
                code: code.to_owned(),
                message: error.message,
                unit_index: None,
            });
            metadata.status = OperationStatus::Failed;
            metadata.completeness = Completeness::Partial;
        }
    }
}

/// Persists a content-free v3 summary and identical v3 metadata JSON report.
///
/// Accepts a mutable metadata report, output directory, deterministic item
/// sequence, and `verbose` flag (controlling whether reader provenance is
/// included in the saved `.json` and `.md` artifacts). Returns unit after
/// artifact publication and typed report updates. Collisions, unsafe
/// directories, encoding, and I/O failures become artifact diagnostics and
/// failed status; no existing file is overwritten.
pub(crate) fn persist_metadata(
    report: &mut MetadataReport,
    directory: &Path,
    sequence: usize,
    verbose: bool,
) {
    let stem = artifact_stem(&report.metadata.identity, sequence);
    let stem = format!("{}-{}", stem, digest_prefix(&report.metadata.digest));
    let directory = absolute_lexical(directory).unwrap_or_else(|_| directory.to_path_buf());
    let summary_path = directory.join(format!("{stem}.md"));
    let json_path = directory.join(format!("{stem}.json"));
    let inserted_at = report.metadata.artifacts.len();
    let summary_artifact = Artifact {
        kind: ArtifactKind::MetadataSummary,
        path: summary_path,
        source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: None,
        height: None,
    };
    let json_artifact = Artifact {
        kind: ArtifactKind::MetadataJson,
        path: json_path,
        source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: None,
        height: None,
    };
    report
        .metadata
        .artifacts
        .extend([summary_artifact.clone(), json_artifact.clone()]);
    let v3_report = report.to_v3(verbose);
    let result = serde_json::to_vec(&v3_report)
        .map_err(|error| failure("serialize metadata report", vec![], error))
        .and_then(|json| {
            publish(
                &directory,
                vec![
                    PendingArtifact {
                        artifact: summary_artifact,
                        bytes: glassbox_inspect::metadata::summary_markdown(report, verbose)
                            .into_bytes(),
                    },
                    PendingArtifact {
                        artifact: json_artifact,
                        bytes: json,
                    },
                ],
            )
        });
    finish_publication(
        &mut report.metadata,
        inserted_at,
        result,
        "metadata_output_failed",
    );
}

/// Adds extraction artifact outcomes and records publication errors on the
/// top-level extraction report without mutating the nested inspection metadata.
///
/// Accepts a mutable [`ExtractReport`], the artifact insertion point,
/// publication result, and operation-specific error code. Returns unit after
/// retaining only actually published paths. On failure, records the artifact
/// error in `report.diagnostics`, sets `report.status = OperationStatus::Failed`,
/// and degrades `report.completeness` from `Exact` to `Partial` while preserving
/// `Unavailable` and keeping `report.metadata.status` and `report.metadata.diagnostics`
/// untouched.
fn finish_extract_publication(
    report: &mut ExtractReport,
    inserted_at: usize,
    result: Result<Vec<Artifact>, OutputFailure>,
    code: &str,
) {
    match result {
        Ok(published) => {
            report.metadata.artifacts.truncate(inserted_at);
            report.metadata.artifacts.extend(published);
        }
        Err(error) => {
            report.metadata.artifacts.truncate(inserted_at);
            report.metadata.artifacts.extend(error.published);
            report.diagnostics.push(Diagnostic {
                stage: DiagnosticStage::Artifact,
                severity: DiagnosticSeverity::Error,
                code: code.to_owned(),
                message: error.message,
                unit_index: None,
            });
            report.status = OperationStatus::Failed;
            if report.completeness != Completeness::Unavailable {
                report.completeness = Completeness::Partial;
            }
        }
    }
}

/// Persists selected Markdown content and identical canonical v3 extraction JSON.
///
/// Accepts a mutable extraction report, output directory, deterministic item
/// sequence, `verbose` flag (controlling provenance in the saved `.json`),
/// `include_markers` (`true` to emit `bodymatter:v1` structure comments in the
/// saved `.md`, `false` for `--no-markers`), and `include_frontmatter` (`true`
/// to emit `schema: "glassbox:v1"` YAML frontmatter on ASR transcript `.md`
/// artifacts, `false` for `--no-frontmatter`). Returns the exact rendered
/// Markdown `String` written to the `.md` artifact so callers combining
/// `--output-dir` with `--format markdown` can print the identical bytes
/// (including the same generated `id` ULID and `updated_at` timestamp).
/// Collisions, unsafe directories, serialization, and I/O failures become
/// top-level extraction artifact errors without mutating the nested inspection
/// status; existing paths and prior successful artifacts are never truncated.
pub(crate) fn persist_extract(
    report: &mut ExtractReport,
    directory: &Path,
    sequence: usize,
    verbose: bool,
    include_markers: bool,
    include_frontmatter: bool,
) -> String {
    let stem = artifact_stem(&report.metadata.identity, sequence);
    let stem = format!("{}-{}", stem, digest_prefix(&report.metadata.digest));
    let directory = absolute_lexical(directory).unwrap_or_else(|_| directory.to_path_buf());
    let markdown_path = directory.join(format!("{stem}.md"));
    let json_path = directory.join(format!("{stem}.json"));
    let inserted_at = report.metadata.artifacts.len();
    let markdown_artifact = Artifact {
        kind: ArtifactKind::MarkdownContent,
        path: markdown_path,
        source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: None,
        height: None,
    };
    let json_artifact = Artifact {
        kind: ArtifactKind::ExtractionJson,
        path: json_path,
        source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
        completeness: Completeness::Exact,
        width: None,
        height: None,
    };
    report
        .metadata
        .artifacts
        .extend([markdown_artifact.clone(), json_artifact.clone()]);
    let rendered_markdown = glassbox_inspect::selection::render_markdown_full(
        report,
        include_markers,
        include_frontmatter,
    );
    let v3_report = report.to_v3(verbose);
    let result = serde_json::to_vec(&v3_report)
        .map_err(|error| failure("serialize extraction report", vec![], error))
        .and_then(|json| {
            publish(
                &directory,
                vec![
                    PendingArtifact {
                        artifact: markdown_artifact,
                        bytes: rendered_markdown.as_bytes().to_vec(),
                    },
                    PendingArtifact {
                        artifact: json_artifact,
                        bytes: json,
                    },
                ],
            )
        });
    finish_extract_publication(report, inserted_at, result, "extraction_output_failed");
    rendered_markdown
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Checks digest_prefix selects full SHA-256, `fp-` sampled fingerprint, or `unhashed`.
    ///
    /// Accepts no parameters. Returns unit after checking consistent full-hash,
    /// consistent sampled fingerprint, source-changed mutation, and missing
    /// digest/fingerprint states. Performs no I/O.
    #[test]
    fn digest_prefix_distinguishes_sha256_sampled_fingerprint_and_unhashed() {
        use glassbox_inspect::reports::{ConsistencyStatus, SampledFingerprint};
        let full = DigestReport {
            digest: Some("0123456789abcdef0123456789abcdef".to_owned()),
            consistency: ConsistencyStatus::Consistent,
            ..DigestReport::default()
        };
        assert_eq!(digest_prefix(&full), "0123456789ab");

        let sampled = DigestReport {
            digest: None,
            fingerprint: Some(SampledFingerprint {
                scheme: "glassbox-sampled-sha256/v1".to_owned(),
                value: "fedcba9876543210fedcba9876543210".to_owned(),
            }),
            consistency: ConsistencyStatus::Consistent,
            ..DigestReport::default()
        };
        assert_eq!(digest_prefix(&sampled), "fp-fedcba987654");

        let changed = DigestReport {
            digest: Some("0123456789abcdef".to_owned()),
            consistency: ConsistencyStatus::SourceChanged,
            ..DigestReport::default()
        };
        assert_eq!(digest_prefix(&changed), "unhashed");

        let empty = DigestReport {
            consistency: ConsistencyStatus::Consistent,
            ..DigestReport::default()
        };
        assert_eq!(digest_prefix(&empty), "unhashed");
    }

    /// Checks that concurrent creators converge on one private real directory.
    #[test]
    fn prepare_directory_tolerates_concurrent_creators() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("race-{}-{stamp}", std::process::id()));
        let results = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| prepare_directory(&directory)))
                .collect();
            handles
                .into_iter()
                .map(|handle| handle.join().expect("no creator panicked"))
                .collect::<Vec<_>>()
        });
        assert!(
            results.iter().all(|result| result.is_ok()),
            "at least one creator failed: {results:?}"
        );
        let metadata =
            std::fs::symlink_metadata(&directory).expect("exactly one private directory exists");
        assert!(metadata.is_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_eq!(metadata.mode() & 0o777, 0o700);
        }
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks that a lost creation race revalidates only a real directory.
    #[test]
    fn create_private_directory_tolerates_won_race_and_rejects_squatters() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let parent = root.join(format!("lost-race-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&parent).expect("test scratch parent created");
        let won = parent.join("won");
        std::fs::DirBuilder::new()
            .create(&won)
            .expect("concurrent creator's real directory exists");
        create_private_directory(&won).expect("lost race converges on the real directory");
        let link = parent.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&won, &link).expect("squatted link created");
        #[cfg(unix)]
        {
            let error = create_private_directory(&link).expect_err("squatted link rejected");
            assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        }
        let file = parent.join("file");
        std::fs::write(&file, b"file squatted into the race window")
            .expect("squatted file created");
        let error = create_private_directory(&file).expect_err("squatted file rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Checks that a squatted symlink or file component is never accepted.
    #[test]
    fn prepare_directory_rejects_non_directory_components() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let parent = root.join(format!("reject-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&parent).expect("test scratch parent created");
        let squat = parent.join("squat");
        std::fs::write(&squat, b"file squatting on the output component")
            .expect("squatting file created");
        let error = prepare_directory(&squat.join("nested")).expect_err("file rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        let link = parent.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&squat, &link).expect("squatting link created");
        #[cfg(unix)]
        {
            let error = prepare_directory(&link.join("nested")).expect_err("link rejected");
            assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        }
        let _ = std::fs::remove_dir_all(&parent);
    }

    /// Checks end-to-end publication keeps private modes and no-overwrite.
    ///
    /// Accepts no parameters. Returns unit after one publication and one exact
    /// retry. Wrong bytes, public modes, replaced names, or leftover temporary
    /// names panic; the scratch tree is removed after the assertions.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publish_is_private_no_overwrite_and_leak_free() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("publish-{}-{stamp}", std::process::id()));
        let summary = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: directory.join("one.md"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let json = Artifact {
            kind: ArtifactKind::MetadataJson,
            path: directory.join("one.json"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let published = match publish(
            &directory,
            vec![
                PendingArtifact {
                    artifact: summary.clone(),
                    bytes: b"summary".to_vec(),
                },
                PendingArtifact {
                    artifact: json,
                    bytes: b"{}".to_vec(),
                },
            ],
        ) {
            Ok(published) => published,
            Err(_) => panic!("first publication succeeds"),
        };
        assert_eq!(published.len(), 2);
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("summary published"),
            b"summary"
        );
        assert_eq!(
            std::fs::read(directory.join("one.json")).expect("report published"),
            b"{}"
        );
        let names: Vec<String> = std::fs::read_dir(&directory)
            .expect("output directory readable")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert_eq!(names.len(), 2, "no temporary names remain: {names:?}");
        for name in ["one.md", "one.json"] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(directory.join(name))
                    .expect("published file exists")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let collision = match publish(
            &directory,
            vec![PendingArtifact {
                artifact: summary,
                bytes: b"changed".to_vec(),
            }],
        ) {
            Ok(_) => panic!("an existing name must never be replaced"),
            Err(failure) => failure,
        };
        assert!(collision.published.is_empty());
        assert!(
            collision.message.contains("File exists"),
            "{}",
            collision.message
        );
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("original retained"),
            b"summary"
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 2);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks whether a `link_procfs` error indicates a sandbox restricting `/proc/self/fd` linking.
    ///
    /// Accepts a raw `std::io::Error` returned by `link_procfs`. Returns `true`
    /// for `NotFound`, `PermissionDenied`, or `Unsupported` (e.g., restricted
    /// mount namespaces or seccomp sandboxes without `/proc/self/fd` linking);
    /// returns `false` for all other errors. Performs no I/O and cannot panic.
    #[cfg(target_os = "linux")]
    fn procfs_link_restricted(error: &std::io::Error) -> bool {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound
                | std::io::ErrorKind::PermissionDenied
                | std::io::ErrorKind::Unsupported
        )
    }

    /// Checks the procfs fallback links anonymous files privately, or falls back to staging cleanly.
    ///
    /// Accepts no parameters. Returns unit after linking one anonymous
    /// temporary through `/proc/self/fd` on bare-metal Linux, or verifying that
    /// `publish_one` / `publish_prepared` cleanly falls back to `link_staging`
    /// when a restricted sandbox blocks `/proc/self/fd` linking or `O_TMPFILE`.
    /// Unexpected errors, changed bytes, leftover temp names, or a non-0600
    /// mode panic.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_fallback_links_anonymous_files_privately() {
        use std::os::fd::AsRawFd;
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("procfs-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        let dot = std::ffi::CString::new(".").unwrap();
        let name = std::ffi::CString::new("one.md").unwrap();
        match open_anonymous(handle.as_raw_fd(), &dot) {
            Ok(mut temporary) => {
                temporary.write_all(b"fallback").expect("bytes written");
                temporary.sync_all().expect("bytes synced");
                match link_procfs(&temporary, handle.as_raw_fd(), &name) {
                    Ok(()) => {}
                    Err(error) if procfs_link_restricted(&error) => {
                        let mut empty_path_usable = false;
                        publish_one(
                            handle.as_raw_fd(),
                            &temporary,
                            b"fallback",
                            &name,
                            &mut empty_path_usable,
                        )
                        .expect("publish_one falls back to link_staging when procfs is restricted");
                    }
                    Err(error) => panic!("unexpected procfs link failure: {error}"),
                }
                drop(temporary);
            }
            Err(error) if anonymous_creation_unsupported(&error) => {
                let mut published = Vec::new();
                publish_prepared(
                    &handle,
                    vec![PendingArtifact {
                        artifact: Artifact {
                            kind: ArtifactKind::MetadataSummary,
                            path: directory.join("one.md"),
                            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
                            completeness: Completeness::Exact,
                            width: None,
                            height: None,
                        },
                        bytes: b"fallback".to_vec(),
                    }],
                    &mut published,
                )
                .expect("publish_prepared falls back to staged publication");
                assert_eq!(published.len(), 1);
            }
            Err(error) => panic!("unexpected anonymous temporary failure: {error}"),
        }
        drop(handle);
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("linked file readable"),
            b"fallback"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.join("one.md"))
                .expect("linked file exists")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "only the final name remains"
        );
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks the staged fallback stays atomic, private, and no-overwrite.
    ///
    /// Accepts no parameters. Returns unit after one staged publication and one
    /// colliding retry. Replaced bytes, leaked staged names, or a missing
    /// AlreadyExists rejection panic; no anonymous inode is involved.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn staging_fallback_is_private_no_overwrite_and_clean() {
        use std::os::fd::AsRawFd;
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("staging-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        let name = std::ffi::CString::new("two.md").unwrap();
        link_staging(handle.as_raw_fd(), &name, b"staged").expect("staged link succeeds");
        assert_eq!(
            std::fs::read(directory.join("two.md")).expect("staged file readable"),
            b"staged"
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(directory.join("two.md"))
                .expect("staged file exists")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "only the final name remains"
        );
        let collision = link_staging(handle.as_raw_fd(), &name, b"changed")
            .expect_err("an existing name is never replaced");
        assert_eq!(collision.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(directory.join("two.md")).expect("original retained"),
            b"staged"
        );
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks only unsupported anonymous-creation errors select the fallback.
    ///
    /// Accepts no parameters. Returns unit after classifying the unsupported
    /// EOPNOTSUPP, EISDIR, EINVAL, and old-kernel ENOENT codes next to
    /// unrelated failures and an error without a raw code. A mis-classified
    /// code panics; no filesystem is touched.
    #[cfg(target_os = "linux")]
    #[test]
    fn unsupported_anonymous_errors_are_classified_for_fallback() {
        for code in [libc::ENOENT, libc::EISDIR, libc::EINVAL, libc::EOPNOTSUPP] {
            assert!(
                anonymous_creation_unsupported(&std::io::Error::from_raw_os_error(code)),
                "code {code} must select the staged fallback"
            );
        }
        for code in [libc::EACCES, libc::EEXIST, libc::EMFILE, libc::ENOSPC] {
            assert!(
                !anonymous_creation_unsupported(&std::io::Error::from_raw_os_error(code)),
                "code {code} must stay a hard publication error"
            );
        }
        assert!(!anonymous_creation_unsupported(&std::io::Error::other(
            "no raw error code"
        )));
    }

    /// Checks the unsupported O_TMPFILE path still publishes no-replace output.
    ///
    /// Accepts no parameters. Returns unit after injecting an anonymous opener
    /// that fails with EOPNOTSUPP, publishing two complete artifacts plus one
    /// colliding retry. Missing bytes, public modes, replaced names, leftover
    /// staged names, or a missing AlreadyExists rejection panic; the scratch
    /// tree is removed after the assertions.
    #[cfg(target_os = "linux")]
    #[test]
    fn forced_staged_fallback_publishes_and_preserves_no_replace() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("forced-staged-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        let summary = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: directory.join("one.md"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let json = Artifact {
            kind: ArtifactKind::MetadataJson,
            path: directory.join("one.json"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let mut published = Vec::new();
        publish_prepared_with(
            &handle,
            vec![
                PendingArtifact {
                    artifact: summary.clone(),
                    bytes: b"summary".to_vec(),
                },
                PendingArtifact {
                    artifact: json,
                    bytes: b"{}".to_vec(),
                },
            ],
            &mut published,
            |_directory_fd: i32, _dot: &std::ffi::CStr| -> std::io::Result<std::fs::File> {
                Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            },
            open_staged,
        )
        .expect("the unsupported anonymous path publishes staged artifacts");
        assert_eq!(published.len(), 2);
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("summary published"),
            b"summary"
        );
        assert_eq!(
            std::fs::read(directory.join("one.json")).expect("report published"),
            b"{}"
        );
        for name in ["one.md", "one.json"] {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(directory.join(name))
                    .expect("published file exists")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
        }
        let names: Vec<String> = std::fs::read_dir(&directory)
            .expect("output directory readable")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert_eq!(names.len(), 2, "no staged names remain: {names:?}");
        let mut retry_published = Vec::new();
        let error = publish_prepared_with(
            &handle,
            vec![PendingArtifact {
                artifact: summary,
                bytes: b"changed".to_vec(),
            }],
            &mut retry_published,
            |_directory_fd: i32, _dot: &std::ffi::CStr| -> std::io::Result<std::fs::File> {
                Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            },
            open_staged,
        )
        .expect_err("an existing name must never be replaced");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(retry_published.is_empty());
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("original retained"),
            b"summary"
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            2,
            "no staged names survive the collision"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks a failed staged publication removes prepared and pending names.
    ///
    /// Accepts no parameters. Returns unit after injecting an anonymous opener
    /// that fails with EOPNOTSUPP and publishing one artifact colliding with a
    /// pre-existing name while a second artifact is still pending. A leaked
    /// staged name for the failing or the pending artifact, replaced bytes, or
    /// a missing AlreadyExists rejection panics; the scratch tree is removed
    /// after the assertions.
    #[cfg(target_os = "linux")]
    #[test]
    fn forced_staged_fallback_cleans_pending_names_after_collision() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("forced-cleanup-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        std::fs::write(directory.join("one.md"), b"original").expect("existing name created");
        let colliding = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: directory.join("one.md"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let pending = Artifact {
            kind: ArtifactKind::MetadataJson,
            path: directory.join("two.json"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let mut published = Vec::new();
        let error = publish_prepared_with(
            &handle,
            vec![
                PendingArtifact {
                    artifact: colliding,
                    bytes: b"changed".to_vec(),
                },
                PendingArtifact {
                    artifact: pending,
                    bytes: b"{}".to_vec(),
                },
            ],
            &mut published,
            |_directory_fd: i32, _dot: &std::ffi::CStr| -> std::io::Result<std::fs::File> {
                Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            },
            open_staged,
        )
        .expect_err("an existing name must never be replaced");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(published.is_empty());
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("original retained"),
            b"original"
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "only the pre-existing name remains"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks a failed staged preparation removes every earlier staged name.
    ///
    /// Accepts no parameters. Returns unit after one artifact is prepared
    /// through a private staged name and the next artifact's staged creation
    /// fails. A staged name surviving for the already prepared artifact panics;
    /// the scratch tree is removed after the assertions.
    #[cfg(target_os = "linux")]
    #[test]
    fn staged_preparation_failure_removes_earlier_staged_names() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("staged-prepare-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        let first = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: directory.join("one.md"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let second = Artifact {
            kind: ArtifactKind::MetadataJson,
            path: directory.join("two.json"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let mut published = Vec::new();
        let mut staged_opens = 0;
        let error = publish_prepared_with(
            &handle,
            vec![
                PendingArtifact {
                    artifact: first,
                    bytes: b"first".to_vec(),
                },
                PendingArtifact {
                    artifact: second,
                    bytes: b"second".to_vec(),
                },
            ],
            &mut published,
            |_directory_fd: i32, _dot: &std::ffi::CStr| -> std::io::Result<std::fs::File> {
                Err(std::io::Error::from_raw_os_error(libc::EOPNOTSUPP))
            },
            |directory_fd: i32| -> std::io::Result<StagedName> {
                staged_opens += 1;
                if staged_opens == 1 {
                    open_staged(directory_fd)
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EACCES))
                }
            },
        )
        .expect_err("the second staged preparation fails");
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        assert!(published.is_empty());
        let names: Vec<String> = std::fs::read_dir(&directory)
            .expect("output directory readable")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert!(
            names.is_empty(),
            "no staged name survives the failed preparation: {names:?}"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks a failed second staged preparation removes earlier staged names.
    ///
    /// Accepts no parameters. Returns unit after `publish_staged_with` prepares
    /// the first artifact through a private staged name and fails the second
    /// staged creation with `EACCES`. A surviving temporary name, unexpected
    /// error code, or non-empty published list panics; the scratch tree is
    /// removed after the assertions.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn staged_publication_failure_removes_earlier_staged_names() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!(
            "staged-publish-fail-{}-{stamp}",
            std::process::id()
        ));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        let first = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: directory.join("one.md"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let second = Artifact {
            kind: ArtifactKind::MetadataJson,
            path: directory.join("two.json"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let mut published = Vec::new();
        let mut staged_opens = 0;
        let error = publish_staged_with(
            &handle,
            vec![
                PendingArtifact {
                    artifact: first,
                    bytes: b"first".to_vec(),
                },
                PendingArtifact {
                    artifact: second,
                    bytes: b"second".to_vec(),
                },
            ],
            &mut published,
            |directory_fd: i32| -> std::io::Result<StagedName> {
                staged_opens += 1;
                if staged_opens == 1 {
                    open_staged(directory_fd)
                } else {
                    Err(std::io::Error::from_raw_os_error(libc::EACCES))
                }
            },
        )
        .expect_err("the second staged preparation fails");
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
        assert!(published.is_empty());
        let names: Vec<String> = std::fs::read_dir(&directory)
            .expect("output directory readable")
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into())
            .collect();
        assert!(
            names.is_empty(),
            "no staged name survives the failed preparation: {names:?}"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks a colliding staged publication cleans both failing and pending names.
    ///
    /// Accepts no parameters. Returns unit after `publish_staged_with` prepares
    /// two staged files where the first collides with a pre-existing filename.
    /// Replaced bytes, a missing `AlreadyExists` error, or any surviving staged
    /// temporary file panics; the scratch tree is removed after the assertions.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn staged_publication_cleans_pending_names_after_collision() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("staged-collision-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        std::fs::write(directory.join("one.md"), b"original").expect("existing name created");
        let colliding = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: directory.join("one.md"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let pending = Artifact {
            kind: ArtifactKind::MetadataJson,
            path: directory.join("two.json"),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let mut published = Vec::new();
        let error = publish_staged_with(
            &handle,
            vec![
                PendingArtifact {
                    artifact: colliding,
                    bytes: b"changed".to_vec(),
                },
                PendingArtifact {
                    artifact: pending,
                    bytes: b"{}".to_vec(),
                },
            ],
            &mut published,
            open_staged,
        )
        .expect_err("an existing name must never be replaced");
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(published.is_empty());
        assert_eq!(
            std::fs::read(directory.join("one.md")).expect("original retained"),
            b"original"
        );
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            1,
            "only the pre-existing name remains"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks an invalid filename drops the popped staged entry's name.
    ///
    /// Accepts no parameters. Returns unit after handing publish_prepared_files
    /// one staged entry whose artifact path has no final filename. A rejection
    /// other than InvalidInput or a staged name surviving the popped entry
    /// panics; the scratch tree is removed after the assertions.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn invalid_filename_drops_the_popped_staged_name() {
        use std::os::fd::AsRawFd;
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("staged-invalid-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        let staged = open_staged(handle.as_raw_fd()).expect("staged name prepared");
        let artifact = Artifact {
            kind: ArtifactKind::MetadataSummary,
            path: PathBuf::from(".."),
            source: glassbox_inspect::reports::FactSource::GeneratedArtifact,
            completeness: Completeness::Exact,
            width: None,
            height: None,
        };
        let mut published = Vec::new();
        let error = publish_prepared_files(
            handle.as_raw_fd(),
            vec![(PreparedFile::Staged(staged), artifact, b"x".to_vec())],
            &mut published,
        )
        .expect_err("an artifact without a filename is rejected");
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
        assert!(published.is_empty());
        assert_eq!(
            std::fs::read_dir(&directory).unwrap().count(),
            0,
            "the popped staged name is removed by its guard"
        );
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks persist_extract preserves Unavailable completeness across stdout and saved JSON.
    ///
    /// Accepts no parameters. Returns unit after publishing an already-failed
    /// Unavailable extraction report and asserting the in-memory report matches
    /// the saved `.json` artifact, then verifying a colliding publish downgrades
    /// Exact completeness to Partial while keeping Unavailable unchanged. Any
    /// mismatch or I/O error panics; the scratch tree is removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn persist_extract_keeps_unavailable_completeness_consistent_with_saved_json() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("persist-unavail-{}-{stamp}", std::process::id()));

        let mut failed_report = ExtractReport::default();
        failed_report.metadata.identity.path = PathBuf::from("broken.bin");
        failed_report.status = OperationStatus::Failed;
        failed_report.metadata.status = OperationStatus::Failed;
        failed_report.completeness = Completeness::Unavailable;

        persist_extract(&mut failed_report, &directory, 1, false, true, true);
        assert_eq!(failed_report.completeness, Completeness::Unavailable);
        let json_artifact = failed_report
            .metadata
            .artifacts
            .iter()
            .find(|item| item.kind == ArtifactKind::ExtractionJson)
            .expect("extraction json artifact published");
        let saved: serde_json::Value = serde_json::from_slice(
            &std::fs::read(&json_artifact.path).expect("saved json readable"),
        )
        .expect("saved json parses");
        let stdout_value =
            serde_json::to_value(failed_report.to_v3(false)).expect("stdout report serializes");
        assert_eq!(saved["completeness"], "unavailable");
        assert_eq!(stdout_value["completeness"], saved["completeness"]);

        // Re-publishing at the same sequence/stem collides; Exact degrades to Partial,
        // extraction status fails, nested inspection status remains Complete, and
        // Unavailable remains Unavailable.
        let mut colliding_exact = ExtractReport::default();
        colliding_exact.metadata.identity.path = PathBuf::from("broken.bin");
        colliding_exact.status = OperationStatus::Complete;
        colliding_exact.metadata.status = OperationStatus::Complete;
        colliding_exact.completeness = Completeness::Exact;
        persist_extract(&mut colliding_exact, &directory, 1, false, true, true);
        assert_eq!(colliding_exact.status, OperationStatus::Failed);
        assert_eq!(colliding_exact.metadata.status, OperationStatus::Complete);
        assert!(colliding_exact.metadata.diagnostics.is_empty());
        assert!(
            colliding_exact
                .diagnostics
                .iter()
                .any(|d| d.code == "extraction_output_failed")
        );
        assert_eq!(colliding_exact.completeness, Completeness::Partial);

        let mut colliding_unavailable = ExtractReport::default();
        colliding_unavailable.metadata.identity.path = PathBuf::from("broken.bin");
        colliding_unavailable.status = OperationStatus::Failed;
        colliding_unavailable.metadata.status = OperationStatus::Failed;
        colliding_unavailable.completeness = Completeness::Unavailable;
        persist_extract(&mut colliding_unavailable, &directory, 1, false, true, true);
        assert_eq!(
            colliding_unavailable.completeness,
            Completeness::Unavailable
        );

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks persist_extract returns the exact rendered Markdown written to disk (same frontmatter ULID).
    ///
    /// Accepts no parameters. Returns unit after persisting a transcript
    /// extraction report with YAML frontmatter and asserting the returned
    /// Markdown string is byte-identical to the saved `.md` artifact (sharing
    /// the same `id` ULID and `updated_at` timestamp). Panics on mismatch or
    /// I/O error; cleans up its scratch directory afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn persist_extract_returns_exact_saved_markdown_including_frontmatter_ulid() {
        use glassbox_inspect::reports::{AsrReceipt, ContentUnit, MediaReceipt, NativeUnit};

        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("persist-md-ulid-{}-{stamp}", std::process::id()));

        let mut report = ExtractReport::default();
        report.metadata.identity.path = PathBuf::from("speech.wav");
        report.status = OperationStatus::Complete;
        report.metadata.status = OperationStatus::Complete;
        report.completeness = Completeness::Exact;
        report.content.units.push(ContentUnit {
            unit_index: 1,
            unit: NativeUnit::TimeInterval,
            source_part: None,
            label: None,
            marker_index: None,
            start_ms: Some(0),
            end_ms: Some(11_000),
            text: Some("And so my fellow Americans.".to_owned()),
            rows: Vec::new(),
            provenance: Vec::new(),
            completeness: Completeness::Exact,
        });
        report.media = Some(MediaReceipt {
            contact_sheet: None,
            asr: Some(AsrReceipt {
                mode: "short".to_owned(),
                backend: "whisper-onnx".to_owned(),
                model: "whisper-tiny.en-onnx".to_owned(),
                provider: "cpu".to_owned(),
                protocol: None,
                language: Some("en".to_owned()),
                duration_ms: 11_000,
                sampled_ranges_ms: vec![[0, 11_000]],
                windows_attempted: 1,
                windows_succeeded: 1,
            }),
            audio_extract: None,
        });

        let returned = persist_extract(&mut report, &directory, 1, false, true, true);
        let md_artifact = report
            .metadata
            .artifacts
            .iter()
            .find(|item| item.kind == ArtifactKind::MarkdownContent)
            .expect("markdown artifact published");
        let on_disk = std::fs::read_to_string(&md_artifact.path).expect("saved markdown readable");
        assert!(returned.starts_with("---\n"));
        assert!(returned.contains("schema: \"glassbox:v1\""));
        assert!(returned.contains("id: \""));
        assert_eq!(returned, on_disk);

        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks publish_one short-circuits AlreadyExists from link_procfs without staging fallback.
    ///
    /// Accepts no parameters. Returns unit after deterministically verifying via
    /// `publish_one_with_procfs` that `AlreadyExists` never invokes
    /// `link_staging`, plus running a guarded live probe when the host supports
    /// anonymous `O_TMPFILE` creation and `/proc/self/fd` linking. Falling back
    /// to `link_staging` on `AlreadyExists`, losing `AlreadyExists`, or mutating
    /// the existing file panics; the scratch tree is removed afterward.
    #[cfg(target_os = "linux")]
    #[test]
    fn publish_one_short_circuits_procfs_already_exists() {
        use std::os::fd::AsRawFd;
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("procfs-exists-{}-{stamp}", std::process::id()));
        prepare_directory(&directory).expect("private output created");
        let handle = open_directory(&directory).expect("output directory opened");
        std::fs::write(directory.join("existing.json"), b"keep").expect("existing file created");
        let dot = std::ffi::CString::new(".").expect("dot cstring");
        let name = std::ffi::CString::new("existing.json").expect("filename cstring");

        // Deterministic unit test independent of sandbox `/proc/self/fd` restrictions.
        let mut empty_path_usable = false;
        let mut staging_called = false;
        let synthetic = publish_one_with_procfs(
            handle.as_raw_fd(),
            &handle,
            b"new",
            &name,
            &mut empty_path_usable,
            |_, _, _| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "synthetic procfs collision",
                ))
            },
            |_, _, _| {
                staging_called = true;
                Ok(())
            },
        )
        .expect_err("procfs AlreadyExists must short-circuit");
        assert_eq!(synthetic.kind(), std::io::ErrorKind::AlreadyExists);
        assert!(
            !staging_called,
            "link_staging must never run after procfs AlreadyExists"
        );
        assert!(
            !synthetic
                .to_string()
                .contains("descriptor link unavailable"),
            "procfs AlreadyExists must not wrap staging error: {synthetic}"
        );

        // Guarded live kernel probe when `O_TMPFILE` is available on this host.
        if let Ok(mut probe) = open_anonymous(handle.as_raw_fd(), &dot) {
            probe.write_all(b"probe").expect("probe write succeeds");
            let probe_name = std::ffi::CString::new("probe.tmp").expect("probe cstring");
            match link_procfs(&probe, handle.as_raw_fd(), &probe_name) {
                Ok(()) => {
                    drop(probe);
                    let _ = std::fs::remove_file(directory.join("probe.tmp"));
                    let mut anonymous =
                        open_anonymous(handle.as_raw_fd(), &dot).expect("anonymous file opened");
                    anonymous
                        .write_all(b"new")
                        .expect("anonymous write succeeds");
                    let mut empty_path_usable = false;
                    let error = publish_one(
                        handle.as_raw_fd(),
                        &anonymous,
                        b"new",
                        &name,
                        &mut empty_path_usable,
                    )
                    .expect_err("collision must return AlreadyExists");
                    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                    assert!(
                        !error.to_string().contains("descriptor link unavailable"),
                        "procfs AlreadyExists must not fall back to link_staging: {error}"
                    );
                }
                Err(probe_error) if procfs_link_restricted(&probe_error) => {
                    let mut empty_path_usable = false;
                    let error = publish_one(
                        handle.as_raw_fd(),
                        &probe,
                        b"new",
                        &name,
                        &mut empty_path_usable,
                    )
                    .expect_err("staged fallback collision must still return AlreadyExists");
                    assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
                }
                Err(probe_error) => panic!("unexpected live procfs probe failure: {probe_error}"),
            }
            assert_eq!(
                std::fs::read(directory.join("existing.json")).expect("original retained"),
                b"keep"
            );
        }
        drop(handle);
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Asserts that no `.glassbox-tmp-*` staged files exist directly inside a directory.
    ///
    /// Accepts a directory path to inspect. Returns unit when zero staged
    /// temporary entries are present; panics if the directory cannot be read or
    /// any `.glassbox-tmp-*` file remains.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn assert_no_staged_temps(directory: &Path) {
        let leaked: Vec<String> = std::fs::read_dir(directory)
            .expect("directory readable")
            .map(|entry| {
                entry
                    .expect("directory entry readable")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .filter(|name| name.starts_with(".glassbox-tmp-"))
            .collect();
        assert!(
            leaked.is_empty(),
            "leaked staged temporary files in {}: {leaked:?}",
            directory.display()
        );
    }

    /// Checks `publish_single_file` works in `0755` parents, creates `0700` nested parents, writes `0600`, and refuses overwrite.
    ///
    /// Accepts no parameters. Returns unit after publishing into an existing
    /// `0755` directory, verifying `AlreadyExists` collision preservation, and
    /// publishing through newly created nested parents (`0700`) with zero
    /// leaked `.glassbox-tmp-*` files. Any unexpected mode, overwritten byte,
    /// leaked temp file, or I/O failure panics; the scratch tree is removed
    /// afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publish_single_file_writes_0600_in_0755_parent_creates_0700_nested_and_rejects_collision() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let parent_0755 = root.join(format!("single-0755-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&parent_0755).expect("scratch parent created");
        std::fs::set_permissions(&parent_0755, std::fs::Permissions::from_mode(0o755))
            .expect("scratch parent set to 0755");

        let target = parent_0755.join("preview.jpg");
        let published = publish_single_file(&target, b"jpeg-payload")
            .expect("single-file publish succeeds inside existing 0755 parent");
        assert_eq!(
            published,
            absolute_lexical(&target).expect("target normalizes")
        );
        assert_eq!(
            std::fs::read(&target).expect("published file readable"),
            b"jpeg-payload"
        );
        assert_eq!(
            std::fs::symlink_metadata(&target)
                .expect("published metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::symlink_metadata(&parent_0755)
                .expect("existing parent metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o755,
            "existing 0755 parent permissions must be preserved"
        );
        assert_no_staged_temps(&parent_0755);

        // Collision must fail closed with AlreadyExists without modifying bytes or leaking temps.
        let collision = publish_single_file(&target, b"overwritten-bytes")
            .expect_err("existing single-file target must never be overwritten");
        assert_eq!(collision.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&target).expect("original bytes preserved"),
            b"jpeg-payload"
        );
        assert_no_staged_temps(&parent_0755);

        // Missing nested parent directories must be created privately with mode 0700.
        let nested_a = parent_0755.join("nested-a");
        let nested_b = nested_a.join("nested-b");
        let nested_target = nested_b.join("sheet.png");
        let nested_published = publish_single_file(&nested_target, b"png-payload")
            .expect("nested single-file publish creates missing 0700 parents");
        assert_eq!(
            nested_published,
            absolute_lexical(&nested_target).expect("nested target normalizes")
        );
        assert_eq!(
            std::fs::symlink_metadata(&nested_a)
                .expect("nested-a metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&nested_b)
                .expect("nested-b metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&nested_target)
                .expect("nested file metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::read(&nested_target).expect("nested file readable"),
            b"png-payload"
        );
        assert_no_staged_temps(&nested_b);

        let _ = std::fs::remove_dir_all(&parent_0755);
    }

    /// Checks `publish_single_file` rejects symlink parents, symlink targets, and invalid paths.
    ///
    /// Accepts no parameters. Returns unit after verifying symlink parent
    /// directories, nested paths through symlink parents, live and dangling
    /// symlink targets, non-directory parents, and root/empty/trailing-slash
    /// paths all fail with `InvalidInput` without writing or leaking staged
    /// files. Any accepted unsafe path or leaked file panics; the scratch tree
    /// is removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn publish_single_file_rejects_symlinks_and_invalid_paths() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let scratch = root.join(format!("single-reject-{}-{stamp}", std::process::id()));
        let real_dir = scratch.join("real-dir");
        std::fs::create_dir_all(&real_dir).expect("real scratch dir created");

        let real_file = scratch.join("real-file.jpg");
        std::fs::write(&real_file, b"untouched").expect("real file created");

        let link_dir = scratch.join("link-dir");
        std::os::unix::fs::symlink(&real_dir, &link_dir).expect("symlink dir created");

        let link_target = scratch.join("link-target.jpg");
        std::os::unix::fs::symlink(&real_file, &link_target).expect("symlink file created");

        let dangling_target = scratch.join("dangling-target.jpg");
        std::os::unix::fs::symlink(scratch.join("never-created.jpg"), &dangling_target)
            .expect("dangling symlink created");

        // Symlink immediate parent and symlink ancestor must be rejected.
        let err = publish_single_file(&link_dir.join("out.jpg"), b"data")
            .expect_err("symlink parent rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        let err = publish_single_file(&link_dir.join("sub/out.jpg"), b"data")
            .expect_err("symlink ancestor rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read_dir(&real_dir).unwrap().count(),
            0,
            "nothing created inside symlink target directory"
        );

        // Live and dangling symlink targets must be rejected clearly.
        let err = publish_single_file(&link_target, b"data").expect_err("symlink target rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(
            std::fs::read(&real_file).expect("real file untouched"),
            b"untouched"
        );

        let err = publish_single_file(&dangling_target, b"data")
            .expect_err("dangling symlink target rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert!(!scratch.join("never-created.jpg").exists());

        // Non-directory parent component must be rejected.
        let err = publish_single_file(&real_file.join("child.jpg"), b"data")
            .expect_err("file-as-parent rejected");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);

        // Invalid path shapes must be rejected.
        let trailing_slash = PathBuf::from(format!("{}/", scratch.display()));
        for invalid in [
            PathBuf::from(""),
            PathBuf::from("/"),
            PathBuf::from("."),
            PathBuf::from(".."),
            trailing_slash,
            scratch.join("sub/.."),
            PathBuf::from(format!("{}/.", scratch.display())),
        ] {
            let err = publish_single_file(&invalid, b"data")
                .expect_err("invalid single-file path must be rejected");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "path {:?} must fail with InvalidInput",
                invalid
            );
        }

        assert_no_staged_temps(&scratch);
        assert_no_staged_temps(&real_dir);
        let _ = std::fs::remove_dir_all(&scratch);
    }

    /// Checks `StagedWriter` streams chunks, patches offset-0 headers via `Seek`, publishes `0600`, and rejects collisions.
    ///
    /// Accepts no parameters. Returns unit after streaming a synthetic WAV
    /// placeholder header and PCM chunks, seeking to offset 0 to patch the
    /// header, publishing into a private `0700` `--output-dir`, confirming
    /// `0600` permissions and zero temp leaks, verifying `AlreadyExists` on
    /// collision, and rejecting invalid filenames and `0755` output dirs. Any
    /// mismatch, overwrite, or leaked file panics; scratch trees are removed
    /// afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn staged_writer_streams_seeks_patches_header_publishes_0600_and_handles_collision() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let output_dir = root.join(format!("staged-writer-{}-{stamp}", std::process::id()));

        let mut writer =
            StagedWriter::create(&output_dir, "clip-extract.wav").expect("staged writer opens");
        let placeholder_header = [0u8; 44];
        writer
            .write_all(&placeholder_header)
            .expect("placeholder WAV header written");
        writer
            .write_all(b"chunk-one-")
            .expect("first chunk written");
        writer
            .write_all(b"chunk-two")
            .expect("second chunk written");

        // Seek back to offset 0 and patch the 44-byte header like a streaming WAV writer.
        let pos = writer
            .seek(SeekFrom::Start(0))
            .expect("seek to start succeeds");
        assert_eq!(pos, 0);
        let mut patched_header = [0u8; 44];
        patched_header[..4].copy_from_slice(b"RIFF");
        patched_header[4..8].copy_from_slice(&(36u32 + 19u32).to_le_bytes());
        patched_header[8..12].copy_from_slice(b"WAVE");
        patched_header[40..44].copy_from_slice(&19u32.to_le_bytes());
        writer
            .write_all(&patched_header)
            .expect("patched WAV header written");

        let final_path = writer.publish().expect("staged writer publishes");
        assert_eq!(
            final_path,
            absolute_lexical(&output_dir.join("clip-extract.wav")).expect("final path normalizes")
        );
        assert_eq!(
            std::fs::symlink_metadata(&output_dir)
                .expect("output dir metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&final_path)
                .expect("published file metadata readable")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let mut expected = Vec::with_capacity(44 + 19);
        expected.extend_from_slice(&patched_header);
        expected.extend_from_slice(b"chunk-one-chunk-two");
        assert_eq!(
            std::fs::read(&final_path).expect("published WAV readable"),
            expected
        );
        assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 1);
        assert_no_staged_temps(&output_dir);

        // Pre-existing target must fail immediately in StagedWriter::create without staging a temp file.
        let err = match StagedWriter::create(&output_dir, "clip-extract.wav") {
            Ok(_) => panic!("existing output file must be rejected before staging"),
            Err(error) => error,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&final_path).expect("original WAV untouched"),
            expected
        );
        assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 1);
        assert_no_staged_temps(&output_dir);

        // Concurrent creation race between create() and publish() must still fail atomically via linkat.
        let mut racing = StagedWriter::create(&output_dir, "race-extract.wav")
            .expect("racing staged writer opens temp file before concurrent arrival");
        racing
            .write_all(b"replacement-audio")
            .expect("racing bytes written");
        let race_target = output_dir.join("race-extract.wav");
        std::fs::write(&race_target, b"concurrent-winner").expect("concurrent winner created");
        let err = racing
            .publish()
            .expect_err("concurrently created output file must never be replaced");
        assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&race_target).expect("concurrent winner untouched"),
            b"concurrent-winner"
        );
        assert_eq!(std::fs::read_dir(&output_dir).unwrap().count(), 2);
        assert_no_staged_temps(&output_dir);

        // Invalid filenames must fail closed with InvalidInput.
        for bad_name in ["", ".", "..", "sub/audio.wav", "../escape.wav", "a\0b.wav"] {
            let err = match StagedWriter::create(&output_dir, bad_name) {
                Ok(_) => panic!("bad filename {bad_name:?} must be rejected"),
                Err(error) => error,
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        }

        // StagedWriter targets --output-dir, so an existing 0755 directory must still be rejected.
        let public_dir = root.join(format!("staged-public-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&public_dir).expect("public dir created");
        std::fs::set_permissions(&public_dir, std::fs::Permissions::from_mode(0o755))
            .expect("public dir set to 0755");
        let err = match StagedWriter::create(&public_dir, "audio.wav") {
            Ok(_) => panic!("0755 --output-dir must be rejected by StagedWriter"),
            Err(error) => error,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        let _ = std::fs::remove_dir_all(&output_dir);
        let _ = std::fs::remove_dir_all(&public_dir);
    }

    /// Checks dropping a `StagedWriter` without calling `publish` unlinks its `.glassbox-tmp-*` file.
    ///
    /// Accepts no parameters. Returns unit after creating a `StagedWriter`,
    /// writing partial bytes, confirming the temporary file is visible before
    /// drop, dropping the writer without `publish()`, and asserting the output
    /// directory is completely empty. Any surviving temp file or created final
    /// file panics; the scratch directory is removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn staged_writer_drop_without_publish_unlinks_temp_file() {
        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let output_dir = root.join(format!("staged-drop-{}-{stamp}", std::process::id()));

        {
            let mut writer = StagedWriter::create(&output_dir, "aborted-extract.wav")
                .expect("staged writer created");
            writer
                .write_all(b"partial-audio-stream")
                .expect("partial write succeeds");
            writer.flush().expect("flush succeeds");
            let entries: Vec<String> = std::fs::read_dir(&output_dir)
                .expect("output dir readable")
                .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            assert_eq!(entries.len(), 1);
            assert!(
                entries[0].starts_with(".glassbox-tmp-"),
                "staged temp file should exist before drop: {entries:?}"
            );
            assert!(!output_dir.join("aborted-extract.wav").exists());
        }

        // After Drop without publish(), the staged temp file must be removed cleanly.
        assert_no_staged_temps(&output_dir);
        assert_eq!(
            std::fs::read_dir(&output_dir).unwrap().count(),
            0,
            "dropped StagedWriter must leave output directory empty"
        );
        let _ = std::fs::remove_dir_all(&output_dir);
    }

    /// Checks `contact_sheet_filename` and `audio_extract_filename` sanitize weird stems safely.
    ///
    /// Accepts no parameters. Returns unit after verifying normal stems,
    /// hidden leading-dot stems, dot-only/parent/empty paths, punctuation and
    /// NUL-bearing stems, overlong stems, and unusual extension strings produce
    /// safe single-component filenames with no separators, NULs, or leading
    /// dots. Performs no I/O.
    #[test]
    fn media_filename_helpers_sanitize_stems_and_extensions() {
        assert_eq!(
            contact_sheet_filename(Path::new("/videos/interview.mp4"), "jpg"),
            "interview-contact-sheet.jpg"
        );
        assert_eq!(
            contact_sheet_filename(Path::new("/videos/interview.mp4"), ".PNG"),
            "interview-contact-sheet.png"
        );
        assert_eq!(
            audio_extract_filename(Path::new("/videos/interview.mkv"), "wav"),
            "interview-extract.wav"
        );
        assert_eq!(
            audio_extract_filename(Path::new("/videos/interview.mkv"), "opus"),
            "interview-extract.opus"
        );

        // Leading dots must be stripped so generated files are never hidden or traversal names.
        assert_eq!(
            contact_sheet_filename(Path::new("/tmp/.hidden-clip.mov"), "jpg"),
            "hidden-clip-contact-sheet.jpg"
        );
        assert_eq!(
            audio_extract_filename(Path::new("...leading.mp4"), "flac"),
            "leading-extract.flac"
        );
        for degenerate in ["", ".", "..", "...", "/"] {
            assert_eq!(
                contact_sheet_filename(Path::new(degenerate), "jpg"),
                "media-contact-sheet.jpg"
            );
            assert_eq!(
                audio_extract_filename(Path::new(degenerate), "wav"),
                "media-extract.wav"
            );
        }

        // Weird characters, NUL bytes, path separators in extensions, and overlong stems.
        let weird_sheet =
            contact_sheet_filename(Path::new("nested/my clip: 2026\0(v1)!.mp4"), "../png");
        assert_eq!(weird_sheet, "my_clip__2026__v1__-contact-sheet.png");
        assert!(!weird_sheet.starts_with('.'));
        assert!(!weird_sheet.contains('/'));
        assert!(!weird_sheet.contains('\\'));
        assert!(!weird_sheet.contains('\0'));

        let long_stem = "a".repeat(80);
        let long_path = PathBuf::from(format!("{long_stem}.mp4"));
        let long_audio = audio_extract_filename(&long_path, "");
        assert_eq!(long_audio, format!("{}-extract.wav", "a".repeat(48)));

        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            for name in [weird_sheet, long_audio] {
                validate_filename_cstr(std::ffi::OsStr::new(&name))
                    .expect("generated media filename must pass strict component validation");
            }
        }
    }

    /// Checks the anchored directory walk rejects ancestor symlinks without creating dirs through them.
    ///
    /// Accepts no parameters. Returns unit after verifying both
    /// `publish_single_file` (`dir/link/sub/out.jpg` and `dir/link/out.jpg`)
    /// and `StagedWriter::create` (`dir/link/sub` and `dir/link`) fail with
    /// `InvalidInput`, emit the symlink-rejection message, create zero
    /// subdirectories inside the symlink target, and leak zero `.glassbox-tmp-*`
    /// files. Any symlink traversal or leaked entry panics; the scratch tree is
    /// removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn anchored_directory_walk_rejects_ancestor_symlinks_without_creating_dirs() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let dir = root.join(format!("anchored-symlink-{}-{stamp}", std::process::id()));
        let real_target = dir.join("real-target");
        std::fs::create_dir_all(&real_target).expect("real target dir created");
        std::fs::set_permissions(&real_target, std::fs::Permissions::from_mode(0o700))
            .expect("real target set to 0700");

        let link = dir.join("link");
        std::os::unix::fs::symlink(&real_target, &link).expect("ancestor symlink created");

        for target_path in [link.join("sub/out.jpg"), link.join("out.jpg")] {
            let err = publish_single_file(&target_path, b"payload")
                .expect_err("ancestor symlink path must be rejected by publish_single_file");
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                err.to_string()
                    .contains("symbolic-link output component rejected"),
                "unexpected error message: {err}"
            );
        }

        for output_dir in [link.join("sub"), link.clone()] {
            let err = match StagedWriter::create(&output_dir, "out.wav") {
                Ok(_) => panic!("ancestor symlink dir must be rejected by StagedWriter::create"),
                Err(error) => error,
            };
            assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
            assert!(
                err.to_string()
                    .contains("symbolic-link output component rejected"),
                "unexpected error message: {err}"
            );
        }

        assert!(
            !real_target.join("sub").exists(),
            "no directory may be created through an ancestor symlink"
        );
        assert_eq!(
            std::fs::read_dir(&real_target).unwrap().count(),
            0,
            "symlink target directory must remain completely empty"
        );
        assert_no_staged_temps(&dir);
        assert_no_staged_temps(&real_target);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Checks pre-staging `fstatat(AT_SYMLINK_NOFOLLOW)` rejects existing symlink and regular targets without staging.
    ///
    /// Accepts no parameters. Returns unit after verifying both
    /// `publish_single_file` and `StagedWriter::create` reject a live symlink
    /// target (`InvalidInput`), a dangling symlink target (`InvalidInput`), and
    /// a pre-existing regular file (`AlreadyExists`) before calling
    /// `open_staged`, confirmed both by zero `.glassbox-tmp-*` leaks and (for
    /// non-root runs) by enforcing a `0500` read-only parent where any staging
    /// attempt would fail with `PermissionDenied`. Any unexpected error kind,
    /// mutated byte, or temp creation panics; the scratch tree is removed
    /// afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn pre_staging_fstatat_rejects_symlink_and_regular_targets_without_creating_temps() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let dir = root.join(format!("prestage-fstatat-{}-{stamp}", std::process::id()));
        prepare_directory(&dir).expect("private 0700 dir created");

        let regular = dir.join("existing.jpg");
        std::fs::write(&regular, b"original-bytes").expect("existing regular file created");
        let live_link = dir.join("live-link.jpg");
        std::os::unix::fs::symlink(&regular, &live_link).expect("live symlink created");
        let dangling_link = dir.join("dangling-link.jpg");
        std::os::unix::fs::symlink(dir.join("never-created.jpg"), &dangling_link)
            .expect("dangling symlink created");

        // When non-root, drop write permission (0500) so any attempt to create a
        // `.glassbox-tmp-*` file before `fstatat` rejection would fail with EACCES.
        // SAFETY: geteuid has no preconditions and returns the effective UID.
        let is_non_root = unsafe { libc::geteuid() } != 0;
        let modes_to_test: &[u32] = if is_non_root {
            &[0o700, 0o500]
        } else {
            &[0o700]
        };

        for &mode in modes_to_test {
            std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(mode))
                .expect("dir permissions set");

            for (name, path) in [
                ("live-link.jpg", &live_link),
                ("dangling-link.jpg", &dangling_link),
            ] {
                let err = publish_single_file(path, b"new")
                    .expect_err("symlink target must fail in publish_single_file");
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
                assert!(
                    err.to_string()
                        .contains("symbolic-link output target rejected"),
                    "unexpected error message: {err}"
                );

                let err = match StagedWriter::create(&dir, name) {
                    Ok(_) => panic!("symlink target must fail in StagedWriter::create"),
                    Err(error) => error,
                };
                assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
                assert!(
                    err.to_string()
                        .contains("symbolic-link output target rejected"),
                    "unexpected error message: {err}"
                );
            }

            let err = publish_single_file(&regular, b"overwrite")
                .expect_err("existing regular file must fail with AlreadyExists");
            assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
            assert!(
                err.to_string().contains("output file already exists"),
                "unexpected error message: {err}"
            );

            let err = match StagedWriter::create(&dir, "existing.jpg") {
                Ok(_) => panic!("existing regular file must fail in StagedWriter::create"),
                Err(error) => error,
            };
            assert_eq!(err.kind(), std::io::ErrorKind::AlreadyExists);
            assert!(
                err.to_string().contains("output file already exists"),
                "unexpected error message: {err}"
            );

            assert_no_staged_temps(&dir);
        }

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .expect("dir permissions restored for cleanup");
        assert_eq!(
            std::fs::read(&regular).expect("regular file bytes preserved"),
            b"original-bytes"
        );
        assert!(!dir.join("never-created.jpg").exists());
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Checks anchored walk permits `0755` parents for `publish_single_file`, creates `0700` missing dirs, and enforces `0700` on `StagedWriter::create`.
    ///
    /// Accepts no parameters. Returns unit after verifying `publish_single_file`
    /// succeeds inside an existing `0755` directory (preserving `0755`) and
    /// creates missing intermediate directories with `0700` via `mkdirat` +
    /// `fchmod`, while `StagedWriter::create` rejects an existing `0755`
    /// `--output-dir` with `PermissionDenied` via anchored `fstat` and creates
    /// a missing `--output-dir` with `0700`. Any mode mismatch or temp leak
    /// panics; the scratch tree is removed afterward.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn anchored_walk_permissions_contract_for_single_file_and_staged_writer() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let parent_0755 = root.join(format!("anchored-perms-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&parent_0755).expect("0755 parent created");
        std::fs::set_permissions(&parent_0755, std::fs::Permissions::from_mode(0o755))
            .expect("parent set to 0755");

        let direct = parent_0755.join("direct.jpg");
        publish_single_file(&direct, b"direct-bytes")
            .expect("publish_single_file accepts existing 0755 parent");
        assert_eq!(
            std::fs::symlink_metadata(&parent_0755)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o755
        );
        assert_eq!(
            std::fs::symlink_metadata(&direct)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );

        let missing_dir = parent_0755.join("missing-a/missing-b");
        let nested_file = missing_dir.join("nested.jpg");
        publish_single_file(&nested_file, b"nested-bytes")
            .expect("publish_single_file creates missing ancestors 0700");
        assert_eq!(
            std::fs::symlink_metadata(parent_0755.join("missing-a"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&missing_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );

        let err = match StagedWriter::create(&parent_0755, "clip.wav") {
            Ok(_) => panic!("StagedWriter::create must reject existing 0755 --output-dir"),
            Err(error) => error,
        };
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);

        let private_dir = parent_0755.join("private-out");
        let mut writer = StagedWriter::create(&private_dir, "clip.wav")
            .expect("StagedWriter::create creates missing 0700 --output-dir");
        writer.write_all(b"wav-bytes").expect("write succeeds");
        let published = writer.publish().expect("publish succeeds");
        assert_eq!(
            std::fs::symlink_metadata(&private_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::symlink_metadata(&published)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_no_staged_temps(&parent_0755);
        assert_no_staged_temps(&missing_dir);
        assert_no_staged_temps(&private_dir);

        let _ = std::fs::remove_dir_all(&parent_0755);
    }
}
