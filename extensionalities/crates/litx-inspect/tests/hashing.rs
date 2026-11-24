//! Module: glassbox-inspect public hashing integration tests.
//! File: hashing.rs.
//! Responsibility: Verify independent SHA-256 parity, sampled fingerprint framing and threshold boundaries, concurrent-mutation withholding, and no-follow input rejection.

use glassbox_inspect::hashing::{
    DEFAULT_FULL_HASH_MAX_BYTES, HASH_BUFFER_SIZE, HashPolicy, SAMPLED_FINGERPRINT_SCHEME,
    SAMPLED_WINDOW_BYTES, hash_file, hash_file_with_policy,
};
use glassbox_inspect::reports::{Completeness, ConsistencyStatus, DiagnosticSeverity};
use sha2::{Digest, Sha256};
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Barrier,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

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

/// Computes an independent reference `glassbox-sampled-sha256/v1` hex digest from
/// the file length and the exact 1 MiB head, middle, and tail windows.
///
/// Accepts total byte length and the three 1 MiB byte slices. Returns a 64-char
/// lowercase hex digest matching `design.md §4.2`. Panics if any slice is not
/// exactly `SAMPLED_WINDOW_BYTES` long.
fn reference_sampled_fingerprint(
    total_bytes: u64,
    head: &[u8],
    middle: &[u8],
    tail: &[u8],
) -> String {
    assert_eq!(head.len() as u64, SAMPLED_WINDOW_BYTES);
    assert_eq!(middle.len() as u64, SAMPLED_WINDOW_BYTES);
    assert_eq!(tail.len() as u64, SAMPLED_WINDOW_BYTES);
    let tail_offset = total_bytes - SAMPLED_WINDOW_BYTES;
    let middle_offset = tail_offset / 2;
    let mut hasher = Sha256::new();
    hasher.update(b"glassbox-sampled-sha256/v1");
    hasher.update([0x00_u8]);
    hasher.update(total_bytes.to_be_bytes());
    for (offset, window) in [(0_u64, head), (middle_offset, middle), (tail_offset, tail)] {
        hasher.update(offset.to_be_bytes());
        hasher.update(SAMPLED_WINDOW_BYTES.to_be_bytes());
        hasher.update(window);
    }
    format!("{:x}", hasher.finalize())
}

/// Writes a sparse file of `total_bytes` length with explicit 1 MiB head, middle,
/// and tail payloads at the exact `glassbox-sampled-sha256/v1` offsets.
///
/// Accepts target file path, total byte length, and the three 1 MiB byte slices.
/// Returns unit after syncing data. Filesystem errors panic.
fn write_sparse_with_windows(
    path: &Path,
    total_bytes: u64,
    head: &[u8],
    middle: &[u8],
    tail: &[u8],
) {
    let tail_offset = total_bytes - SAMPLED_WINDOW_BYTES;
    let middle_offset = tail_offset / 2;
    let mut file = std::fs::File::create(path).unwrap();
    file.set_len(total_bytes).unwrap();
    for (offset, window) in [(0_u64, head), (middle_offset, middle), (tail_offset, tail)] {
        file.seek(SeekFrom::Start(offset)).unwrap();
        file.write_all(window).unwrap();
    }
    file.sync_data().unwrap();
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
    assert!(empty.digest.fingerprint.is_none());
    assert_eq!(empty.digest.bytes_hashed.value, Some(0));
    assert_eq!(empty.digest.bytes_hashed.completeness, Completeness::Exact);
    let binary = hash_file(&binary);
    assert_eq!(
        binary.digest.digest.as_deref(),
        Some("ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad")
    );
    assert!(binary.digest.fingerprint.is_none());
    assert_eq!(binary.digest.bytes_hashed.value, Some(3));
    assert_eq!(binary.digest.consistency, ConsistencyStatus::Consistent);
    std::fs::remove_dir_all(&directory).ok();
}

/// Verifies a multi-buffer file streams completely with a fixed 64 KiB buffer
/// and matches system `sha256sum` when installed.
///
/// Accepts no parameters. Returns unit after hashing two MiB plus a tail.
/// Missing/truncated hashes, changed identity, `sha256sum` disagreement, or a
/// changed buffer contract panic.
#[test]
fn large_file_hashes_all_bytes_with_bounded_buffer_and_sha256sum_parity() {
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
    assert!(outcome.digest.fingerprint.is_none());
    assert!(
        !outcome
            .diagnostics
            .iter()
            .any(|d| d.code == "fingerprint_sampled")
    );
    let computed = outcome.digest.digest.expect("full sha256 expected");
    assert_eq!(computed.len(), 64);
    assert_eq!(outcome.identity.byte_size, Some(length as u64));
    if let Ok(output) = std::process::Command::new("sha256sum").arg(&path).output() {
        assert!(output.status.success(), "sha256sum failed on test fixture");
        let stdout = String::from_utf8(output.stdout).unwrap();
        let expected = stdout.split_whitespace().next().unwrap();
        assert_eq!(computed, expected);
    }
    std::fs::remove_dir_all(&directory).ok();
}

