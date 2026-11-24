//! Module: glassbox-cli bounded batch execution.
//! File: batch.rs.
//! Responsibility: Share file/OCR slots, preserve request order, apply backpressure, and join cancellation.

use glassbox_inspect::SharedWorkBudget;
use glassbox_inspect::reports::OperationStatus;
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

const MAX_REORDER_BYTES_PER_JOB: usize = 4 << 20;

/// Longest sleep between reservation wake rechecks.
///
/// A blocked reservation is woken by the coordinator after every cancellation,
/// and this timeout is only the residual backstop for a missed notification.
const RESERVATION_POLL: Duration = Duration::from_millis(50);

#[cfg(unix)]
static SIGINT_RECEIVED: AtomicBool = AtomicBool::new(false);

/// Installs the process's minimal SIGINT flag handler for live batch cancellation.
///
/// Accepts no parameters. Returns unit after installing one atomic-only handler
/// and clearing stale observations. Platform handler-installation errors are not
/// exposed by C `signal`; ordinary channel cancellation still remains available.
#[cfg(unix)]
fn install_sigint_handler() {
    extern "C" fn mark_sigint(_signal: i32) {
        SIGINT_RECEIVED.store(true, Ordering::Release);
    }
    unsafe extern "C" {
        fn signal(signal: i32, handler: extern "C" fn(i32)) -> usize;
    }
    SIGINT_RECEIVED.store(false, Ordering::Release);
    // SAFETY: SIGINT is a valid signal and `mark_sigint` has C handler ABI,
    // static lifetime, and performs only an atomic store with no allocation.
    let _ = unsafe { signal(2, mark_sigint) };
}

