//! The image catalog's unit layer (`docs/arch/image-catalog.md`, IC-6,
//! TL-1): the input identity, the strict builder's refusals, the
//! single-flight rebuild, the identity recheck after a build, the atomic
//! swap, the failure status, and the watcher.
//!
//! Each test cites the acceptance criterion it proves (AC-39 to AC-42 in
//! `docs/acceptance/operator.acceptance`).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use machine_service_core::catalog::{
    file, watch, BuiltLines, CatalogFailure, CatalogSource, FileIdentity, FileSource, Freshness,
    IdentityEntry, ImageCatalog, ImageLine, InputIdentity, LogSink, ResourceDefaults,
};

// ---------------------------------------------------------------- helpers

/// A log sink that keeps every line.
fn capture() -> (LogSink, Arc<Mutex<Vec<String>>>) {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&lines);
    (
        Arc::new(move |message: &str| sink.lock().expect("log").push(message.to_string())),
        lines,
    )
}

fn errors(log: &Arc<Mutex<Vec<String>>>) -> usize {
    log.lock()
        .expect("log")
        .iter()
        .filter(|line| line.starts_with("ERROR"))
        .count()
}

fn line(base: &str) -> ImageLine {
    ImageLine {
        kind: "linux".into(),
        base_vm: base.into(),
        work_vm: None,
        clone_prefix: "t-".into(),
        guest_user: "admin".into(),
        defaults: ResourceDefaults::default(),
        source: format!("scripted://{base}"),
    }
}

/// What a [`Scripted`] source currently holds.
struct Script {
    /// The line names; each line's base is `<name>-base`.
    names: Vec<String>,
    /// Bumped for every change; part of the identity.
    version: u64,
    /// When set, a build fails with this reason.
    fail: Option<String>,
}

/// A controllable source: its identity is its names and version, and a hook
/// runs inside every build.
struct Scripted {
    script: Mutex<Script>,
    builds: AtomicUsize,
    hook: Mutex<Option<BuildHook>>,
}

/// Runs inside a build, with the build's ordinal.
type BuildHook = Box<dyn FnMut(usize) + Send>;

impl Scripted {
    fn new(names: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            script: Mutex::new(Script {
                names: names.iter().map(|name| name.to_string()).collect(),
                version: 0,
                fail: None,
            }),
            builds: AtomicUsize::new(0),
            hook: Mutex::new(None),
        })
    }

    fn set(&self, names: &[&str]) {
        let mut script = self.script.lock().expect("script");
        script.names = names.iter().map(|name| name.to_string()).collect();
        script.version += 1;
    }

    fn fail(&self, reason: Option<&str>) {
        let mut script = self.script.lock().expect("script");
        script.fail = reason.map(str::to_string);
        script.version += 1;
    }

    fn on_build(&self, hook: impl FnMut(usize) + Send + 'static) {
        *self.hook.lock().expect("hook") = Some(Box::new(hook));
    }

    fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }
}

impl CatalogSource for Scripted {
    fn identity(&self) -> InputIdentity {
        let script = self.script.lock().expect("script");
        InputIdentity {
            entries: script
                .names
                .iter()
                .map(|name| IdentityEntry {
                    image: name.clone(),
                    path: PathBuf::from(format!("scripted://{name}/line.conf")),
                    file: FileIdentity::Regular {
                        inode: 0,
                        size: 0,
                        mtime_ns: 0,
                        sha256: Some(format!("{}:{:?}", script.version, script.fail)),
                    },
                })
                .collect(),
            unreadable: None,
        }
    }

    fn build(&self, _identity: &InputIdentity) -> Result<BuiltLines, CatalogFailure> {
        let count = self.builds.fetch_add(1, Ordering::SeqCst) + 1;
        if let Some(hook) = self.hook.lock().expect("hook").as_mut() {
            hook(count);
        }
        let script = self.script.lock().expect("script");
        if let Some(reason) = &script.fail {
            return Err(CatalogFailure::new(
                Some("scripted://broken/line.conf".into()),
                reason.clone(),
            ));
        }
        let mut built = BuiltLines::default();
        for name in &script.names {
            built
                .lines
                .insert(name.clone(), line(&format!("{name}-base")));
            built.credentials.insert(name.clone(), "secret");
        }
        Ok(built)
    }
}

