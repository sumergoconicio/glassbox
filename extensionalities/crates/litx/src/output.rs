// Module: glassbox-cli private artifact publication.
// File: output.rs.
// Responsibility: Save v2 metadata and content artifacts through no-overwrite, no-follow, complete-file publication.

use glassbox_inspect::reports::{
    Artifact, ArtifactKind, Completeness, Diagnostic, DiagnosticSeverity, DiagnosticStage,
    DocumentMetadata, ExtractReport, MetadataReport, OperationStatus,
};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(target_os = "linux")]
use std::os::unix::ffi::OsStrExt;

#[cfg(target_os = "linux")]
const AT_EMPTY_PATH: i32 = 0x1000;
#[cfg(target_os = "linux")]
const AT_FDCWD: i32 = -100;
#[cfg(target_os = "linux")]
const AT_SYMLINK_FOLLOW: i32 = 0x400;
#[cfg(target_os = "linux")]
const O_CLOEXEC: i32 = 0x80000;
#[cfg(target_os = "linux")]
const O_WRONLY: i32 = 1;
#[cfg(target_os = "linux")]
const O_CREAT: i32 = 0x40;
#[cfg(target_os = "linux")]
const O_EXCL: i32 = 0x80;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const O_DIRECTORY: i32 = 0x4000;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
const O_DIRECTORY: i32 = 0x10000;
#[cfg(all(target_os = "linux", any(target_arch = "arm", target_arch = "aarch64")))]
const O_NOFOLLOW: i32 = 0x8000;
#[cfg(all(target_os = "linux", any(target_arch = "x86", target_arch = "x86_64")))]
const O_NOFOLLOW: i32 = 0x20000;
#[cfg(target_os = "linux")]
const O_TMPFILE: i32 = 0x400000 | O_DIRECTORY;

#[cfg(target_os = "linux")]
unsafe extern "C" {
    fn open(path: *const std::os::raw::c_char, flags: i32, ...) -> i32;
    fn openat(dirfd: i32, path: *const std::os::raw::c_char, flags: i32, ...) -> i32;
    fn linkat(
        olddirfd: i32,
        oldpath: *const std::os::raw::c_char,
        newdirfd: i32,
        newpath: *const std::os::raw::c_char,
        flags: i32,
    ) -> i32;
    fn unlinkat(dirfd: i32, path: *const std::os::raw::c_char, flags: i32) -> i32;
}