/// Reports whether the actual process received SIGINT since batch startup.
///
/// Accepts no parameters. Returns the atomic signal observation. Non-Unix
/// platforms return false; no signal, FFI, allocation, or blocking occurs there.
fn sigint_received() -> bool {
    #[cfg(unix)]
    {
        SIGINT_RECEIVED.load(Ordering::Acquire)
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Aggregate batch outcome with invocation errors taking 64-over-1-over-2 precedence.
pub(crate) struct BatchOutcome {
    /// Complete, partial, or failed status across all per-file operations.
    pub(crate) status: OperationStatus,
    /// Whether any malformed or oversized JSONL request was invalid.
    pub(crate) invalid: bool,
    /// Whether any admitted request was unfinished when cancellation began.
    pub(crate) cancelled: bool,
}

/// One path request with its original external ID and optional inventory identity.
pub(crate) struct BatchRequest {
    /// Admission sequence used for ordered output and unique artifact names.
    pub(crate) sequence: u64,
    /// Caller-owned JSON ID, including duplicates and explicit null.
    pub(crate) id: Value,
    /// Explicit regular-file path to inspect or extract.
    pub(crate) path: PathBuf,
    /// Optional anchored `dev:ino` identity captured during directory enumeration.
    pub(crate) expected: Option<String>,
    /// Optional opened regular-file handle pinned until inspection and identity check finish.
    pub(crate) pinned_file: Option<std::fs::File>,
}

/// One completed or immediately rejected request ready for ordered serialization.
pub(crate) struct BatchCompletion {
    response: Value,
    status: OperationStatus,
    invalid: bool,
    cancelled: bool,
}

impl BatchCompletion {
    /// Wraps one canonical schema v3 report while retaining its external identity.
    ///
    /// Accepts the sequenced request, aggregate item status, and serializable
    /// report value. Returns a completion with no invocation/cancellation error.
    pub(crate) fn report(
        request: BatchRequest,
        status: OperationStatus,
        report: Result<Value, serde_json::Error>,
    ) -> Self {
        match report {
            Ok(report) => {
                let response = serde_json::json!({
                    "id": request.id,
                    "path": request.path,
                    "status": status,
                    "report": report,
                });
                Self {
                    response,
                    status,
                    invalid: false,
                    cancelled: false,
                }
            }
            Err(error) => Self::error(
                request.id,
                &request.path,
                OperationStatus::Failed,
                "serialization",
                "report_serialization_failed",
                error,
            ),
        }
    }

    /// Creates an invalid-request response without opening the submitted path.
    ///
    /// Accepts an optional external ID and parser/line-cap diagnostic. Returns
    /// a completion whose invalid flag produces aggregate exit 64 after ordering.
    pub(crate) fn invalid(id: Value, message: impl ToString) -> Self {
        Self {
            response: serde_json::json!({
                "id": id,
                "status": "invalid",
                "error": {"stage": "batch", "message": message.to_string()},
            }),
            status: OperationStatus::Failed,
            invalid: true,
            cancelled: false,
        }
    }

    /// Creates one deterministic traversal or immediate operational response.
    ///
    /// Accepts external ID, path, report/error status, stable stage/code, and
    /// message. Returns a non-invalid completion with a structured error object.
    pub(crate) fn error(
        id: Value,
        path: &Path,
        status: OperationStatus,
        stage: &str,
        code: &str,
        message: impl ToString,
    ) -> Self {
        Self {
            response: serde_json::json!({
                "id": id,
                "path": path.to_string_lossy(),
                "status": status,
                "error": {"stage": stage, "code": code, "message": message.to_string()},
            }),
            status,
            invalid: false,
            cancelled: false,
        }
    }

    /// Marks an admitted but never-started request as explicitly cancelled.
    ///
    /// Accepts the sequenced request. Returns a failed non-invalid completion
    /// with its ID/path preserved and no fabricated metadata or content report.
    fn cancelled(request: BatchRequest) -> Self {
        Self {
            response: serde_json::json!({
                "id": request.id,
                "path": request.path,
                "status": "cancelled",
                "cancelled": true,
                "error": {"stage": "cancellation", "code": "request_not_started",
                          "message": "batch cancelled before this request started"},
            }),
            status: OperationStatus::Failed,
            invalid: false,
            cancelled: true,
        }
    }
}

enum PendingWork {
    Process(BatchRequest),
    Complete(BatchCompletion),
}

struct SequencedWork {
    sequence: u64,
    pending: PendingWork,
}

/// One serialized response whose Value AST was already released.
///
/// Retains only the encoded JSONL bytes and the three aggregate flags, so
/// out-of-order completions hold exactly one copy of their response memory.
struct SerializedCompletion {
    /// Sequence used for ordered emission and reservation release.
    sequence: u64,
    /// Item status folded into the aggregate exit severity.
    status: OperationStatus,
    /// Whether the line was a malformed request demanding exit 64.
    invalid: bool,
    /// Whether the item was cut short by cancellation.
    cancelled: bool,
    /// Encoded JSONL bytes without the trailing newline.
    encoded: Vec<u8>,
    /// Whether the reorder byte reservation must be released after emission.
    reserved: bool,
}

enum BatchEvent {
    Item(SerializedCompletion),
    InputFinished { total: u64, error: Option<String> },
}

/// Cooperative cancellation shared by producer, workers, and output coordinator.
#[derive(Clone)]
pub(crate) struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    /// Reports local or actual SIGINT cancellation without blocking.
    ///
    /// Accepts no parameters. Returns true after either cancellation source.
    /// This atomic-only check has no allocation, I/O, or failure states.
    pub(crate) fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire) || sigint_received()
    }

    /// Stops admission for this batch and wakes cooperative workers.
    ///
    /// Accepts no parameters. Returns unit after setting the shared flag. Active
    /// native work still joins normally; no FFI operation is asynchronously interrupted.
    fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
}

/// Bounded admission handle used by JSONL and directory producers.
pub(crate) struct BatchProducer {
    sequence: u64,
    credits: mpsc::Receiver<()>,
    work: mpsc::SyncSender<SequencedWork>,
    cancellation: CancellationToken,
}

impl BatchProducer {
    /// Clones the cooperative token used to wake a blocking input adapter.
    ///
    /// Accepts no parameters. Returns a token observing local and SIGINT state.
    /// No admission credit, queue slot, file lease, or native resource is acquired.
    pub(crate) fn cancellation_token(&self) -> CancellationToken {
        self.cancellation.clone()
    }

    /// Admits one path request or applies backpressure until a slot is available.
    ///
    /// Accepts external ID, path, and optional inventory identity. Returns true
    /// after bounded queue admission, false after cancellation, or an error if
    /// the worker queue disconnected unexpectedly. No whole-input buffering occurs.
    pub(crate) fn request(
        &mut self,
        id: Value,
        path: PathBuf,
        expected: Option<String>,
    ) -> Result<bool, String> {
        self.request_with_file(id, path, expected, None)
    }