fn catalog(source: &Arc<Scripted>, wait: Duration) -> (Arc<ImageCatalog>, Arc<Mutex<Vec<String>>>) {
    let (sink, log) = capture();
    let catalog = ImageCatalog::new(Arc::clone(source) as Arc<dyn CatalogSource>, sink, wait);
    (catalog, log)
}

fn write_line(images: &Path, image: &str, text: &str) -> PathBuf {
    let directory = images.join(image);
    std::fs::create_dir_all(&directory).expect("image dir");
    let path = directory.join("line.conf");
    let temporary = directory.join("line.conf.tmp");
    std::fs::write(&temporary, text).expect("write line.conf");
    std::fs::rename(&temporary, &path).expect("replace line.conf");
    path
}

const GOOD: &str = "LINE_KIND=linux\nBASE_VM=fixture-base\nCLONE_PREFIX=f-\nGUEST_USER=admin\nGUEST_PASS=secret-do-not-show\nCPU=4\n";

// --------------------------------------------------------------- identity

/// AC-42: computing the identity executes nothing, and it moves when a
/// `line.conf` changes even when its size and modification time do not.
#[test]
fn ac42_identity_covers_every_line_conf_and_executes_nothing() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    let marker = dir.path().join("sourced");
    write_line(&images, "b-line", GOOD);
    let a = write_line(
        &images,
        "a-line",
        &format!(
            "touch {}\nLINE_KIND=linux\nBASE_VM=a\nCLONE_PREFIX=a-\n",
            marker.display()
        ),
    );
    std::fs::create_dir_all(images.join("no-conf")).expect("folder without conf");
    std::fs::write(images.join("stray-file"), "x").expect("stray file");

    let first = file::identity(&images);
    assert!(first.unreadable.is_none());
    let names: Vec<&str> = first
        .entries
        .iter()
        .map(|entry| entry.image.as_str())
        .collect();
    assert_eq!(names, ["a-line", "b-line"], "sorted, conf folders only");
    assert!(matches!(
        first.entries[0].file,
        FileIdentity::Regular {
            sha256: Some(_),
            ..
        }
    ));
    assert!(!marker.exists(), "the identity executed a line.conf");
    assert_eq!(
        first,
        file::identity(&images),
        "stable while nothing changes"
    );

    // Same size, same modification time, other bytes: the hash moves it.
    let metadata = std::fs::metadata(&a).expect("stat");
    let original = std::fs::read(&a).expect("read");
    let mut changed = original.clone();
    let last = changed.len() - 2;
    changed[last] = b'b';
    std::fs::write(&a, &changed).expect("rewrite in place");
    let times = std::fs::FileTimes::new().set_modified(metadata.modified().expect("mtime"));
    std::fs::File::options()
        .write(true)
        .open(&a)
        .expect("open")
        .set_times(times)
        .expect("restore mtime");
    assert_ne!(first, file::identity(&images));
}

/// AC-41: the identity records what the builder must refuse, so a broken
/// input has a stable identity and is not retried on every request.
#[test]
fn ac41_identity_records_unreadable_and_nonregular_inputs() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    std::fs::create_dir_all(images.join("odd/line.conf")).expect("a folder named line.conf");
    let identity = file::identity(&images);
    assert_eq!(identity.entries[0].file, FileIdentity::NonRegular);
    assert_eq!(identity, file::identity(&images));

    let missing = file::identity(&dir.path().join("absent"));
    assert!(missing.unreadable.is_some());
}

// ---------------------------------------------------------------- builder

