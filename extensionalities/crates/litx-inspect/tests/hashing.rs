// Module: glassbox-inspect public hashing integration tests.
// File: hashing.rs.
// Responsibility: Verify independent digests, bounded large-file streaming, and no-follow input rejection.

use glassbox_inspect::hashing::{HASH_BUFFER_SIZE, hash_file};
use glassbox_inspect::reports::{Completeness, ConsistencyStatus};
use std::path::{Path, PathBuf};

/// Creates a unique ignored fixture directory for one hashing test.
///
/// Accepts a test label. Returns a new target path with private permissions.
/// Creation failures panic; no existing evidence or user data is removed.
fn workdir(label: &str) -> PathBuf {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/tests")
        .join(format!(
            "public-hashing-{label}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
    std::fs::create_dir_all(&path).unwrap();
    path
}

/// Verifies empty and binary files against independent SHA-256 constants.
///
/// Accepts no parameters. Returns unit after hashing two fixtures. Unexpected
/// identity, byte count, digest, or consistency panics; fixture writes can also panic.
#[test]
fn empty_and_binary_digests_match_independent_oracles() {
    let directory = workdir("oracles");
    let empty = directory.join("empty.bin");
    let binary = directory.join("binary.bin");
    std::fs::write(&empty, []).unwrap();
    std::fs::write(&binary, b"abc").unwrap();
    let empty = hash_file(&empty);
    assert_eq!(
        empty.digest.digest.as_deref(),
        Some("e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")
    );
    assert_eq!(empty.digest.bytes_hashed.value, Some(0));
    assert_eq!(empty.digest.bytes_hashed.completeness, Completeness::Exact);
    let binary = hash_file(&binary);
    assert_eq!(
        binary.digest.digest.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert_eq!(binary.digest.bytes_hashed.value, Some(3));
    assert_eq!(binary.digest.consistency, ConsistencyStatus::Consistent);
}

/// Verifies a multi-buffer file streams completely with a fixed 64 KiB buffer.
///
/// Accepts no parameters. Returns unit after hashing two MiB plus a tail.
/// Missing/truncated hashes, changed identity, or a changed buffer contract panic.
#[test]
fn large_file_hashes_all_bytes_with_bounded_buffer() {
    let directory = workdir("large");
    let path = directory.join("large.bin");
    let length = HASH_BUFFER_SIZE * 32 + 17;
    let bytes: Vec<u8> = (0..length).map(|index| (index % 251) as u8).collect();
    std::fs::write(&path, bytes).unwrap();
    let outcome = hash_file(&path);
    assert_eq!(HASH_BUFFER_SIZE, 64 * 1024);
    assert_eq!(outcome.digest.bytes_hashed.value, Some(length as u64));
    assert_eq!(
        outcome.digest.bytes_hashed.completeness,
        Completeness::Exact
    );
    assert_eq!(outcome.digest.consistency, ConsistencyStatus::Consistent);
    assert_eq!(outcome.digest.digest.unwrap().len(), 64);
    assert_eq!(outcome.identity.byte_size, Some(length as u64));
}

/// Verifies final and ancestor symlinks are rejected instead of followed.
///
/// Accepts no parameters. Returns unit after direct and nested link attempts.
/// Any trusted digest or successful linked inspection panics; fixture I/O can panic.
#[cfg(unix)]
#[test]
fn symlink_files_and_components_are_rejected() {
    let directory = workdir("links");
    let target = directory.join("target.txt");
    let link = directory.join("link.txt");
    let real = directory.join("real");
    let directory_link = directory.join("directory-link");
    std::fs::write(&target, b"secret").unwrap();
    std::fs::create_dir(&real).unwrap();
    std::fs::write(real.join("nested.txt"), b"nested").unwrap();
    std::os::unix::fs::symlink(&target, &link).unwrap();
    std::os::unix::fs::symlink(&real, &directory_link).unwrap();
    for path in [link, directory_link.join("nested.txt")] {
        let outcome = hash_file(&path);
        assert!(!outcome.input_valid);
        assert!(outcome.digest.digest.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "input_symlink_rejected"
                    || diagnostic.code == "input_open_failed")
        );
    }
}

/// Verifies directories fail as non-regular inputs without a hashing read.
///
/// Accepts no parameters. Returns unit after one directory inspection attempt.
/// A valid digest or input acceptance panics; no recursive traversal occurs.
#[test]
fn directory_is_not_a_regular_hash_input() {
    let directory = workdir("directory");
    let outcome = hash_file(&directory);
    assert!(!outcome.input_valid);
    assert!(outcome.digest.digest.is_none());
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "input_not_regular_file")
    );
}
