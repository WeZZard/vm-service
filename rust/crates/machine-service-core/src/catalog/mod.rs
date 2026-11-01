//! The image catalog (`docs/arch/image-catalog.md`, IC-1 to IC-7).
//!
//! The daemon keeps one immutable [`CatalogSnapshot`] of its image lines. A
//! request that reads the catalog first computes the [`InputIdentity`] of the
//! `line.conf` files and compares it with the snapshot's; when they differ it
//! asks for a rebuild and waits for it (IC-3, DC-38). One rebuild runs at a
//! time, and the new snapshot replaces the old one in a single pointer swap
//! (IC-4). A failed rebuild keeps the last good snapshot and reports itself in
//! every [`CatalogStatus`] (IC-5).
//!
//! The file watcher in [`watch`] only starts a rebuild earlier. Nothing here
//! depends on it.

pub mod file;
pub mod watch;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant};

use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use crate::config::unix_now;

pub use file::FileSource;

/// How long a request waits for a rebuild it asked for (IC-3).
pub const REBUILD_WAIT: Duration = Duration::from_secs(30);

/// How many times one rebuild starts over because the files moved while it
/// ran (IC-4) before it gives up and reports a failure.
const MAX_BUILD_ATTEMPTS: usize = 5;

// ------------------------------------------------------------------ lines

/// An image line's resource defaults. `None` leaves the value to the service.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceDefaults {
    /// Virtual CPUs.
    pub cpu: Option<i64>,
    /// Memory in MiB.
    pub memory_mb: Option<i64>,
    /// Disk in GB; `None` inherits the base's disk.
    pub disk_gb: Option<i64>,
}

impl ResourceDefaults {
    /// The `defaults` object `/images` reports.
    pub fn to_json(&self) -> Value {
        json!({"cpu": self.cpu, "memory_mb": self.memory_mb, "disk_gb": self.disk_gb})
    }
}

/// One image line of a snapshot. It carries no credential (IC-1).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageLine {
    /// `macos` or `linux`.
    pub kind: String,
    /// The golden base VM.
    pub base_vm: String,
    /// The work VM, when one is configured.
    pub work_vm: Option<String>,
    /// The prefix of every clone's name.
    pub clone_prefix: String,
    /// The guest account.
    pub guest_user: String,
    /// The resource defaults.
    pub defaults: ResourceDefaults,
    /// The file the line was read from.
    pub source: String,
}

impl ImageLine {
    /// The line as acquisition reads it, without any credential.
    pub fn to_json(&self) -> Value {
        json!({
            "kind": self.kind,
            "base_vm": self.base_vm,
            "work_vm": self.work_vm,
            "clone_prefix": self.clone_prefix,
            "ssh_user": self.guest_user,
            "defaults": self.defaults.to_json(),
            "source": self.source,
        })
    }
}

/// The guest passwords a build read, keyed by image.
///
/// They live beside the snapshot, never in it: no response, status or log
/// line is built from this type, and its `Debug` form prints no value. The
/// lease bootstrap is its only reader.
#[derive(Clone, Default)]
pub struct GuestCredentials(HashMap<String, String>);

impl GuestCredentials {
    /// Record an image's guest password.
    pub fn insert(&mut self, image: impl Into<String>, password: impl Into<String>) {
        self.0.insert(image.into(), password.into());
    }

    /// The guest password of an image.
    pub fn password(&self, image: &str) -> Option<&str> {
        self.0.get(image).map(String::as_str)
    }
}

impl std::fmt::Debug for GuestCredentials {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "GuestCredentials({} redacted)", self.0.len())
    }
}

/// What one successful build produced.
#[derive(Debug, Default)]
pub struct BuiltLines {
    /// The lines, by image name.
    pub lines: BTreeMap<String, ImageLine>,
    /// The guest passwords, kept out of the snapshot.
    pub credentials: GuestCredentials,
}

// --------------------------------------------------------------- identity