/// AC-42: the builder is strict; every rule fails the whole build and names
/// the file.
#[test]
fn ac42_builder_refuses_every_strict_rule() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    write_line(&images, "good", GOOD);
    let cases = [
        ("false\n", "parse failed"),
        (
            "LINE_KIND=linux\nBASE_VM=base\nCLONE_PREFIX=x\nfalse\n",
            "parse failed",
        ),
        ("syntax ( error\n", "parse failed"),
        ("BASE_VM=base\nCLONE_PREFIX=x\n", "invalid association OS"),
        (
            "LINE_KIND=bsd\nBASE_VM=base\nCLONE_PREFIX=x\n",
            "invalid association OS",
        ),
        (
            "LINE_KIND=linux\nBASE_VM=../up\nCLONE_PREFIX=x\n",
            "invalid image/base",
        ),
        (
            "LINE_KIND=linux\nBASE_VM=base\nCLONE_PREFIX=x\nMEMORY_MB=lots\n",
            "resource defaults",
        ),
    ];
    for (text, reason) in cases {
        let path = write_line(&images, "zz", text);
        let failure = file::build(&images, &file::identity(&images)).expect_err(text);
        assert!(failure.reason.contains(reason), "{text}: {failure}");
        assert_eq!(failure.file, Some(path.display().to_string()), "{text}");
    }
    std::fs::remove_dir_all(images.join("zz")).expect("remove zz");

    // A non-regular line.conf.
    std::fs::create_dir_all(images.join("dir/line.conf")).expect("dir conf");
    let failure = file::build(&images, &file::identity(&images)).expect_err("nonregular");
    assert!(failure.reason.contains("nonregular"), "{failure}");
    std::fs::remove_dir_all(images.join("dir")).expect("remove dir");

    // More lines than the maximum.
    for index in 0..=machine_service_core::application_catalog::MAX_IMAGES {
        write_line(&images, &format!("line{index:03}"), GOOD);
    }
    let failure = file::build(&images, &file::identity(&images)).expect_err("too many");
    assert!(failure.reason.contains("too many"), "{failure}");
    std::fs::remove_dir_all(&images).expect("remove images");

    // No lines, and no folder at all.
    std::fs::create_dir_all(&images).expect("empty images");
    let failure = file::build(&images, &file::identity(&images)).expect_err("empty");
    assert!(
        failure.reason.contains("no configured image lines"),
        "{failure}"
    );
    let absent = dir.path().join("absent");
    let failure = file::build(&absent, &file::identity(&absent)).expect_err("absent");
    assert!(failure.reason.contains("unreadable"), "{failure}");
}

/// IC-1: a built line carries its defaults and no credential; the guest
/// password stays beside the snapshot and out of every status.
#[test]
fn ic1_snapshot_and_status_carry_no_credential() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    write_line(&images, "fixture", GOOD);
    let (sink, _log) = capture();
    let catalog = ImageCatalog::new(
        Arc::new(FileSource::new(images)),
        sink,
        Duration::from_secs(30),
    );
    let view = catalog.start();
    let snapshot = view.snapshot().expect("snapshot");
    let line = &snapshot.lines["fixture"];
    assert_eq!(line.defaults.cpu, Some(4));
    assert_eq!(line.defaults.memory_mb, Some(16384));
    assert_eq!(line.guest_user, "admin");
    assert_eq!(view.guest_password("fixture"), Some("secret-do-not-show"));
    for text in [
        format!("{snapshot:?}"),
        line.to_json().to_string(),
        view.status.to_json().to_string(),
        format!("{view:?}"),
    ] {
        assert!(!text.contains("secret-do-not-show"), "{text}");
    }
}

// ----------------------------------------------------------- the catalog

/// AC-42: reads before the daemon starts the catalog build nothing.
#[test]
fn ac42_nothing_is_built_before_start() {
    let source = Scripted::new(&["a"]);
    let (catalog, _log) = catalog(&source, Duration::from_secs(5));
    for _ in 0..3 {
        let view = catalog.read();
        assert_eq!(view.status.freshness, Freshness::Unavailable);
        assert!(view.snapshot().is_none());
    }
    assert_eq!(source.builds(), 0);
    let view = catalog.start();
    assert_eq!(view.status.freshness, Freshness::Current);
    assert_eq!(view.status.generation, Some(1));
    assert_eq!(source.builds(), 1);
}

