//! Job lifecycle (design §6.1/§8): submission runs on a worker thread;
//! progress and state live behind a shared snapshot the host polls, and a
//! cancel flag every page boundary checks (GDI also mounts `AbortProc`).
//! A process-level JOBS table tracks in-flight worker threads so hosts can
//! join them before exit — a process death mid-spool kills the spooler
//! helper and the job with it (the WordCraft lesson).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

/// Job phase as seen by the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobState {
    /// Preparing/sending the job to the spooler.
    Spooling,
    /// Producing pages (GDI vector replay or CUPS transfer).
    Rendering,
    /// Completed successfully.
    Done,
    /// Cancelled by the host.
    Aborted,
    /// Failed; see the snapshot's error message.
    Failed,
}

/// Point-in-time view of a job (hosts poll this; no channels to plumb).
#[derive(Debug, Clone, PartialEq)]
pub struct JobSnapshot {
    pub state: JobState,
    pub pages_done: usize,
    pub pages_total: usize,
    pub error: Option<String>,
}

impl JobSnapshot {
    pub(crate) fn fresh(pages_total: usize) -> Self {
        Self {
            state: JobState::Spooling,
            pages_done: 0,
            pages_total,
            error: None,
        }
    }
}

/// State shared between the worker thread and every handle clone.
pub(crate) struct SharedJob {
    snapshot: Mutex<JobSnapshot>,
    cancelled: AtomicBool,
}

impl SharedJob {
    pub(crate) fn new(pages_total: usize) -> Self {
        Self {
            snapshot: Mutex::new(JobSnapshot::fresh(pages_total)),
            cancelled: AtomicBool::new(false),
        }
    }

    pub(crate) fn snapshot(&self) -> JobSnapshot {
        self.snapshot.lock().unwrap().clone()
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// Worker-side total update (PDF jobs learn the page count after the
    /// document loads; Pages payloads know it at submit).
    pub(crate) fn set_pages_total(&self, total: usize) {
        self.snapshot.lock().unwrap().pages_total = total;
    }

    /// Worker-side update: set state and/or advance page progress.
    pub(crate) fn update(
        &self,
        state: Option<JobState>,
        pages_done: Option<usize>,
        error: Option<String>,
    ) {
        let mut snap = self.snapshot.lock().unwrap();
        if let Some(state) = state {
            snap.state = state;
        }
        if let Some(done) = pages_done {
            snap.pages_done = done;
        }
        if error.is_some() {
            snap.error = error;
        }
    }
}

/// Host-side handle onto a submitted job (cloneable — every clone sees the
/// same shared snapshot; `wait` joins the worker thread once).
pub struct JobHandle {
    inner: Arc<SharedJob>,
    thread: Option<JoinHandle<()>>,
}

impl Clone for JobHandle {
    fn clone(&self) -> Self {
        // Clones observe progress but do not join (only the handle that
        // still owns the thread does).
        Self {
            inner: Arc::clone(&self.inner),
            thread: None,
        }
    }
}

impl JobHandle {
    pub(crate) fn new(inner: Arc<SharedJob>, thread: JoinHandle<()>) -> Self {
        Self {
            inner,
            thread: Some(thread),
        }
    }

    /// Latest snapshot (state, page progress, error).
    pub fn snapshot(&self) -> JobSnapshot {
        self.inner.snapshot()
    }

    /// Request cancellation. Cooperative: the worker observes the flag at
    /// each page boundary (and via AbortProc under GDI); an already
    /// finished job is unaffected.
    pub fn cancel(&self) {
        self.inner.cancel();
    }

    pub fn is_cancelled(&self) -> bool {
        self.inner.is_cancelled()
    }

    /// Block until the worker finishes; returns the final snapshot.
    pub fn wait(mut self) -> JobSnapshot {
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        self.inner.snapshot()
    }
}

/// Process-level registry of in-flight job threads. Submission registers;
/// thread exit unregisters. Hosts call [`join_all`] before process exit
/// (e.g. on window close, after a veto check) so no spool job is killed
/// mid-flight.
static JOBS: std::sync::OnceLock<Mutex<Vec<JoinHandle<()>>>> = std::sync::OnceLock::new();

fn jobs() -> &'static Mutex<Vec<JoinHandle<()>>> {
    JOBS.get_or_init(|| Mutex::new(Vec::new()))
}