/// The identity of one `line.conf` (IC-2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileIdentity {
    /// A regular file. `sha256` is `None` only when the catalog has more
    /// lines than [`crate::application_catalog::MAX_IMAGES`], which fails the
    /// build anyway.
    Regular {
        /// The inode number. The device number is left out (DC-17).
        inode: u64,
        /// The size in bytes.
        size: u64,
        /// The modification time in nanoseconds.
        mtime_ns: i128,
        /// The SHA-256 of the contents.
        sha256: Option<String>,
    },
    /// A `line.conf` that is not a regular file.
    NonRegular,
    /// A `line.conf` that could not be read.
    Unreadable(String),
}

/// One image line's entry in an [`InputIdentity`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdentityEntry {
    /// The image name, the folder's name.
    pub image: String,
    /// The `line.conf` path.
    pub path: PathBuf,
    /// The file's identity.
    pub file: FileIdentity,
}

/// The identity of every catalog input (IC-2): the image names, sorted, each
/// with its `line.conf`'s identity.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InputIdentity {
    /// One entry per folder that holds a `line.conf`, sorted by image.
    pub entries: Vec<IdentityEntry>,
    /// Why the folder of lines could not be listed, when it could not.
    pub unreadable: Option<String>,
}

impl InputIdentity {
    /// A SHA-256 over the whole identity, for logs and tests.
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(format!("{self:?}").as_bytes());
        format!("{:x}", hasher.finalize())
    }
}

// ----------------------------------------------------------------- source

/// Why a build failed (IC-5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogFailure {
    /// The failing file, when one file is to blame.
    pub file: Option<String>,
    /// The reason, safe to show a client.
    pub reason: String,
}

impl CatalogFailure {
    /// A failure of `file` for `reason`.
    pub fn new(file: Option<String>, reason: impl Into<String>) -> Self {
        Self {
            file,
            reason: reason.into(),
        }
    }
}

impl std::fmt::Display for CatalogFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.file {
            Some(file) => write!(formatter, "{file}: {}", self.reason),
            None => write!(formatter, "{}", self.reason),
        }
    }
}

/// The catalog's inputs and its strict builder.
///
/// [`FileSource`] reads `images/*/line.conf`. Tests substitute an in-memory
/// source through [`crate::host::Host::catalog_source`].
pub trait CatalogSource: Send + Sync {
    /// Compute the input identity. Executes nothing (IC-2).
    fn identity(&self) -> InputIdentity;

    /// Build every line `identity` names, or fail as a whole (IC-4).
    fn build(&self, identity: &InputIdentity) -> Result<BuiltLines, CatalogFailure>;

    /// The folder a file watcher should observe, if any.
    fn watch_root(&self) -> Option<PathBuf> {
        None
    }
}

/// The source of an instance with no image source configured (IC-7,
/// DC-94): it reads no files and always builds an empty catalog, so the
/// catalog is current with no lines and nothing warns.
#[derive(Debug, Clone, Copy, Default)]
pub struct NoSource;

impl CatalogSource for NoSource {
    fn identity(&self) -> InputIdentity {
        InputIdentity::default()
    }

    fn build(&self, _identity: &InputIdentity) -> Result<BuiltLines, CatalogFailure> {
        Ok(BuiltLines::default())
    }
}

// --------------------------------------------------------------- snapshot

/// One complete, immutable image catalog (IC-1).
#[derive(Debug)]
pub struct CatalogSnapshot {
    /// Counts the snapshots this daemon has published, from 1.
    pub generation: u64,
    /// When the build finished, in Unix seconds.
    pub built_at: f64,
    /// The input identity the snapshot was built from.
    pub identity: InputIdentity,
    /// The lines, by image name.
    pub lines: BTreeMap<String, ImageLine>,
}

/// What the catalog publishes: the snapshot and, beside it, the credentials
/// of the same build. Swapped as one pointer (IC-4).
#[derive(Debug)]
struct Published {
    snapshot: Arc<CatalogSnapshot>,
    credentials: GuestCredentials,
}

/// Whether the published snapshot matches the files (IC-5).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The snapshot was built from the files as they are now.
    Current,
    /// The files changed and the snapshot no longer matches them.
    Stale,
    /// No good snapshot has been built yet.
    Unavailable,
}

impl Freshness {
    /// The wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Freshness::Current => "current",
            Freshness::Stale => "stale",
            Freshness::Unavailable => "unavailable",
        }
    }
}