/// AC-39: readers that find the inputs changed rebuild once between them,
/// and all of them see the new lines (single flight, IC-3).
#[test]
fn ac39_concurrent_readers_share_one_rebuild() {
    let source = Scripted::new(&["a"]);
    let (catalog, _log) = catalog(&source, Duration::from_secs(10));
    catalog.start();
    let started = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&started);
    source.on_build(move |_| {
        flag.store(true, Ordering::SeqCst);
        std::thread::sleep(Duration::from_millis(300));
    });
    source.set(&["a", "extra"]);

    let readers: Vec<_> = (0..16)
        .map(|_| {
            let catalog = Arc::clone(&catalog);
            std::thread::spawn(move || catalog.read())
        })
        .collect();
    for reader in readers {
        let view = reader.join().expect("reader");
        assert_eq!(view.status.freshness, Freshness::Current);
        let names: Vec<&String> = view.snapshot().expect("snapshot").lines.keys().collect();
        assert_eq!(names, ["a", "extra"]);
        assert_eq!(view.status.generation, Some(2));
    }
    assert!(started.load(Ordering::SeqCst));
    assert_eq!(
        source.builds(),
        2,
        "one startup build and one shared rebuild"
    );
}

/// AC-42: a build whose inputs moved while it ran is discarded and redone,
/// so no snapshot carries an identity it was not built from (IC-4).
#[test]
fn ac42_build_whose_inputs_moved_is_discarded() {
    let source = Scripted::new(&["a"]);
    let (catalog, _log) = catalog(&source, Duration::from_secs(10));
    let mover = Arc::clone(&source);
    source.on_build(move |count| {
        if count == 1 {
            // The files change while the first build executes them.
            mover.script.lock().expect("script").names = vec!["b".into()];
            mover.script.lock().expect("script").version += 1;
        }
    });
    let view = catalog.start();
    assert_eq!(source.builds(), 2);
    let snapshot = view.snapshot().expect("snapshot");
    let names: Vec<&String> = snapshot.lines.keys().collect();
    assert_eq!(names, ["b"]);
    assert_eq!(snapshot.identity, source.identity());
    assert_eq!(
        snapshot.generation, 1,
        "the discarded build published nothing"
    );
}

/// AC-42: readers running during one thousand rebuilds that alternate
/// between two sets of lines only ever see one complete set, labelled with
/// its own generation and input identity.
#[test]
fn ac42_readers_see_only_complete_snapshots_across_a_thousand_swaps() {
    let source = Scripted::new(&["mac-a", "mac-b"]);
    let (catalog, _log) = catalog(&source, Duration::from_secs(30));
    catalog.start();
    let stop = Arc::new(AtomicBool::new(false));
    let readers: Vec<_> = (0..4)
        .map(|_| {
            let catalog = Arc::clone(&catalog);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut last = 0;
                let mut seen = 0usize;
                while !stop.load(Ordering::SeqCst) {
                    let view = catalog.peek();
                    let snapshot = view.snapshot().expect("never unavailable").clone();
                    let names: Vec<&str> = snapshot.lines.keys().map(String::as_str).collect();
                    assert!(
                        names == ["mac-a", "mac-b"] || names == ["linux-c"],
                        "a partial set: {names:?}"
                    );
                    let labelled: Vec<&str> = snapshot
                        .identity
                        .entries
                        .iter()
                        .map(|entry| entry.image.as_str())
                        .collect();
                    assert_eq!(names, labelled, "the identity labels another set");
                    for (name, line) in &snapshot.lines {
                        assert_eq!(line.base_vm, format!("{name}-base"));
                    }
                    assert!(snapshot.generation >= last, "generation went back");
                    last = snapshot.generation;
                    seen += 1;
                }
                seen
            })
        })
        .collect();
    for round in 0..1000 {
        if round % 2 == 0 {
            source.set(&["linux-c"]);
        } else {
            source.set(&["mac-a", "mac-b"]);
        }
        let view = catalog.reload();
        assert_eq!(view.status.freshness, Freshness::Current, "round {round}");
    }
    stop.store(true, Ordering::SeqCst);
    for reader in readers {
        assert!(reader.join().expect("reader") > 0);
    }
    let last = catalog.read();
    assert!(last.status.generation.expect("generation") >= 1001);
}