    /// Admits one path request with an optional pinned file handle under backpressure.
    ///
    /// Accepts external ID, path, optional anchored `dev:ino` identity, and an
    /// optional opened regular-file handle kept alive until inspection and
    /// identity verification finish. Returns true after bounded queue
    /// admission, false after cancellation, or an error if the worker queue
    /// disconnected unexpectedly. Backpressure bounds retained handles by jobs.
    pub(crate) fn request_with_file(
        &mut self,
        id: Value,
        path: PathBuf,
        expected: Option<String>,
        pinned_file: Option<std::fs::File>,
    ) -> Result<bool, String> {
        self.submit(PendingWork::Process(BatchRequest {
            sequence: 0,
            id,
            path,
            expected,
            pinned_file,
        }))
    }

    /// Admits an already completed invalid/traversal response in request order.
    ///
    /// Accepts a completion constructed by the producer. Returns true after
    /// admission, false after cancellation, or a disconnected-queue error.
    pub(crate) fn complete(&mut self, completion: BatchCompletion) -> Result<bool, String> {
        self.submit(PendingWork::Complete(completion))
    }

    /// Acquires one outstanding-response credit and assigns the next sequence.
    ///
    /// Accepts pending work. Returns admission/cancellation state or a queue
    /// error. The credit is released only after ordered output, which bounds
    /// slow-head reordering by jobs rather than total input length.
    fn submit(&mut self, pending: PendingWork) -> Result<bool, String> {
        if self.cancellation.is_cancelled() {
            return Ok(false);
        }
        if self.credits.recv().is_err() {
            if self.cancellation.is_cancelled() {
                return Ok(false);
            }
            return Err("batch admission credits disconnected".into());
        }
        if self.cancellation.is_cancelled() {
            return Ok(false);
        }
        self.sequence += 1;
        let work = SequencedWork {
            sequence: self.sequence,
            pending,
        };
        if self.work.send(work).is_err() {
            if self.cancellation.is_cancelled() {
                return Ok(false);
            }
            return Err("batch worker queue disconnected".into());
        }
        Ok(true)
    }
}

#[derive(Default)]
struct ReorderState {
    reserved_bytes: usize,
}

struct ReorderBudget {
    next: AtomicU64,
    capacity: usize,
    state: Mutex<ReorderState>,
    changed: Condvar,
}

impl ReorderBudget {
    /// Creates the byte-capped out-of-order budget for this job count.
    ///
    /// Accepts positive jobs. Returns sequence-one state with a checked N*4MiB
    /// capacity. Overflow saturates at usize::MAX rather than wrapping to zero.
    fn new(jobs: usize) -> Self {
        Self {
            next: AtomicU64::new(1),
            capacity: jobs.saturating_mul(MAX_REORDER_BYTES_PER_JOB),
            state: Mutex::new(ReorderState::default()),
            changed: Condvar::new(),
        }
    }

    /// Reserves serialized bytes only for a response waiting behind a slow head.
    ///
    /// Accepts sequence, encoded size, and cancellation token. Returns whether a
    /// reservation must be released. The next response bypasses reservation and
    /// is written immediately; a later oversized response blocks only while it
    /// is over the shared cap, waking on each release or on cancellation so a
    /// blocked worker can never outlive a cancelled batch.
    fn reserve(&self, sequence: u64, bytes: usize, cancellation: &CancellationToken) -> bool {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        loop {
            if cancellation.is_cancelled() {
                return false;
            }
            if sequence == self.next.load(Ordering::Acquire) {
                return false;
            }
            let over =
                bytes > self.capacity || state.reserved_bytes.saturating_add(bytes) > self.capacity;
            if !over {
                break;
            }
            // Bounded sleep rechecks cancellation even without a coordinator
            // notification; `advance` and `wake` normally interrupt sooner.
            let (guard, _) = self
                .changed
                .wait_timeout(state, RESERVATION_POLL)
                .unwrap_or_else(|error| error.into_inner());
            state = guard;
        }
        state.reserved_bytes = state.reserved_bytes.saturating_add(bytes);
        true
    }

    /// Wakes every blocked reservation without changing byte accounting.
    ///
    /// Accepts no parameters. Returns unit after notification. The coordinator
    /// calls this whenever cancellation starts so workers blocked behind a slow
    /// head observe the stop without waiting for a poll timeout.
    fn wake(&self) {
        self.changed.notify_all();
    }