/// A failed rebuild, as the status reports it.
#[derive(Debug, Clone)]
pub struct RecordedFailure {
    /// The identity that failed to build.
    pub identity: InputIdentity,
    /// Why.
    pub failure: CatalogFailure,
    /// When, in Unix seconds.
    pub failed_at: f64,
}

/// The catalog status every catalog-reading response carries (IC-5).
#[derive(Debug, Clone)]
pub struct CatalogStatus {
    /// The served snapshot's generation.
    pub generation: Option<u64>,
    /// The served snapshot's build time.
    pub built_at: Option<f64>,
    /// `current`, `stale` or `unavailable`.
    pub freshness: Freshness,
    /// The last rebuild's failure, while it is not superseded.
    pub failure: Option<RecordedFailure>,
    /// Why no snapshot is served, when no failure explains it.
    pub note: Option<String>,
}

impl CatalogStatus {
    /// The `catalog` object of a response.
    pub fn to_json(&self) -> Value {
        let mut body = Map::new();
        body.insert("generation".into(), json!(self.generation));
        body.insert("built_at".into(), number(self.built_at));
        body.insert("status".into(), json!(self.freshness.as_str()));
        if self.freshness != Freshness::Current {
            if let Some(failure) = &self.failure {
                body.insert("error".into(), json!(failure.failure.reason));
                body.insert("failed_at".into(), number(Some(failure.failed_at)));
                body.insert("file".into(), json!(failure.failure.file));
            }
        }
        Value::Object(body)
    }

    /// The diagnostic of a catalog that serves nothing.
    pub fn diagnostic(&self) -> String {
        match (&self.failure, &self.note) {
            (Some(failure), _) => format!("image catalog unavailable: {}", failure.failure),
            (None, Some(note)) => format!("image catalog unavailable: {note}"),
            (None, None) => "image catalog unavailable".to_string(),
        }
    }
}

fn number(value: Option<f64>) -> Value {
    value
        .and_then(serde_json::Number::from_f64)
        .map(Value::Number)
        .unwrap_or(Value::Null)
}

/// What one read of the catalog saw: the snapshot it serves, if any, and the
/// status. A caller that holds a view keeps its snapshot however many swaps
/// follow (IC-4).
#[derive(Debug, Clone)]
pub struct CatalogView {
    published: Option<Arc<Published>>,
    /// The status at the time of the read.
    pub status: CatalogStatus,
}

impl CatalogView {
    /// The served snapshot, or `None` while the catalog is unavailable.
    pub fn snapshot(&self) -> Option<&Arc<CatalogSnapshot>> {
        self.published.as_ref().map(|published| &published.snapshot)
    }

    /// One image's guest password, from the build that made this snapshot.
    pub fn guest_password(&self, image: &str) -> Option<&str> {
        self.published
            .as_ref()
            .and_then(|published| published.credentials.password(image))
    }
}

// ---------------------------------------------------------------- catalog

#[derive(Default)]
struct BuildState {
    /// Rebuilds are allowed only after [`ImageCatalog::start`].
    armed: bool,
    building: bool,
    started: u64,
    finished: u64,
    generation: u64,
    failure: Option<RecordedFailure>,
    /// The last failure written to the log, so each is logged once.
    logged: Option<(InputIdentity, CatalogFailure)>,
}

/// A sink for the service log.
pub type LogSink = Arc<dyn Fn(&str) + Send + Sync>;

/// The image catalog: the published snapshot and the single-flight builder.
pub struct ImageCatalog {
    source: Arc<dyn CatalogSource>,
    published: RwLock<Option<Arc<Published>>>,
    state: Mutex<BuildState>,
    settled: Condvar,
    log: LogSink,
    wait: Duration,
}

impl ImageCatalog {
    /// A catalog over `source` that logs through `log` and lets a request
    /// wait `wait` for a rebuild. Nothing is built until [`Self::start`].
    pub fn new(source: Arc<dyn CatalogSource>, log: LogSink, wait: Duration) -> Arc<Self> {
        Arc::new(Self {
            source,
            published: RwLock::new(None),
            state: Mutex::new(BuildState::default()),
            settled: Condvar::new(),
            log,
            wait,
        })
    }