/// Verifies exact threshold behavior (`T` vs `T + 1`), sampled latency budget,
/// and `HashPolicy::Full` override on real `DEFAULT_FULL_HASH_MAX_BYTES` files.
///
/// Accepts no parameters. Returns unit after testing `T` under default policy,
/// `T + 1` under default policy (asserting `< 50 ms` completion and
/// `fingerprint_sampled` diagnostic), and `T + 1` under `HashPolicy::Full`.
/// Unexpected digests, missing diagnostics, or latency violations panic.
#[test]
fn threshold_boundary_and_full_hash_policy_override() {
    assert_eq!(DEFAULT_FULL_HASH_MAX_BYTES, 128 * 1024 * 1024);
    let directory = workdir("threshold-boundary");
    let at_threshold = directory.join("at-threshold.bin");
    let over_threshold = directory.join("over-threshold.bin");
    let head = vec![0x31_u8; SAMPLED_WINDOW_BYTES as usize];
    let mid = vec![0x52_u8; SAMPLED_WINDOW_BYTES as usize];
    let tail = vec![0x73_u8; SAMPLED_WINDOW_BYTES as usize];

    write_sparse_with_windows(
        &at_threshold,
        DEFAULT_FULL_HASH_MAX_BYTES,
        &head,
        &mid,
        &tail,
    );
    write_sparse_with_windows(
        &over_threshold,
        DEFAULT_FULL_HASH_MAX_BYTES + 1,
        &head,
        &mid,
        &tail,
    );

    // bytes == T -> full SHA-256, no fingerprint, no fingerprint_sampled diagnostic.
    #[cfg(debug_assertions)]
    let full_start = Instant::now();
    let exact = hash_file(&at_threshold);
    #[cfg(debug_assertions)]
    let full_elapsed = full_start.elapsed();
    assert!(exact.input_valid);
    assert!(exact.digest.digest.is_some());
    assert!(exact.digest.fingerprint.is_none());
    assert_eq!(
        exact.digest.bytes_hashed.value,
        Some(DEFAULT_FULL_HASH_MAX_BYTES)
    );
    assert_eq!(exact.digest.bytes_hashed.completeness, Completeness::Exact);
    assert_eq!(exact.digest.consistency, ConsistencyStatus::Consistent);
    assert!(
        !exact
            .diagnostics
            .iter()
            .any(|d| d.code == "fingerprint_sampled")
    );

    // bytes == T + 1 -> sampled fingerprint in milliseconds, no sha256, info diagnostic.
    let start = Instant::now();
    let sampled = hash_file(&over_threshold);
    let elapsed = start.elapsed();
    assert!(sampled.input_valid);
    assert!(sampled.digest.digest.is_none());
    let fingerprint = sampled
        .digest
        .fingerprint
        .as_ref()
        .expect("sampled fingerprint expected above threshold");
    assert_eq!(fingerprint.scheme, SAMPLED_FINGERPRINT_SCHEME);
    assert_eq!(
        fingerprint.value,
        reference_sampled_fingerprint(DEFAULT_FULL_HASH_MAX_BYTES + 1, &head, &mid, &tail)
    );
    assert_eq!(
        sampled.digest.bytes_hashed.value,
        Some(3 * SAMPLED_WINDOW_BYTES)
    );
    assert_eq!(
        sampled.digest.bytes_hashed.completeness,
        Completeness::Partial
    );
    assert_eq!(sampled.digest.consistency, ConsistencyStatus::Consistent);
    assert!(sampled.diagnostics.iter().any(|d| {
        d.code == "fingerprint_sampled"
            && d.severity == DiagnosticSeverity::Info
            && d.message == "change detection used size and modification time only."
    }));
    // Release builds enforce the strict < 50 ms budget; unoptimized debug builds
    // enforce both a bounded wall-clock cap and >10x speedup over full hashing at T.
    #[cfg(not(debug_assertions))]
    assert!(
        elapsed < Duration::from_millis(50),
        "sampled fingerprint on {} bytes took {:?}, exceeding 50 ms release budget",
        DEFAULT_FULL_HASH_MAX_BYTES + 1,
        elapsed
    );
    #[cfg(debug_assertions)]
    assert!(
        elapsed < Duration::from_millis(500) && elapsed.saturating_mul(10) < full_elapsed,
        "sampled fingerprint in debug build took {:?} vs full hash {:?} at {}",
        elapsed,
        full_elapsed,
        DEFAULT_FULL_HASH_MAX_BYTES
    );

    // HashPolicy::Full on T + 1 -> full SHA-256, no fingerprint, no fingerprint_sampled.
    let full = hash_file_with_policy(&over_threshold, HashPolicy::Full);
    assert!(full.input_valid);
    assert!(full.digest.digest.is_some());
    assert!(full.digest.fingerprint.is_none());
    assert_eq!(
        full.digest.bytes_hashed.value,
        Some(DEFAULT_FULL_HASH_MAX_BYTES + 1)
    );
    assert_eq!(full.digest.bytes_hashed.completeness, Completeness::Exact);
    assert_eq!(full.digest.consistency, ConsistencyStatus::Consistent);
    assert!(
        !full
            .diagnostics
            .iter()
            .any(|d| d.code == "fingerprint_sampled")
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Verifies domain separation and exact `glassbox-sampled-sha256/v1` framing across
/// distinct file lengths that share identical 1 MiB head, middle, and tail windows.
///
/// Accepts no parameters. Returns unit after comparing `T + 1` and `T + 4096`
/// sparse fixtures against each other and against the independent reference hasher.
/// Collisions or framing mismatches panic.
#[test]
fn sampled_fingerprint_domain_separation_and_reference_framing() {
    let directory = workdir("domain-separation");
    let len_a = DEFAULT_FULL_HASH_MAX_BYTES + 1;
    let len_b = DEFAULT_FULL_HASH_MAX_BYTES + 4096;
    let file_a = directory.join("len-a.bin");
    let file_b = directory.join("len-b.bin");

    let head: Vec<u8> = (0..SAMPLED_WINDOW_BYTES as usize)
        .map(|i| ((i * 13 + 7) % 251) as u8)
        .collect();
    let mid: Vec<u8> = (0..SAMPLED_WINDOW_BYTES as usize)
        .map(|i| ((i * 29 + 19) % 251) as u8)
        .collect();
    let tail: Vec<u8> = (0..SAMPLED_WINDOW_BYTES as usize)
        .map(|i| ((i * 43 + 31) % 251) as u8)
        .collect();

    write_sparse_with_windows(&file_a, len_a, &head, &mid, &tail);
    write_sparse_with_windows(&file_b, len_b, &head, &mid, &tail);

    let out_a = hash_file(&file_a);
    let out_b = hash_file(&file_b);

    let fp_a = out_a
        .digest
        .fingerprint
        .expect("file_a should emit sampled fingerprint");
    let fp_b = out_b
        .digest
        .fingerprint
        .expect("file_b should emit sampled fingerprint");

    assert_eq!(fp_a.scheme, SAMPLED_FINGERPRINT_SCHEME);
    assert_eq!(fp_b.scheme, SAMPLED_FINGERPRINT_SCHEME);
    assert_eq!(
        fp_a.value,
        reference_sampled_fingerprint(len_a, &head, &mid, &tail)
    );
    assert_eq!(
        fp_b.value,
        reference_sampled_fingerprint(len_b, &head, &mid, &tail)
    );
    assert_ne!(
        fp_a.value, fp_b.value,
        "distinct file lengths with identical 1 MiB windows must produce distinct fingerprints"
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Verifies concurrent mutation during hashing withholds both digest and fingerprint
/// and emits `source_changed_during_hash`.
///
/// Accepts no parameters. Returns unit after mutating a file while `hash_file_with_policy`
/// streams it. Retaining a digest or fingerprint after mutation panics.
#[test]
fn concurrent_mutation_withholds_digest_and_fingerprint() {
    let directory = workdir("concurrent-mutation");
    let path = directory.join("mutating.bin");
    let initial_len = 64 * 1024 * 1024_u64;
    let mut observed_changed = false;

    for _attempt in 0..5 {
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(initial_len).unwrap();
        file.sync_data().unwrap();
        drop(file);

        let barrier = Arc::new(Barrier::new(2));
        let done = Arc::new(AtomicBool::new(false));
        let writer_barrier = Arc::clone(&barrier);
        let writer_done = Arc::clone(&done);
        let writer_path = path.clone();

        let writer = std::thread::spawn(move || {
            writer_barrier.wait();
            std::thread::sleep(Duration::from_millis(5));
            while !writer_done.load(Ordering::Relaxed) {
                if let Ok(mut f) = std::fs::OpenOptions::new().append(true).open(&writer_path) {
                    let _ = f.write_all(b"x");
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });

        barrier.wait();
        let outcome = hash_file_with_policy(&path, HashPolicy::Full);
        done.store(true, Ordering::Relaxed);
        writer.join().unwrap();

        if outcome.identity.consistency == ConsistencyStatus::SourceChanged {
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
            observed_changed = true;
            break;
        }
    }

    assert!(
        observed_changed,
        "expected concurrent writer to trigger source_changed_during_hash"
    );
    std::fs::remove_dir_all(&directory).ok();
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
        assert!(outcome.digest.fingerprint.is_none());
        assert!(
            outcome
                .diagnostics
                .iter()
                .any(|diagnostic| diagnostic.code == "input_symlink_rejected"
                    || diagnostic.code == "input_open_failed")
        );
    }
    // GB-AUD-15: `symlink_dir/../target.txt` must be rejected BEFORE lexical `..` folding.
    let dotdot_escape = directory_link.join("..").join("target.txt");
    let escaped = hash_file(&dotdot_escape);
    assert!(!escaped.input_valid);
    assert!(escaped.digest.digest.is_none());
    assert!(escaped.digest.fingerprint.is_none());
    assert!(
        escaped
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "input_symlink_rejected"),
        "expected input_symlink_rejected for symlink_dir/../target.txt, got: {:?}",
        escaped.diagnostics
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Verifies non-symlink `symlink_metadata` errors (such as traversing through a
/// regular file `file.txt/child`) report `input_open_failed` rather than
/// `input_symlink_rejected`.
///
/// Accepts no parameters. Returns unit after creating a regular file and
/// hashing a nested child path beneath it. Misclassified diagnostics or a
/// retained digest panic; fixture I/O failures can also panic.
#[test]
fn non_symlink_metadata_error_reports_input_open_failed() {
    let directory = workdir("non-symlink-enotdir");
    let regular = directory.join("file.txt");
    std::fs::write(&regular, b"regular-file").unwrap();
    let child = regular.join("child");

    let outcome = hash_file(&child);
    std::fs::remove_dir_all(&directory).ok();

    assert!(!outcome.input_valid);
    assert!(outcome.digest.digest.is_none());
    assert!(outcome.digest.fingerprint.is_none());
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "input_open_failed"),
        "expected input_open_failed for file.txt/child, got: {:?}",
        outcome.diagnostics
    );
    assert!(
        !outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "input_symlink_rejected"),
        "expected no input_symlink_rejected for file.txt/child, got: {:?}",
        outcome.diagnostics
    );
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
    assert!(outcome.digest.fingerprint.is_none());
    assert!(
        outcome
            .diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == "input_not_regular_file")
    );
    std::fs::remove_dir_all(&directory).ok();
}

/// Calibration benchmark exercising the production `HashPolicy::Full` streaming
/// SHA-256 path over a 256 MiB fixture passed via `GLASSBOX_HASH_BENCH_FILE`.
///
/// Accepts no parameters; requires `GLASSBOX_HASH_BENCH_FILE` pointing to an
/// existing 256 MiB regular file. Runs 1 warmup + 5 warm passes, verifies
/// identical hex digests across passes, and prints JSON benchmark timings.
/// Panics if the environment variable is unset or any pass fails.
#[test]
#[ignore = "Explicit release-mode 256 MiB calibration gate (V-IO-01)"]
fn benchmark_256_mib_full_sha256_gate() {
    let path = PathBuf::from(
        std::env::var("GLASSBOX_HASH_BENCH_FILE")
            .expect("set GLASSBOX_HASH_BENCH_FILE to the 256 MiB benchmark file"),
    );
    let warmup = hash_file_with_policy(&path, HashPolicy::Full);
    assert!(warmup.input_valid);
    let expected_hex = warmup
        .digest
        .digest
        .expect("warmup full SHA-256 must succeed");
    assert_eq!(warmup.digest.bytes_hashed.value, Some(256 * 1024 * 1024));

    let mut passes_ms = Vec::with_capacity(5);
    for _ in 0..5 {
        let start = Instant::now();
        let outcome = hash_file_with_policy(&path, HashPolicy::Full);
        let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;
        assert!(outcome.input_valid);
        assert_eq!(
            outcome.digest.digest.as_deref(),
            Some(expected_hex.as_str())
        );
        assert_eq!(outcome.digest.bytes_hashed.value, Some(256 * 1024 * 1024));
        passes_ms.push(elapsed_ms);
    }
    let mut sorted = passes_ms.clone();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let median_ms = sorted[2];
    println!(
        "HASH_BENCH_JSON={{\"bytes\":268435456,\"sha256\":\"{}\",\"passes_ms\":[{:.3},{:.3},{:.3},{:.3},{:.3}],\"median_ms\":{:.3}}}",
        expected_hex,
        passes_ms[0],
        passes_ms[1],
        passes_ms[2],
        passes_ms[3],
        passes_ms[4],
        median_ms
    );
}