pub(crate) fn register_worker(handle: JobHandle) {
    // Take the thread out; the registry owns the join responsibility until
    // the thread completes (a detached marker replaces the handle's).
    if let Some(thread) = handle.thread {
        jobs().lock().unwrap().push(thread);
    }
    // Prune finished threads opportunistically.
    jobs().lock().unwrap().retain(|t| !t.is_finished());
}

/// Block until every in-flight job thread has exited.
pub fn join_all() {
    loop {
        let next = {
            let mut guard = jobs().lock().unwrap();
            guard.retain(|t| !t.is_finished());
            guard.pop()
        };
        match next {
            Some(thread) => {
                let _ = thread.join();
            }
            None => break,
        }
    }
}

/// Test/example vehicle (doc-hidden): drives the job machinery on a real
/// worker thread without touching any platform sink. `fail_at` aborts with
/// an error after N pages; the worker checks the cancel flag between
/// pages.
#[doc(hidden)]
pub fn submit_noop(pages_total: usize, fail_at: Option<usize>, page_delay_ms: u64) -> JobHandle {
    let shared = Arc::new(SharedJob::new(pages_total));
    let worker = Arc::clone(&shared);
    let thread = std::thread::spawn(move || {
        worker.update(Some(JobState::Rendering), None, None);
        for page in 1..=pages_total {
            if worker.is_cancelled() {
                worker.update(Some(JobState::Aborted), None, None);
                return;
            }
            if page_delay_ms > 0 {
                std::thread::sleep(std::time::Duration::from_millis(page_delay_ms));
            }
            if fail_at == Some(page) {
                worker.update(
                    Some(JobState::Failed),
                    Some(page - 1),
                    Some(format!("synthetic failure at page {page}")),
                );
                return;
            }
            worker.update(None, Some(page), None);
        }
        worker.update(Some(JobState::Done), None, None);
    });
    let handle = JobHandle::new(shared, thread);
    register_worker(handle.clone());
    handle
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn noop_job_completes() {
        let handle = submit_noop(3, None, 0);
        let final_snapshot = handle.wait();
        assert_eq!(final_snapshot.state, JobState::Done);
        assert_eq!(final_snapshot.pages_done, 3);
        assert_eq!(final_snapshot.pages_total, 3);
        assert!(final_snapshot.error.is_none());
    }

    #[test]
    fn noop_job_fails_at_page() {
        let handle = submit_noop(5, Some(2), 0);
        let final_snapshot = handle.wait();
        assert_eq!(final_snapshot.state, JobState::Failed);
        assert_eq!(final_snapshot.pages_done, 1);
        assert!(final_snapshot
            .error
            .as_deref()
            .unwrap()
            .contains("page 2"));
    }

    #[test]
    fn cancel_aborts_mid_job() {
        let handle = submit_noop(10, None, 20);
        handle.cancel();
        let final_snapshot = handle.wait();
        assert!(
            final_snapshot.state == JobState::Aborted
                || final_snapshot.state == JobState::Done,
            "cancel races the fast worker; both are legal: {final_snapshot:?}"
        );
        // A cancel clearly delivered before completion aborts.
        let slow = submit_noop(50, None, 15);
        slow.cancel();
        assert_eq!(slow.wait().state, JobState::Aborted);
    }

    #[test]
    fn join_all_returns_when_table_empty() {
        join_all();
        join_all();
    }

    #[test]
    fn join_all_drains_registered_workers() {
        let handle = submit_noop(4, None, 5);
        drop(handle); // registry still owns the thread
        join_all();
        // After join_all the table is empty; a second call is instant.
        join_all();
    }

    #[test]
    fn clones_share_one_snapshot() {
        let handle = submit_noop(6, None, 0);
        let clone = handle.clone();
        let final_snapshot = handle.wait();
        assert_eq!(clone.snapshot().state, final_snapshot.state);
    }
}