    /// Advances ordered output and releases any retained-response byte charge.
    ///
    /// Accepts emitted/discarded sequence and reservation flag. Returns unit
    /// after waking blocked result producers. Output failure uses the same path,
    /// preventing full result queues from deadlocking worker cancellation.
    fn advance(&self, sequence: u64) {
        let state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        self.next.store(sequence + 1, Ordering::Release);
        drop(state);
        self.changed.notify_all();
    }

    /// Releases the exact serialized byte reservation after ordered handling.
    ///
    /// Accepts encoded byte count and reservation flag. Returns unit after
    /// saturating subtraction and notification. Sequence advancement is kept in
    /// `advance`; this helper is folded into that method by the coordinator.
    fn release_bytes(&self, bytes: usize, reserved: bool) {
        if !reserved {
            return;
        }
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        state.reserved_bytes = state.reserved_bytes.saturating_sub(bytes);
        drop(state);
        self.changed.notify_all();
    }
}

/// Combines item statuses without hiding an operational failure behind partials.
///
/// Accepts aggregate and item status. Returns failed for failed/unknown, partial
/// for partial/unsupported, otherwise complete. No I/O or failure states occur.
fn combine_status(current: OperationStatus, item: OperationStatus) -> OperationStatus {
    use OperationStatus::{Complete, Failed, Partial, Unknown, Unsupported};
    match (current, item) {
        (Failed | Unknown, _) | (_, Failed | Unknown) => Failed,
        (Partial | Unsupported, _) | (_, Partial | Unsupported) => Partial,
        _ => Complete,
    }
}

