//! Write-ahead file checkpoints: every file a write tool is about to touch is
//! copied aside first, grouped per task run ("epoch"). `/undo` restores the
//! newest epoch — an edit-safety net for agent-driven changes.
//!
//! Layout (under the platform data dir, `kazi/checkpoints/`):
//! ```text
//! <root>/<epoch>/manifest.jsonl   one JSON entry per snapshot, append order
//! <root>/<epoch>/files/<idx>      the pre-write copy of each file
//! ```
//! Entries record `existed: false` for to-be-created files, so undo deletes
//! them. Undo replays entries in reverse (last snapshot wins per file) and is
//! best-effort per file: one failure does not abort the rest. A replayed epoch
//! gets a `.undone` marker, so `/undo` can never apply the same backup twice
//! over whatever the user edited in between.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// How many epochs to keep on disk.
const KEEP_EPOCHS: usize = 20;

/// Marker file inside a replayed epoch dir.
const UNDONE_MARKER: &str = ".undone";

#[derive(Debug, Serialize, Deserialize)]
struct Entry {
    /// Absolute path that was (about to be) written.
    path: PathBuf,
    /// Copy of the original content, relative to the epoch dir. `None` when
    /// the file did not exist yet.
    backup: Option<PathBuf>,
    existed: bool,
}

/// Paths a replay touched, split by what happened to them.
#[derive(Debug, Default)]
pub struct Undone {
    /// Files whose pre-write content was put back.
    pub restored: Vec<PathBuf>,
    /// Files that did not exist before the task and were deleted again.
    pub removed: Vec<PathBuf>,
    /// Files that could not be touched (locked, missing, permissions).
    pub failed: Vec<PathBuf>,
}

/// What `/undo` found for the newest checkpoint.
#[derive(Debug)]
pub enum UndoOutcome {
    /// No checkpointed task exists yet.
    Nothing,
    /// The newest checkpoint was already replayed before.
    AlreadyUndone,
    /// A checkpoint was replayed; [`Undone`] says what happened per file.
    Replayed(Undone),
}

/// Low-level store: epoch allocation, snapshots, undo.
pub struct Checkpoints {
    root: PathBuf,
    /// Held across snapshot index allocation + manifest append: parallel tool
    /// calls in one round used to read the same line count and claim the same
    /// `files/<idx>` slot, so `/undo` restored one file's bytes into another.
    write_lock: Mutex<()>,
}