    /// The catalog's source.
    pub fn source(&self) -> &Arc<dyn CatalogSource> {
        &self.source
    }

    /// Allow rebuilds and build the first snapshot, waiting for it however
    /// long it takes. The daemon calls this once it owns `daemon.lock`, so no
    /// other process ever executes a `line.conf` for it (IC-4).
    pub fn start(self: &Arc<Self>) -> CatalogView {
        self.lock_state().armed = true;
        self.resolve(None, true)
    }

    /// Read the catalog, rebuilding first when the files changed and waiting
    /// for that rebuild up to the configured wait (IC-3).
    pub fn read(self: &Arc<Self>) -> CatalogView {
        self.resolve(Some(Instant::now() + self.wait), false)
    }

    /// Read the catalog without waiting. A changed input still starts a
    /// rebuild, so the next reader likely finds it done.
    pub fn peek(self: &Arc<Self>) -> CatalogView {
        self.resolve(Some(Instant::now()), false)
    }

    /// Rebuild even when the files did not change, wait for it up to the
    /// configured wait, and report the result (`POST /images/reload`).
    pub fn reload(self: &Arc<Self>) -> CatalogView {
        self.resolve(Some(Instant::now() + self.wait), true)
    }

    fn lock_state(&self) -> MutexGuard<'_, BuildState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn current(&self) -> Option<Arc<Published>> {
        self.published
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn resolve(self: &Arc<Self>, deadline: Option<Instant>, force: bool) -> CatalogView {
        let mut state = self.lock_state();
        if !state.armed {
            return Self::view(&state, None, false, Some("not started".into()));
        }
        if force {
            // A forced rebuild waits for one that has not yet read the files:
            // the one it starts, or the one already running (IC-3).
            let target = if state.building {
                state.started
            } else {
                self.spawn(&mut state);
                state.started
            };
            while state.finished < target {
                match self.wait_for_change(state, deadline) {
                    (guard, true) => state = guard,
                    (guard, false) => {
                        drop(guard);
                        return self.settle_view(None);
                    }
                }
            }
        }
        loop {
            drop(state);
            let identity = self.source.identity();
            state = self.lock_state();
            let published = self.current();
            if let Some(current) = &published {
                if current.snapshot.identity == identity {
                    state.failure = None;
                    return Self::view(&state, published, true, None);
                }
            }
            let failed_here = state
                .failure
                .as_ref()
                .is_some_and(|failure| failure.identity == identity);
            if failed_here {
                // A broken file costs one failed execution, not one per
                // request (IC-3).
                return Self::view(&state, published, false, None);
            }
            if !state.building {
                self.spawn(&mut state);
            }
            match self.wait_for_change(state, deadline) {
                (guard, true) => state = guard,
                (guard, false) => {
                    let note = guard.building.then(|| "rebuild in progress".to_string());
                    return Self::view(&guard, self.current(), false, note);
                }
            }
        }
    }

    /// A view after a wait ran out, without starting anything.
    fn settle_view(self: &Arc<Self>, note: Option<String>) -> CatalogView {
        let identity = self.source.identity();
        let state = self.lock_state();
        let published = self.current();
        let matches = published
            .as_ref()
            .is_some_and(|current| current.snapshot.identity == identity);
        let note = note.or_else(|| state.building.then(|| "rebuild in progress".to_string()));
        Self::view(&state, published, matches, note)
    }