/// AC-41: a broken file keeps the last good snapshot, reports `stale` with
/// the file, the error and the time, logs exactly one ERROR line however
/// many requests follow, and fixing the file returns `current` at a new
/// generation.
#[test]
fn ac41_failed_rebuild_keeps_last_good_and_is_loud_once() {
    let source = Scripted::new(&["macos26", "ubuntu2404"]);
    let (catalog, log) = catalog(&source, Duration::from_secs(10));
    catalog.start();
    for _ in 0..6 {
        catalog.reload();
    }
    let good = catalog.read();
    assert_eq!(good.status.generation, Some(7));
    assert_eq!(good.status.freshness, Freshness::Current);

    source.fail(Some("line configuration parse failed (zsh exit status 1)"));
    let builds = source.builds();
    for _ in 0..20 {
        let view = catalog.read();
        assert_eq!(view.status.freshness, Freshness::Stale);
        assert_eq!(view.status.generation, Some(7));
        let names: Vec<&String> = view.snapshot().expect("last good").lines.keys().collect();
        assert_eq!(names, ["macos26", "ubuntu2404"]);
        let status = view.status.to_json();
        assert_eq!(status["status"], "stale");
        assert_eq!(status["file"], "scripted://broken/line.conf");
        assert!(status["error"]
            .as_str()
            .expect("error")
            .contains("parse failed"));
        assert!(status["failed_at"].as_f64().expect("failed_at") > 0.0);
        assert_eq!(catalog.peek().status.freshness, Freshness::Stale);
    }
    assert_eq!(
        source.builds(),
        builds + 1,
        "one failed execution, not one per request"
    );
    assert_eq!(errors(&log), 1);

    source.fail(None);
    let fixed = catalog.read();
    assert_eq!(fixed.status.freshness, Freshness::Current);
    assert_eq!(fixed.status.generation, Some(8));
    let status = fixed.status.to_json();
    assert!(status.get("error").is_none(), "{status}");
    assert_eq!(errors(&log), 1);
}

/// AC-41: with no good snapshot yet, the catalog is `unavailable`, and the
/// next change to the files is tried at once, with no restart.
#[test]
fn ac41_unavailable_start_recovers_on_the_next_change() {
    let source = Scripted::new(&["a"]);
    source.fail(Some("broken"));
    let (catalog, log) = catalog(&source, Duration::from_secs(10));
    let view = catalog.start();
    assert_eq!(view.status.freshness, Freshness::Unavailable);
    assert!(view.status.diagnostic().contains("broken"));
    assert_eq!(view.status.to_json()["status"], "unavailable");
    assert_eq!(view.status.to_json()["generation"], serde_json::Value::Null);
    assert_eq!(errors(&log), 1);
    assert_eq!(catalog.read().status.freshness, Freshness::Unavailable);

    source.fail(None);
    let view = catalog.read();
    assert_eq!(view.status.freshness, Freshness::Current);
    assert_eq!(view.status.generation, Some(1));
}

/// IC-3: `POST /images/reload` rebuilds unchanged inputs and retries a
/// failed identity, which plain reads do not.
#[test]
fn ic3_reload_rebuilds_and_retries() {
    let source = Scripted::new(&["a"]);
    let (catalog, log) = catalog(&source, Duration::from_secs(10));
    catalog.start();
    let view = catalog.reload();
    assert_eq!(view.status.generation, Some(2));
    assert_eq!(source.builds(), 2);

    source.fail(Some("broken"));
    catalog.read();
    catalog.read();
    assert_eq!(source.builds(), 3);
    let failed = catalog.reload();
    assert_eq!(source.builds(), 4, "reload retries the failed identity");
    assert_eq!(failed.status.freshness, Freshness::Stale);
    assert_eq!(errors(&log), 1, "the same failure is logged once");
}