impl Checkpoints {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            write_lock: Mutex::new(()),
        }
    }

    /// Allocate the next epoch (`max-on-disk + 1`) and prune old ones.
    pub fn begin_epoch(&self) -> u64 {
        let n = self.latest_epoch().unwrap_or(0) + 1;
        let _ = std::fs::create_dir_all(self.root.join(n.to_string()));
        self.prune();
        n
    }

    /// Highest epoch present on disk, if any.
    pub fn latest_epoch(&self) -> Option<u64> {
        let mut best: Option<u64> = None;
        for e in std::fs::read_dir(&self.root).ok()?.flatten() {
            if let Ok(n) = e.file_name().to_string_lossy().parse::<u64>() {
                best = Some(best.map_or(n, |b: u64| b.max(n)));
            }
        }
        best
    }

    fn epoch_dir(&self, epoch: u64) -> PathBuf {
        self.root.join(epoch.to_string())
    }

    /// Copy `path` aside before it is modified. Best-effort: failures are
    /// swallowed (an un-undoable write is worse than no safety net, but must
    /// never block the write itself).
    pub fn snapshot(&self, epoch: u64, path: &Path) {
        let _serialized = self.write_lock.lock().unwrap_or_else(|e| e.into_inner());
        let dir = self.epoch_dir(epoch);
        let files = dir.join("files");
        if std::fs::create_dir_all(&files).is_err() {
            return;
        }
        let manifest = dir.join("manifest.jsonl");
        let idx = std::fs::read_to_string(&manifest)
            .map(|s| s.lines().count())
            .unwrap_or(0);

        let (backup, existed) = if path.exists() {
            let rel = PathBuf::from("files").join(idx.to_string());
            if std::fs::copy(path, dir.join(&rel)).is_err() {
                return;
            }
            (Some(rel), true)
        } else {
            (None, false)
        };

        let entry = Entry {
            path: path.to_path_buf(),
            backup,
            existed,
        };
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&manifest)
        {
            let _ = writeln!(f, "{}", serde_json::to_string(&entry).unwrap_or_default());
        }
    }

    /// Restore the newest epoch unless it was already replayed. An epoch with
    /// failures stays replayable so a locked file can be retried.
    pub fn undo_latest(&self) -> std::io::Result<UndoOutcome> {
        let Some(epoch) = self.latest_epoch() else {
            return Ok(UndoOutcome::Nothing);
        };
        if self.epoch_dir(epoch).join(UNDONE_MARKER).exists() {
            return Ok(UndoOutcome::AlreadyUndone);
        }
        let undone = self.undo(epoch)?;
        if undone.failed.is_empty() {
            let _ = std::fs::write(self.epoch_dir(epoch).join(UNDONE_MARKER), b"");
        }
        Ok(UndoOutcome::Replayed(undone))
    }

    /// Replay one epoch: entries last-first so the newest snapshot of a
    /// repeatedly-written file wins. Best-effort per file — a failure lands in
    /// [`Undone::failed`] and does not abort the rest. An epoch whose manifest
    /// is absent means the task changed nothing (the dir is created up front).
    pub fn undo(&self, epoch: u64) -> std::io::Result<Undone> {
        let dir = self.epoch_dir(epoch);
        let manifest = dir.join("manifest.jsonl");
        let text = match std::fs::read_to_string(&manifest) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(e),
        };
        let entries: Vec<Entry> = text
            .lines()
            .filter_map(|l| serde_json::from_str(l).ok())
            .collect();

        let mut done = Undone::default();
        for entry in entries.iter().rev() {
            match (&entry.backup, entry.existed) {
                (Some(rel), true) => match std::fs::copy(dir.join(rel), &entry.path) {
                    Ok(_) => done.restored.push(entry.path.clone()),
                    Err(_) => done.failed.push(entry.path.clone()),
                },
                (None, false) => {
                    if !entry.path.exists() || std::fs::remove_file(&entry.path).is_ok() {
                        done.removed.push(entry.path.clone());
                    } else {
                        done.failed.push(entry.path.clone());
                    }
                }
                (Some(_), false) => {} // inconsistent manifest: skip
                (None, true) => done.failed.push(entry.path.clone()),
            }
        }
        Ok(done)
    }

    fn prune(&self) {
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return;
        };
        let mut epochs: Vec<(u64, PathBuf)> = entries
            .flatten()
            .filter_map(|e| {
                e.file_name()
                    .to_string_lossy()
                    .parse::<u64>()
                    .ok()
                    .map(|n| (n, e.path()))
            })
            .collect();
        if epochs.len() <= KEEP_EPOCHS {
            return;
        }
        epochs.sort_by_key(|(n, _)| *n);
        for (_, path) in epochs.iter().take(epochs.len() - KEEP_EPOCHS) {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

/// Task-scoped handle held by [`super::ToolCtx`]: remembers which epoch the
/// current run is writing into, so tools can snapshot without arguments.
pub struct Checkpointing {
    store: Checkpoints,
    epoch: Mutex<Option<u64>>,
}

impl Checkpointing {
    pub fn new(root: PathBuf) -> Self {
        Self {
            store: Checkpoints::new(root),
            epoch: Mutex::new(None),
        }
    }

    /// Open a new epoch for the task about to run.
    pub fn begin_epoch(&self) -> u64 {
        let n = self.store.begin_epoch();
        *self.epoch.lock().unwrap_or_else(|e| e.into_inner()) = Some(n);
        n
    }

    /// Snapshot a path into the current epoch (no-op outside a run).
    pub fn snapshot(&self, path: &Path) {
        let epoch = *self.epoch.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(epoch) = epoch {
            self.store.snapshot(epoch, path);
        }
    }

    /// `/undo`: restore the newest epoch.
    pub fn undo_latest(&self) -> std::io::Result<UndoOutcome> {
        self.store.undo_latest()
    }
}

/// Read a file as text for staging: refuses binaries and oversized files the
/// same way the edit tools do, and treats "missing" as an empty base.
pub fn read_text_lossy(path: &Path) -> Option<String> {
    const READ_CAP: u64 = 4 * 1024 * 1024;
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > READ_CAP {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.iter().take(8192).any(|b| *b == 0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

/// One planned file change parked in review mode.
#[derive(Debug, Clone)]
pub struct StagedChange {
    /// Tool that planned the change (`write_file` / `apply_patch`).
    pub tool: String,
    /// Absolute path the change targets.
    pub path: PathBuf,
    /// Content before the change (`None` = the file does not exist yet).
    pub before: Option<String>,
    /// Content as it will be written when accepted.
    pub after: String,
}

impl StagedChange {
    /// Displayed path: relative to `cwd` when it lives underneath it.
    pub fn display_path(&self, cwd: &Path) -> String {
        self.path
            .strip_prefix(cwd)
            .unwrap_or(&self.path)
            .display()
            .to_string()
    }

    /// Unified diff of the planned change (no change → `None`).
    pub fn diff(&self, cwd: &Path) -> Option<crate::diff::UnifiedDiff> {
        crate::diff::unified(
            self.before.as_deref().unwrap_or(""),
            &self.after,
            &self.display_path(cwd),
            3,
        )
    }

    /// `+added` / `-removed` counts, computed from the diff.
    pub fn counts(&self, cwd: &Path) -> (usize, usize) {
        self.diff(cwd)
            .map(|d| (d.added, d.removed))
            .unwrap_or((0, 0))
    }
}

/// Review-mode staging area: write tools park their planned content here
/// instead of touching the disk, and the engine turns the batch into a
/// changeset the user accepts or rejects file by file.
///
/// One entry per path (a later change to the same file replaces the earlier
/// one and `apply_patch` patches the *staged* text, not the stale disk copy),
/// in first-touch order so the review list matches the model's plan.
#[derive(Default)]
pub struct Staging {
    pending: Mutex<Vec<StagedChange>>,
}

impl Staging {
    pub fn new() -> Self {
        Self::default()
    }

    /// Current staged content for `path`, if any — what a follow-up patch in
    /// the same round must edit against.
    pub fn pending_text(&self, path: &Path) -> Option<String> {
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|c| c.path == path)
            .map(|c| c.after.clone())
    }

    /// Park one change; replaces an earlier pending change for the same path
    /// while keeping the original position.
    pub fn stage(&self, change: StagedChange) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.iter_mut().find(|c| c.path == change.path) {
            Some(existing) => {
                // keep the ORIGINAL `before` so the diff stays truthful
                existing.tool = change.tool;
                existing.after = change.after;
            }
            None => pending.push(change),
        }
    }

    pub fn len(&self) -> usize {
        self.pending.lock().unwrap_or_else(|e| e.into_inner()).len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Drain everything staged so far (the engine owns the verdict).
    pub fn take(&self) -> Vec<StagedChange> {
        std::mem::take(&mut *self.pending.lock().unwrap_or_else(|e| e.into_inner()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tempdir(name: &str) -> PathBuf {
        let id = std::process::id() as u64
            + std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;
        let d = std::env::temp_dir().join(format!("kazi-ckpt-{name}-{id}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn undo_restores_modified_files() {
        let root = tempdir("restore");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("note.txt");
        std::fs::write(&file, "original").unwrap();

        let _epoch = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "changed by the agent").unwrap();

        let UndoOutcome::Replayed(done) = cp.undo_latest().unwrap() else {
            panic!("epoch exists");
        };
        assert_eq!(done.restored, vec![file.clone()]);
        assert!(done.failed.is_empty());
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "original");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_deletes_files_that_did_not_exist() {
        let root = tempdir("delete");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("brand-new.txt");

        let _epoch = cp.begin_epoch();
        cp.snapshot(&file); // records existed: false
        std::fs::write(&file, "created").unwrap();
        assert!(file.exists());

        let UndoOutcome::Replayed(done) = cp.undo_latest().unwrap() else {
            panic!("epoch exists");
        };
        assert_eq!(done.removed, vec![file.clone()]);
        assert!(!file.exists(), "undo must remove created files");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn snapshots_are_scoped_per_epoch_and_latest_wins() {
        let root = tempdir("scoped");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("f.txt");

        std::fs::write(&file, "v1").unwrap();
        let _e1 = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "v2").unwrap();

        let _e2 = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "v3").unwrap();

        // undo only touches the newest epoch → back to v2
        let UndoOutcome::Replayed(done) = cp.undo_latest().unwrap() else {
            panic!("epoch exists");
        };
        assert_eq!(done.restored.len(), 1);
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "v2");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_without_epochs_is_nothing() {
        let root = tempdir("empty");
        let cp = Checkpointing::new(root.join("store"));
        assert!(matches!(cp.undo_latest().unwrap(), UndoOutcome::Nothing));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_second_undo_never_replays_the_same_epoch() {
        let root = tempdir("twice");
        let cp = Checkpointing::new(root.join("store"));
        let file = root.join("f.txt");
        std::fs::write(&file, "v1").unwrap();

        let _epoch = cp.begin_epoch();
        cp.snapshot(&file);
        std::fs::write(&file, "agent edit").unwrap();

        assert!(matches!(
            cp.undo_latest().unwrap(),
            UndoOutcome::Replayed(_)
        ));
        // the user fixes things by hand; a repeated /undo must not clobber that
        std::fs::write(&file, "hand-fixed").unwrap();
        assert!(matches!(
            cp.undo_latest().unwrap(),
            UndoOutcome::AlreadyUndone
        ));
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "hand-fixed");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn undo_after_a_task_that_changed_nothing_is_empty_not_an_error() {
        let root = tempdir("nochanges");
        let cp = Checkpointing::new(root.join("store"));
        let _epoch = cp.begin_epoch(); // dir created up front, nothing snapshotted

        let UndoOutcome::Replayed(done) = cp.undo_latest().unwrap() else {
            panic!("epoch exists");
        };
        assert_eq!(
            done.restored.len() + done.removed.len() + done.failed.len(),
            0
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Parallel tool calls in one round snapshot concurrently: every file must
    /// get its own backup slot, or undo restores one file's bytes into another.
    #[test]
    fn parallel_snapshots_keep_distinct_backups() {
        let root = tempdir("parallel");
        let cp = std::sync::Arc::new(Checkpointing::new(root.join("store")));
        let _epoch = cp.begin_epoch();
        let files: Vec<PathBuf> = (0..8).map(|i| root.join(format!("f{i}.txt"))).collect();
        for (i, f) in files.iter().enumerate() {
            std::fs::write(f, format!("v{i}")).unwrap();
        }

        let handles: Vec<_> = files
            .iter()
            .cloned()
            .map(|f| {
                let cp = cp.clone();
                std::thread::spawn(move || cp.snapshot(&f))
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        for f in &files {
            std::fs::write(f, "clobbered").unwrap();
        }

        let UndoOutcome::Replayed(done) = cp.undo_latest().unwrap() else {
            panic!("epoch exists");
        };
        assert_eq!(done.restored.len(), files.len(), "one backup per file");
        for (i, f) in files.iter().enumerate() {
            assert_eq!(
                std::fs::read_to_string(f).unwrap(),
                format!("v{i}"),
                "file {i} must restore from its own backup"
            );
        }
        let _ = std::fs::remove_dir_all(&root);
    }
}