/// Runs bounded ordered workers and joins producer/OCR work on every exit path.
///
/// Accepts positive jobs, a producer closure, per-worker factory, and JSONL
/// writer. Returns aggregate item status after EOF/cancellation and thread join.
/// Input, serialization, and stdout failures return Err after queue cancellation
/// and draining; malformed requests remain per-item responses and do not stop valid work.
pub(crate) fn run<R, F, W>(
    jobs: usize,
    reader: R,
    worker_factory: F,
    output: &mut dyn Write,
) -> Result<BatchOutcome, String>
where
    R: FnOnce(&mut BatchProducer) -> Result<(), String> + Send,
    F: Fn(SharedWorkBudget, CancellationToken) -> W + Send + Sync,
    W: FnMut(BatchRequest) -> BatchCompletion + Send,
{
    install_sigint_handler();
    let budget = SharedWorkBudget::new(jobs)?;
    let cancellation = CancellationToken(Arc::new(AtomicBool::new(false)));
    let (credit_tx, credit_rx) = mpsc::sync_channel::<()>(jobs);
    for _ in 0..jobs {
        credit_tx.send(()).map_err(|error| error.to_string())?;
    }
    let (work_tx, work_rx) = mpsc::sync_channel::<SequencedWork>(jobs);
    let work_rx = Arc::new(Mutex::new(work_rx));
    let (event_tx, event_rx) = mpsc::sync_channel::<BatchEvent>(jobs);
    let reorder = ReorderBudget::new(jobs);
    let mut credit_tx = Some(credit_tx);

    std::thread::scope(|scope| {
        let reader_events = event_tx.clone();
        let reader_cancellation = cancellation.clone();
        scope.spawn(move || {
            let mut producer = BatchProducer {
                sequence: 0,
                credits: credit_rx,
                work: work_tx,
                cancellation: reader_cancellation,
            };
            let error = reader(&mut producer).err();
            let total = producer.sequence;
            drop(producer);
            let _ = reader_events.send(BatchEvent::InputFinished { total, error });
        });

        for _ in 0..jobs {
            let work_rx = Arc::clone(&work_rx);
            let events = event_tx.clone();
            let worker_budget = budget.clone();
            let worker_cancellation = cancellation.clone();
            let reorder = &reorder;
            let factory = &worker_factory;
            scope.spawn(move || {
                let mut worker = factory(worker_budget.clone(), worker_cancellation.clone());
                loop {
                    let received = {
                        let receiver = work_rx.lock().unwrap_or_else(|error| error.into_inner());
                        receiver.recv()
                    };
                    let Ok(work) = received else {
                        break;
                    };
                    if worker_cancellation.is_cancelled() {
                        worker_cancellation.cancel();
                        worker_budget.cancel();
                        reorder.wake();
                    }
                    let completion = match work.pending {
                        PendingWork::Process(request) if worker_cancellation.is_cancelled() => {
                            BatchCompletion::cancelled(request)
                        }
                        PendingWork::Process(request) => {
                            let sequence = work.sequence;
                            let request = BatchRequest {
                                sequence,
                                ..request
                            };
                            let mut completion = worker(request);
                            if worker_cancellation.is_cancelled() {
                                completion.cancelled = true;
                                completion.response["cancelled"] = Value::Bool(true);
                            }
                            completion
                        }
                        PendingWork::Complete(completion) => completion,
                    };
                    let mut completion = completion;
                    completion.response["sequence"] = Value::from(work.sequence);
                    let mut encoded = match serde_json::to_vec(&completion.response) {
                        Ok(encoded) => encoded,
                        Err(error) => {
                            completion.status = OperationStatus::Failed;
                            let fallback = serde_json::json!({
                                "sequence": work.sequence,
                                "id": completion.response.get("id"),
                                "status": "failed",
                                "error": {"stage": "serialization", "message": error.to_string()},
                            });
                            serde_json::to_vec(&fallback)
                                .unwrap_or_else(|_| b"{\"status\":\"failed\"}\n".to_vec())
                        }
                    };
                    let reserved =
                        reorder.reserve(work.sequence, encoded.len(), &worker_cancellation);
                    let item = SerializedCompletion {
                        sequence: work.sequence,
                        status: completion.status,
                        invalid: completion.invalid,
                        cancelled: completion.cancelled,
                        encoded: std::mem::take(&mut encoded),
                        reserved,
                    };
                    // The response Value AST is released here, so each completion
                    // retains only its encoded bytes and aggregate flags.
                    drop(completion);
                    if events.send(BatchEvent::Item(item)).is_err() {
                        break;
                    }
                }
            });
        }
        drop(event_tx);

        let mut outcome = BatchOutcome {
            status: OperationStatus::Complete,
            invalid: false,
            cancelled: false,
        };
        let mut pending = BTreeMap::<u64, SerializedCompletion>::new();
        let mut next = 1u64;
        let mut input_total = None;
        let mut input_error = None;
        let mut output_error = None;

        while input_total.is_none_or(|total| next <= total) {
            let event = match event_rx.recv() {
                Ok(event) => event,
                Err(_) => {
                    input_error.get_or_insert_with(|| "batch event queue disconnected".into());
                    break;
                }
            };
            if cancellation.is_cancelled() {
                cancellation.cancel();
                budget.cancel();
                credit_tx.take();
                reorder.wake();
            }
            match event {
                BatchEvent::InputFinished { total, error } => {
                    input_total = Some(total);
                    input_error = error;
                }
                BatchEvent::Item(item) => {
                    pending.insert(item.sequence, item);
                    while let Some(item) = pending.remove(&next) {
                        outcome.invalid |= item.invalid;
                        outcome.status = combine_status(outcome.status, item.status);
                        outcome.cancelled |= item.cancelled;
                        if output_error.is_none() {
                            let write = output
                                .write_all(&item.encoded)
                                .and_then(|_| output.write_all(b"\n"))
                                .and_then(|_| output.flush());
                            if let Err(error) = write {
                                output_error = Some(error.to_string());
                                cancellation.cancel();
                                budget.cancel();
                                credit_tx.take();
                                reorder.wake();
                            }
                        }
                        reorder.release_bytes(item.encoded.len(), item.reserved);
                        reorder.advance(next);
                        if let Some(credits) = &credit_tx {
                            let _ = credits.send(());
                        }
                        next += 1;
                    }
                }
            }
        }
        if cancellation.is_cancelled() {
            outcome.status = OperationStatus::Failed;
        }
        credit_tx.take();
        if cancellation.is_cancelled() {
            budget.cancel();
        }
        if let Some(error) = input_error.or(output_error) {
            Err(error)
        } else {
            Ok(outcome)
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies slow-head reservations never charge more than the byte cap.
    ///
    /// Accepts no parameters. Returns unit after checking exact arithmetic and
    /// oversized-next bypass. Assertion failures identify bound regressions.
    #[test]
    fn reorder_capacity_is_checked_and_head_can_advance() {
        let budget = ReorderBudget::new(1);
        assert_eq!(budget.capacity, MAX_REORDER_BYTES_PER_JOB);
        assert!(!budget.reserve(
            1,
            usize::MAX,
            &CancellationToken(Arc::new(AtomicBool::new(false)))
        ));
    }

    /// Verifies an over-budget reservation stops waiting once cancelled.
    ///
    /// Accepts no parameters. Returns unit after cancelling a token while one
    /// reservation for a later, oversized response blocks behind a slow head.
    /// A hang or an unbounded wait fails the elapsed bound; a charged
    /// reservation (true result) fails the bypass assertion.
    #[test]
    fn blocked_reservation_wakes_and_bypasses_on_cancellation() {
        let budget = ReorderBudget::new(1);
        let token = CancellationToken(Arc::new(AtomicBool::new(false)));
        let stopper = token.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            stopper.cancel();
        });
        let started = std::time::Instant::now();
        let reserved = budget.reserve(2, MAX_REORDER_BYTES_PER_JOB + 1, &token);
        assert!(!reserved, "a cancelled reservation must never charge bytes");
        assert!(started.elapsed() < Duration::from_secs(2));
    }

    /// Verifies aggregate status keeps failed over partial and partial over complete.
    ///
    /// Accepts no parameters. Returns unit after checking the full precedence
    /// lattice. Assertion failures identify exit-severity regressions.
    #[test]
    fn status_severity_precedence_is_preserved() {
        use OperationStatus::{Complete, Failed, Partial, Unsupported};
        assert_eq!(combine_status(Complete, Failed), Failed);
        assert_eq!(combine_status(Failed, Complete), Failed);
        assert_eq!(combine_status(Complete, Partial), Partial);
        assert_eq!(combine_status(Unsupported, Complete), Partial);
        assert_eq!(combine_status(Partial, Unsupported), Partial);
        assert_eq!(combine_status(Complete, Complete), Complete);
    }

    /// Verifies `request_with_file` delivers an opened file handle to the worker.
    ///
    /// Accepts no parameters. Returns unit after submitting one pinned scratch
    /// file and one unpinned request through `batch::run`. A missing handle,
    /// changed inode, or non-complete status fails the assertion; the scratch
    /// directory is removed on completion.
    #[test]
    fn request_with_file_pins_handle_until_worker_consumes_request() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/output-tests");
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the UNIX epoch")
            .as_nanos();
        let directory = root.join(format!("batch-pinned-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&directory).expect("scratch directory created");
        let file_path = directory.join("pinned.txt");
        std::fs::write(&file_path, b"pinned-body").expect("scratch file created");
        let handle = std::fs::File::open(&file_path).expect("scratch file opened");
        #[cfg(unix)]
        let expected_id = {
            use std::os::unix::fs::MetadataExt;
            let meta = handle.metadata().expect("metadata read");
            format!("{:x}:{:x}", meta.dev(), meta.ino())
        };
        #[cfg(not(unix))]
        let expected_id = String::new();

        let mut out = Vec::new();
        let path_for_producer = file_path.clone();
        let expected_for_worker = expected_id.clone();
        let outcome = run(
            1,
            move |producer| {
                assert!(producer.request_with_file(
                    Value::from("pinned"),
                    path_for_producer.clone(),
                    Some(expected_id),
                    Some(handle),
                )?);
                assert!(producer.request(Value::from("plain"), path_for_producer, None)?);
                Ok(())
            },
            |_budget, _cancellation| {
                let expected_for_worker = expected_for_worker.clone();
                move |mut request| {
                    if request.id == "pinned" {
                        let pinned = request
                            .pinned_file
                            .take()
                            .expect("pinned file handle must reach the worker");
                        #[cfg(unix)]
                        {
                            use std::os::unix::fs::MetadataExt;
                            let meta = pinned.metadata().expect("pinned metadata readable");
                            assert_eq!(
                                format!("{:x}:{:x}", meta.dev(), meta.ino()),
                                expected_for_worker
                            );
                        }
                        drop(pinned);
                    } else {
                        assert!(request.pinned_file.is_none());
                    }
                    BatchCompletion::report(
                        request,
                        OperationStatus::Complete,
                        Ok(serde_json::json!({"ok": true})),
                    )
                }
            },
            &mut out,
        )
        .expect("batch run succeeds");
        assert_eq!(outcome.status, OperationStatus::Complete);
        let _ = std::fs::remove_dir_all(&directory);
    }
}