/// IC-3: a request waits for a slow rebuild only up to its bound, then
/// answers from the last good snapshot as `stale`.
#[test]
fn ic3_the_wait_for_a_rebuild_is_bounded() {
    let source = Scripted::new(&["a"]);
    let (catalog, _log) = catalog(&source, Duration::from_millis(200));
    catalog.start();
    let release = Arc::new(AtomicBool::new(false));
    let gate = Arc::clone(&release);
    source.on_build(move |_| {
        while !gate.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(10));
        }
    });
    source.set(&["a", "b"]);
    let began = Instant::now();
    let view = catalog.read();
    assert!(began.elapsed() < Duration::from_secs(5));
    assert_eq!(view.status.freshness, Freshness::Stale);
    assert_eq!(view.status.note.as_deref(), Some("rebuild in progress"));
    assert_eq!(view.snapshot().expect("last good").lines.len(), 1);

    release.store(true, Ordering::SeqCst);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let view = catalog.read();
        if view.status.freshness == Freshness::Current {
            assert_eq!(view.snapshot().expect("snapshot").lines.len(), 2);
            break;
        }
        assert!(Instant::now() < deadline, "the rebuild never landed");
    }
}

/// AC-39, AC-40: on real files, adding a line, editing its CPU and removing
/// it each show on the next read.
#[test]
fn ac39_ac40_file_changes_show_on_the_next_read() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    write_line(&images, "macos26", GOOD);
    let (sink, _log) = capture();
    let catalog = ImageCatalog::new(
        Arc::new(FileSource::new(images.clone())),
        sink,
        Duration::from_secs(30),
    );
    catalog.start();

    write_line(
        &images,
        "linux-extra",
        &GOOD.replace("fixture-base", "extra-base"),
    );
    let names = |catalog: &Arc<ImageCatalog>| -> Vec<String> {
        catalog
            .read()
            .snapshot()
            .expect("snapshot")
            .lines
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(names(&catalog), ["linux-extra", "macos26"]);

    write_line(&images, "macos26", &GOOD.replace("CPU=4", "CPU=2"));
    let view = catalog.read();
    assert_eq!(
        view.snapshot().expect("snapshot").lines["macos26"]
            .defaults
            .cpu,
        Some(2)
    );

    std::fs::remove_dir_all(images.join("linux-extra")).expect("remove line");
    assert_eq!(names(&catalog), ["macos26"]);
}

// ----------------------------------------------------------------- watcher

/// IC-3: the watcher reports a new folder, an edited `line.conf` and a
/// replaced one.
#[cfg(target_os = "macos")]
#[test]
fn ic3_watcher_reports_changes() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    let conf = write_line(&images, "macos26", GOOD);
    let count = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&count);
    let _watcher = watch::watch(images.clone(), move || {
        counter.fetch_add(1, Ordering::SeqCst);
    })
    .expect("watcher");
    // Let the watcher arm before the first change.
    std::thread::sleep(Duration::from_millis(300));

    let wait_for = |at_least: usize| {
        let deadline = Instant::now() + Duration::from_secs(5);
        while count.load(Ordering::SeqCst) < at_least {
            assert!(
                Instant::now() < deadline,
                "no event after {at_least} changes"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&conf)
        .expect("open")
        .write_all(b"# edited in place\n")
        .expect("append");
    wait_for(1);
    std::thread::sleep(Duration::from_millis(300));
    write_line(&images, "macos26", GOOD);
    wait_for(2);
    std::thread::sleep(Duration::from_millis(300));
    write_line(&images, "linux-extra", GOOD);
    wait_for(3);
}

/// IC-3, DC-38: with the watcher running, a change is rebuilt before any
/// request asks for it.
#[cfg(target_os = "macos")]
#[test]
fn ic3_watcher_rebuilds_before_a_request() {
    let dir = tempfile::tempdir().expect("tempdir");
    let images = dir.path().join("images");
    write_line(&images, "macos26", GOOD);
    let (sink, log) = capture();
    let catalog = ImageCatalog::new(
        Arc::new(FileSource::new(images.clone())),
        sink,
        Duration::from_secs(30),
    );
    catalog.start();
    let _watcher = watch::watch_catalog(&catalog)
        .expect("watcher")
        .expect("a file source is watchable");
    std::thread::sleep(Duration::from_millis(300));

    write_line(&images, "linux-extra", GOOD);
    let deadline = Instant::now() + Duration::from_secs(10);
    while !log
        .lock()
        .expect("log")
        .iter()
        .any(|line| line.contains("generation 2"))
    {
        assert!(Instant::now() < deadline, "the watcher started no rebuild");
        std::thread::sleep(Duration::from_millis(20));
    }
}