    /// Wait until a rebuild finishes or `deadline` passes. Returns whether a
    /// rebuild finished.
    fn wait_for_change<'a>(
        &'a self,
        mut state: MutexGuard<'a, BuildState>,
        deadline: Option<Instant>,
    ) -> (MutexGuard<'a, BuildState>, bool) {
        let seen = state.finished;
        while state.finished == seen {
            if !state.building {
                // Nothing to wait for; the caller re-reads the files.
                return (state, true);
            }
            match deadline {
                None => {
                    state = self
                        .settled
                        .wait(state)
                        .unwrap_or_else(PoisonError::into_inner);
                }
                Some(deadline) => {
                    let now = Instant::now();
                    if now >= deadline {
                        return (state, false);
                    }
                    state = self
                        .settled
                        .wait_timeout(state, deadline - now)
                        .unwrap_or_else(PoisonError::into_inner)
                        .0;
                }
            }
        }
        (state, true)
    }

    fn view(
        state: &BuildState,
        published: Option<Arc<Published>>,
        matches: bool,
        note: Option<String>,
    ) -> CatalogView {
        let freshness = match (&published, matches) {
            (None, _) => Freshness::Unavailable,
            (Some(_), true) => Freshness::Current,
            (Some(_), false) => Freshness::Stale,
        };
        let status = CatalogStatus {
            generation: published
                .as_ref()
                .map(|current| current.snapshot.generation),
            built_at: published.as_ref().map(|current| current.snapshot.built_at),
            freshness,
            failure: state.failure.clone(),
            note,
        };
        CatalogView { published, status }
    }

    fn spawn(self: &Arc<Self>, state: &mut BuildState) {
        state.building = true;
        state.started += 1;
        let catalog = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("image-catalog-build".into())
            .spawn(move || catalog.run_build());
        if let Err(error) = spawned {
            state.building = false;
            state.finished += 1;
            let failure = CatalogFailure::new(None, format!("cannot start the builder: {error}"));
            self.record_failure(state, InputIdentity::default(), failure);
            self.settled.notify_all();
        }
    }

    fn run_build(&self) {
        let outcome =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.build_stable()))
                .unwrap_or_else(|_| {
                    Err((
                        self.source.identity(),
                        CatalogFailure::new(None, "the image catalog builder panicked"),
                    ))
                });
        let mut state = self.lock_state();
        match outcome {
            Ok((identity, built)) => {
                state.generation += 1;
                let names: Vec<&str> = built.lines.keys().map(String::as_str).collect();
                let message = format!(
                    "INFO: image catalog generation {} built: {}",
                    state.generation,
                    names.join(", ")
                );
                let snapshot = CatalogSnapshot {
                    generation: state.generation,
                    built_at: unix_now(),
                    identity,
                    lines: built.lines,
                };
                let published = Arc::new(Published {
                    snapshot: Arc::new(snapshot),
                    credentials: built.credentials,
                });
                // The swap: a reader holds the old pointer or the new one,
                // never a snapshot still being built (IC-4).
                *self
                    .published
                    .write()
                    .unwrap_or_else(PoisonError::into_inner) = Some(published);
                state.failure = None;
                state.logged = None;
                (self.log)(&message);
            }
            Err((identity, failure)) => self.record_failure(&mut state, identity, failure),
        }
        state.building = false;
        state.finished += 1;
        self.settled.notify_all();
    }

    /// Record a failure and log it once per input identity (IC-5).
    fn record_failure(
        &self,
        state: &mut BuildState,
        identity: InputIdentity,
        failure: CatalogFailure,
    ) {
        let key = (identity.clone(), failure.clone());
        if state.logged.as_ref() != Some(&key) {
            let serving = match self.current() {
                Some(current) => format!("serving generation {}", current.snapshot.generation),
                None => "no catalog to serve".to_string(),
            };
            let message = format!("ERROR: image catalog rebuild failed: {failure}; {serving}");
            eprintln!("machine-service: {message}");
            (self.log)(&message);
            state.logged = Some(key);
        }
        state.failure = Some(RecordedFailure {
            identity,
            failure,
            failed_at: unix_now(),
        });
    }

    /// Build until the files hold still across one build (IC-4).
    fn build_stable(&self) -> Result<(InputIdentity, BuiltLines), (InputIdentity, CatalogFailure)> {
        let mut before = self.source.identity();
        for _ in 0..MAX_BUILD_ATTEMPTS {
            let result = self.source.build(&before);
            let after = self.source.identity();
            if after == before {
                return match result {
                    Ok(built) => Ok((before, built)),
                    Err(failure) => Err((before, failure)),
                };
            }
            // The files moved during the build; its result describes no
            // identity, so it is discarded.
            before = after;
        }
        Err((
            before,
            CatalogFailure::new(None, "the image files kept changing during the rebuild"),
        ))
    }
}