/// One fully encoded artifact awaiting anonymous temporary-file publication.
struct PendingArtifact {
    /// Public path and typed artifact facts included in the v2 report.
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

/// Creates missing output components privately and rejects links or public roots.
///
/// Accepts an absolute output directory. Returns the same path after ensuring
/// each component is a real directory. Missing directories are created
/// atomically with 0700 and concurrent creators of the same component are
/// tolerated by revalidation; an existing final directory must already
/// forbid group/other access. Existing ancestor permissions are preserved,
/// and all filesystem errors are returned.
fn prepare_directory(path: &Path) -> std::io::Result<()> {
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

/// Creates one missing output component privately, tolerating concurrent creators.
///
/// Accepts the absolute missing component path. Returns unit after one atomic
/// no-replace creation or revalidation of a concurrently created entry. A lost
/// creation race is resolved by revalidation: a concurrently created real
/// directory is accepted unchanged, while a link or file squatted into the
/// race window is rejected with the original creation error. Creation pins
/// mode 0700 before any entry becomes visible, and the explicit permission
/// restore keeps umask-stripped owner bits intact.
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
/// handle anchored for anonymous file creation. Missing paths, links, and host
/// open errors are returned without creating output files.
#[cfg(target_os = "linux")]
fn open_directory(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    let path_string = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in output path"))?;
    // SAFETY: path_string is NUL-terminated and alive for the call; open copies it.
    let descriptor = unsafe { open(path_string.as_ptr(), O_DIRECTORY | O_NOFOLLOW | O_CLOEXEC) };
    if descriptor < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        // SAFETY: this function owns the newly returned unique descriptor.
        Ok(unsafe { std::fs::File::from_raw_fd(descriptor) })
    }
}

/// One prepared artifact awaiting its final public name.
#[cfg(target_os = "linux")]
enum PreparedFile {
    /// Unnamed temporary inode created through O_TMPFILE and held open until
    /// one of the descriptor linking strategies publishes it.
    Anonymous(std::fs::File),
    /// Private staged temporary name holding the complete bytes; used when
    /// the kernel or filesystem rejects anonymous O_TMPFILE creation. The
    /// guard removes the staged name on drop unless publication disarms it.
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
    // SAFETY: dot is NUL-terminated and O_TMPFILE creates a new unnamed inode.
    let descriptor = unsafe {
        openat(
            directory_fd,
            dot.as_ptr(),
            O_WRONLY | O_TMPFILE | O_CLOEXEC,
            0o600,
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

/// Publishes prepared artifacts and leaves no staged name behind.
///
/// Accepts the anchored directory descriptor, prepared artifacts with their
/// complete bytes, and the publication ledger. Returns unit after each file
/// is linked under its final name with no-replace semantics; anonymous files
/// use the strongest descriptor linking strategy and staged files link
/// directly. Every staged name is owned by a guard that removes it on drop,
/// so an invalid filename on the popped artifact, a failed link, or an
/// abandoned pending artifact cannot leak a temporary name; artifacts linked
/// before a failure stay listed in `published`.
#[cfg(target_os = "linux")]
fn publish_prepared_files(
    directory_fd: i32,
    prepared: Vec<(PreparedFile, Artifact, Vec<u8>)>,
    published: &mut Vec<Artifact>,
) -> std::io::Result<()> {
    let mut items: std::collections::VecDeque<(PreparedFile, Artifact, Vec<u8>)> = prepared.into();
    let mut empty_path_usable = true;
    while let Some((prepared, artifact, bytes)) = items.pop_front() {
        let file_name = artifact.path.file_name().ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing filename")
        })?;
        let name = std::ffi::CString::new(file_name.as_bytes()).map_err(|_| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "NUL in filename")
        })?;
        match prepared {
            PreparedFile::Anonymous(file) => {
                publish_one(directory_fd, &file, &bytes, &name, &mut empty_path_usable)?;
            }
            PreparedFile::Staged(staged) => {
                staged.publish(&name)?;
            }
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
        linkat(
            file.as_raw_fd(),
            empty.as_ptr(),
            directory_fd,
            name.as_ptr(),
            AT_EMPTY_PATH,
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
        linkat(
            AT_FDCWD,
            descriptor.as_ptr(),
            directory_fd,
            name.as_ptr(),
            AT_SYMLINK_FOLLOW,
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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

#[cfg(target_os = "linux")]
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
            linkat(
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
        let removed = unsafe { unlinkat(self.directory_fd, self.name.as_ptr(), 0) } == 0;
        self.armed = false;
        if !removed {
            return Err(std::io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
impl Drop for StagedName {
    fn drop(&mut self) {
        if self.armed {
            // SAFETY: directory_fd stays valid for the guard's lifetime and
            // flags 0 remove a non-directory name; removal is best effort
            // because the original failure is the reported error.
            let _ = unsafe { unlinkat(self.directory_fd, self.name.as_ptr(), 0) };
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
#[cfg(target_os = "linux")]
fn open_staged(directory_fd: i32) -> std::io::Result<StagedName> {
    use std::os::fd::FromRawFd;
    let name = staged_name()?;
    // SAFETY: directory_fd is valid; O_CREAT with O_EXCL and O_NOFOLLOW either
    // creates the staged name once with mode 0600 or fails without following a
    // squatted link.
    let descriptor = unsafe {
        openat(
            directory_fd,
            name.as_ptr(),
            O_WRONLY | O_CREAT | O_EXCL | O_NOFOLLOW | O_CLOEXEC,
            0o600,
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
#[cfg(target_os = "linux")]
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
#[cfg(target_os = "linux")]
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
    if *empty_path_usable {
        match link_empty_path(file, directory_fd, name) {
            Ok(()) => return Ok(()),
            Err(error) if empty_path_denied(&error) => *empty_path_usable = false,
            Err(error) => return Err(error),
        }
    }
    match link_procfs(file, directory_fd, name) {
        Ok(()) => Ok(()),
        Err(procfs) => link_staging(directory_fd, name, bytes).map_err(|staged| {
            std::io::Error::new(
                staged.kind(),
                format!("descriptor link unavailable (procfs: {procfs}; staged: {staged})"),
            )
        }),
    }
}

/// Publishes complete v2 artifacts into one private no-follow output directory.
///
/// Accepts a destination directory and fully serialized artifact payloads.
/// Returns published typed paths on success. Directory, link, serialization,
/// write, collision, and sync failures return OutputFailure; concurrent or
/// existing names are never replaced and no named temporary files are leaked.
#[cfg(target_os = "linux")]
fn publish(
    directory: &Path,
    pending: Vec<PendingArtifact>,
) -> Result<Vec<Artifact>, OutputFailure> {
    let directory = absolute_lexical(directory)
        .map_err(|error| failure("resolve output directory", vec![], error))?;
    prepare_directory(&directory)
        .map_err(|error| failure("prepare output directory", vec![], error))?;
    let handle = open_directory(&directory)
        .map_err(|error| failure("open output directory", vec![], error))?;
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

/// Reports unsupported anonymous publication on non-Linux hosts without writes.
///
/// Accepts a directory and pending artifacts. Returns an operational failure;
/// no destination is created, truncated, or followed. Native Linux publication
/// is the verified implementation for this task.
#[cfg(not(target_os = "linux"))]
fn publish(
    _directory: &Path,
    _pending: Vec<PendingArtifact>,
) -> Result<Vec<Artifact>, OutputFailure> {
    Err(OutputFailure {
        message: "safe anonymous artifact publication is currently supported on Linux only".into(),
        published: vec![],
    })
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
    let mut basename: String = source
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '_' | '-') {
                character
            } else {
                '_'
            }
        })
        .take(48)
        .collect();
    if basename.is_empty() || basename.chars().all(|character| character == '.') {
        basename = "document".into();
    }
    format!("{basename}-{sequence:06}")
}

/// Selects a trusted digest prefix or explicit no-hash artifact identity.
///
/// Accepts shared document metadata. Returns the first twelve hexadecimal
/// digest characters when consistent, otherwise `nohash`. No source bytes are
/// read and no new digest is computed.
fn digest_prefix(metadata: &DocumentMetadata) -> String {
    metadata
        .digest
        .digest
        .as_deref()
        .filter(|_| {
            metadata.digest.consistency == glassbox_inspect::reports::ConsistencyStatus::Consistent
        })
        .map(|digest| digest[..12].to_owned())
        .unwrap_or_else(|| "nohash".to_owned())
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

/// Persists a content-free summary and identical v2 metadata JSON report.
///
/// Accepts a mutable metadata report, output directory, and deterministic item
/// sequence. Returns unit after artifact publication and typed report updates.
/// Collisions, unsafe directories, encoding, and I/O failures become artifact
/// diagnostics and failed status; no existing file is overwritten.
pub(crate) fn persist_metadata(report: &mut MetadataReport, directory: &Path, sequence: usize) {
    let stem = artifact_stem(&report.metadata.identity, sequence);
    let stem = format!("{}-{}", stem, digest_prefix(&report.metadata));
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
    let result = serde_json::to_vec(report)
        .map_err(|error| failure("serialize metadata report", vec![], error))
        .and_then(|json| {
            publish(
                &directory,
                vec![
                    PendingArtifact {
                        artifact: summary_artifact,
                        bytes: glassbox_inspect::metadata::summary(report).into_bytes(),
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

/// Persists selected Markdown content and identical canonical v2 extraction JSON.
///
/// Accepts a mutable extraction report, output directory, and deterministic
/// item sequence. Returns unit after complete-file publication. Collisions,
/// unsafe directories, serialization, and I/O failures become artifact errors;
/// existing paths and prior successful artifacts are never truncated.
pub(crate) fn persist_extract(report: &mut ExtractReport, directory: &Path, sequence: usize) {
    let stem = artifact_stem(&report.metadata.identity, sequence);
    let stem = format!("{}-{}", stem, digest_prefix(&report.metadata));
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
    let result = serde_json::to_vec(report)
        .map_err(|error| failure("serialize extraction report", vec![], error))
        .and_then(|json| {
            publish(
                &directory,
                vec![
                    PendingArtifact {
                        artifact: markdown_artifact,
                        bytes: glassbox_inspect::selection::render_markdown(report).into_bytes(),
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
        "extraction_output_failed",
    );
    if report.metadata.status == OperationStatus::Failed {
        report.completeness = Completeness::Partial;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    #[cfg(target_os = "linux")]
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

    /// Checks the procfs fallback links anonymous files as an ordinary user.
    ///
    /// Accepts no parameters. Returns unit after linking one anonymous
    /// temporary exclusively through `/proc/self/fd`. A link failure, changed
    /// bytes, or a public mode panics; this is the publication path Linux below
    /// 6.10 takes after the kernel denies AT_EMPTY_PATH.
    #[cfg(target_os = "linux")]
    #[test]
    fn procfs_fallback_links_anonymous_files_privately() {
        use std::os::fd::{AsRawFd, FromRawFd};
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
        // SAFETY: dot is NUL-terminated and O_TMPFILE creates a new unnamed inode.
        let descriptor = unsafe {
            openat(
                handle.as_raw_fd(),
                dot.as_ptr(),
                1 | O_TMPFILE | O_CLOEXEC,
                0o600,
            )
        };
        assert!(descriptor >= 0, "temporary inode created");
        // SAFETY: this test owns the newly returned unique descriptor.
        let mut temporary = unsafe { std::fs::File::from_raw_fd(descriptor) };
        temporary.write_all(b"fallback").expect("bytes written");
        temporary.sync_all().expect("bytes synced");
        let name = std::ffi::CString::new("one.md").unwrap();
        link_procfs(&temporary, handle.as_raw_fd(), &name)
            .expect("ordinary-user procfs link succeeds");
        drop(temporary);
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
        let _ = std::fs::remove_dir_all(&directory);
    }

    /// Checks the staged fallback stays atomic, private, and no-overwrite.
    ///
    /// Accepts no parameters. Returns unit after one staged publication and one
    /// colliding retry. Replaced bytes, leaked staged names, or a missing
    /// AlreadyExists rejection panic; no anonymous inode is involved.
    #[cfg(target_os = "linux")]
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

    /// Checks an invalid filename drops the popped staged entry's name.
    ///
    /// Accepts no parameters. Returns unit after handing publish_prepared_files
    /// one staged entry whose artifact path has no final filename. A rejection
    /// other than InvalidInput or a staged name surviving the popped entry
    /// panics; the scratch tree is removed after the assertions.
    #[cfg(target_os = "linux")]
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
}
