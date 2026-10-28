use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::compiler::Compiler as _;
use crate::daemon::Daemon;
use crate::daemon_publish::PublishJob;
use crate::store::BuildClaim;

/// A compile's outputs in a target directory of their own.
struct Compiled {
    _dir: tempfile::TempDir,
    config: Config,
    args: RustcArgs,
    rlib: PathBuf,
    artifacts: Vec<Artifact>,
    files: Vec<(PathBuf, String)>,
    key: String,
}

fn compiled(label: &str) -> Compiled {
    let dir = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(dir.path().join("cache"));
    compiled_in(dir, config, label, b'r')
}

/// Compiles of their own, `fill` the byte of each output, all storing into
/// one store.
fn compiled_many(count: usize) -> (tempfile::TempDir, Vec<Compiled>) {
    let root = tempfile::tempdir().unwrap();
    let config = crate::test_support::test_config(root.path().join("cache"));
    let compiled = (0..count)
        .map(|index| {
            let dir = tempfile::tempdir_in(root.path()).unwrap();
            let fill = b'a' + u8::try_from(index % 26).unwrap();
            compiled_in(dir, config.clone(), &format!("many-{index}"), fill)
        })
        .collect();
    (root, compiled)
}

fn compiled_in(dir: tempfile::TempDir, config: Config, label: &str, fill: u8) -> Compiled {
    let deps = dir.path().join("target/debug/deps");
    std::fs::create_dir_all(&deps).unwrap();
    let rlib = deps.join("libfoo-0123456789abcdef.rlib");
    // Large enough for the file-hash memo to keep a row for it.
    std::fs::write(&rlib, vec![fill; 96 << 10]).unwrap();
    let args = crate::compiler::rustc::RustcCompiler::new()
        .parse(
            &[
                "rustc",
                "--crate-name",
                "foo",
                "--crate-type",
                "lib",
                "--edition",
                "2021",
                "src/lib.rs",
                "--emit=link",
                "-C",
                "metadata=0123456789abcdef",
                "-C",
                "extra-filename=-0123456789abcdef",
                "--out-dir",
                deps.to_str().unwrap(),
            ]
            .map(str::to_string),
        )
        .unwrap();
    let name = "libfoo-0123456789abcdef.rlib".to_string();
    Compiled {
        artifacts: vec![Artifact {
            path: rlib.clone(),
            store_name: name.clone(),
            kind: ArtifactKind::Library,
            required: true,
        }],
        files: vec![(rlib.clone(), name)],
        key: blake3::hash(label.as_bytes()).to_hex().to_string(),
        _dir: dir,
        config,
        args,
        rlib,
    }
}

fn handoff<'a>(
    compiled: &'a Compiled,
    compiler: &'a RustcCompiler,
    start: std::time::Instant,
) -> RustcHandoff<'a> {
    RustcHandoff {
        cache_key: &compiled.key,
        crate_name: "foo",
        target: "x86_64-unknown-linux-gnu",
        profile: "dev",
        args: &compiled.args,
        compiler,
        artifacts: &compiled.artifacts,
        files: &compiled.files,
        stdout: "",
        stderr: "warning: kept\n",
        compile_time_ms: 7,
        event: EventInputs::new("/ws", "foo", EventResult::Miss, 0),
        size: 96 << 10,
        start,
        store_start: start,
    }
}

/// A daemon that answers hand-offs over the socket with its real handler and
/// queues what it accepts, with the publish worker not started: whatever it
/// accepted is pending until the test starts the worker.
struct PendingDaemon {
    daemon: Arc<Daemon>,
    rx: Option<tokio::sync::mpsc::Receiver<PublishJob>>,
    server: tokio::task::JoinHandle<()>,
}

impl PendingDaemon {
    fn listen(config: &Config) -> Self {
        Self::listen_queueing(config, 8)
    }

    /// [`Self::listen`] with room for `capacity` accepted hand-offs.
    fn listen_queueing(config: &Config, capacity: usize) -> Self {
        use interprocess::local_socket::traits::tokio::Listener as _;
        let daemon = Arc::new(Daemon::new(config.clone()));
        let (tx, rx) = tokio::sync::mpsc::channel(capacity);
        daemon.publish_queue().set_sender(tx);
        let socket = config.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = crate::transport::ListenerOptions::new()
            .name(crate::transport::socket_name(&socket).unwrap())
            .create_tokio()
            .unwrap();
        let lifecycle = Arc::new(kunobi_daemon::Lifecycle::default());
        let server = {
            let daemon = Arc::clone(&daemon);
            tokio::spawn(async move {
                while let Ok(stream) = listener.accept().await {
                    let (daemon, lifecycle) = (Arc::clone(&daemon), Arc::clone(&lifecycle));
                    tokio::spawn(async move {
                        let _ = crate::daemon::handle_connection(stream, &daemon, &lifecycle).await;
                    });
                }
            })
        };
        Self {
            daemon,
            rx: Some(rx),
            server,
        }
    }

    /// Start the publish worker and wait until it has stored everything
    /// accepted so far.
    async fn publish_all(&mut self) {
        let worker = self.start_worker();
        self.drain(worker).await;
    }

    /// Start the publish worker; it stores what is queued and keeps taking
    /// hand-offs until [`Self::drain`].
    fn start_worker(&mut self) -> tokio::sync::oneshot::Receiver<()> {
        crate::daemon_publish::spawn_publish_worker(
            Arc::clone(&self.daemon),
            self.rx.take().unwrap(),
        )
        .unwrap()
    }

    /// Close the queue and wait until the worker has stored all of it.
    /// The bound only catches a worker that never finishes: on a machine at
    /// load 225, twelve entries took more than 30 s.
    async fn drain(&self, worker: tokio::sync::oneshot::Receiver<()>) {
        self.daemon.publish_queue().close();
        tokio::time::timeout(Duration::from_secs(120), worker)
            .await
            .expect("the worker drains the queue")
            .unwrap();
    }
}

impl Drop for PendingDaemon {
    fn drop(&mut self) {
        self.server.abort();
    }
}

fn handoff_requests(config: &Config) -> usize {
    std::fs::read_dir(crate::daemon_publish::handoff_dir(config)).map_or(0, Iterator::count)
}

/// How long the tests below hold the index's write lock: long enough that a
/// wrapper that waited for it could not be done before it is let go, and
/// that a loaded test machine still reaches the registration while it is
/// held.
const BUSY: Duration = Duration::from_secs(3);

/// An output rewritten in place after the wrapper returned, its read-only
/// bit ignored (the staged blob shares its inode), is never stored under the
/// compile's key: the daemon finds other bytes than the wrapper hashed and
/// commits nothing, the blob that no longer holds its digest is gone, and
/// the key is free again for the next build to compile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_output_rewritten_after_the_wrapper_returned_is_not_stored() {
    use std::os::unix::fs::PermissionsExt;
    let compiled = compiled("rewritten-after-return");
    let mut daemon = PendingDaemon::listen(&compiled.config);
    let compiled_hash = crate::cache_key::hash_file(&compiled.rlib).unwrap();

    let (outcome, _, _) = store_now(&compiled, Some(BUSY)).await;
    assert!(matches!(outcome, RustcStore::HandedOff));
    std::fs::set_permissions(&compiled.rlib, std::fs::Permissions::from_mode(0o644)).unwrap();
    {
        use std::io::{Seek, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&compiled.rlib)
            .unwrap();
        file.seek(std::io::SeekFrom::Start(1000)).unwrap();
        file.write_all(b"post-processed").unwrap();
    }
    let rewritten = crate::cache_key::hash_file(&compiled.rlib).unwrap();

    daemon.publish_all().await;

    let store = Store::open(&compiled.config).unwrap();
    assert!(store.get(&compiled.key).unwrap().is_none(), "no entry");
    assert!(
        !store.blob_path(&rewritten).exists(),
        "no blob of the new bytes"
    );
    assert!(
        !store.blob_path(&compiled_hash).exists(),
        "no blob under the compiled digest holding other bytes"
    );
    assert!(matches!(
        store.claim_build(&compiled.key).unwrap(),
        BuildClaim::Acquired(_)
    ));
    let events = crate::events::read_events(&compiled.config.event_log_path()).unwrap();
    assert!(
        events[0].store_error.contains("changed after the compile"),
        "{}",
        events[0].store_error
    );
    assert_eq!(handoff_requests(&compiled.config), 0, "no snapshot is left");
}

/// An output replaced by a new file after the wrapper returned does not
/// touch the staged blob's inode: the entry is what the compiler wrote, and
/// the new file gets no memo row under the stored digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_output_replaced_after_the_wrapper_returned_keeps_the_compiled_bytes() {
    let compiled = compiled("replaced-after-return");
    let mut daemon = PendingDaemon::listen(&compiled.config);
    let written = std::fs::read(&compiled.rlib).unwrap();

    let (outcome, _, _) = store_now(&compiled, Some(BUSY)).await;
    assert!(matches!(outcome, RustcStore::HandedOff));
    let replacement = compiled.rlib.with_extension("new");
    std::fs::write(&replacement, vec![b'n'; 96 << 10]).unwrap();
    std::fs::rename(&replacement, &compiled.rlib).unwrap();

    daemon.publish_all().await;

    let store = Store::open(&compiled.config).unwrap();
    let meta = store.get(&compiled.key).unwrap().expect("committed");
    assert_eq!(
        std::fs::read(store.blob_path(&meta.files[0].hash)).unwrap(),
        written
    );
    assert!(matches!(
        store.file_hash_cache().lookup_cached(&compiled.rlib),
        crate::cache_key::FileHashLookup::NeedsHash(_)
    ));
}

/// Without a daemon the wrapper stores the compile itself: the hand-off is
/// not wanted, and offering it anyway on a busy index gives the key back,
/// leaves no link behind, and registers the entry here once the index lets
/// it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_daemon_the_wrapper_stores_itself() {
    let compiled = compiled("no-daemon");
    let store = Store::open(&compiled.config).unwrap();
    assert!(!wanted(&compiled.config, &store));

    let (outcome, lock, _) = store_now(&compiled, Some(BUSY)).await;
    assert!(matches!(outcome, RustcStore::Stored(Ok(_))));
    assert!(lock.is_some(), "the wrapper stores under its own lock");
    assert_eq!(handoff_requests(&compiled.config), 0, "no snapshot is left");
    assert!(store.get(&compiled.key).unwrap().is_some());
}

/// A daemon from before rustc hand-offs answers the unknown request with an
/// error; the wrapper then registers the entry itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_daemon_leaves_the_store_to_the_wrapper() {
    let compiled = compiled("older-daemon");
    let older = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let server = {
        use interprocess::local_socket::traits::tokio::Listener as _;
        let socket = compiled.config.socket_path();
        std::fs::create_dir_all(socket.parent().unwrap()).unwrap();
        let listener = crate::transport::ListenerOptions::new()
            .name(crate::transport::socket_name(&socket).unwrap())
            .create_tokio()
            .unwrap();
        let older = Arc::clone(&older);
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
            while let Ok(stream) = listener.accept().await {
                let mut write = &stream;
                let mut line = String::new();
                let _ = tokio::io::BufReader::new(&stream)
                    .read_line(&mut line)
                    .await;
                older.lock().unwrap().push(line.clone());
                // What an older daemon says to a variant it does not know.
                let _ = write
                    .write_all(b"{\"ok\":false,\"error\":\"invalid request: unknown variant\"}\n")
                    .await;
            }
        })
    };

    let (outcome, lock, _) = store_now(&compiled, Some(BUSY)).await;
    server.abort();
    assert!(matches!(outcome, RustcStore::Stored(Ok(_))));
    assert!(lock.is_some());
    let requests = older.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|line| line.contains("\"publish_rustc\"")),
        "{requests:?}"
    );
    assert_eq!(handoff_requests(&compiled.config), 0, "no snapshot is left");
    let store = Store::open(&compiled.config).unwrap();
    assert!(store.get(&compiled.key).unwrap().is_some());
}

/// A daemon that dies with a rustc store queued commits nothing: dropping
/// the job releases the key, no entry appears, the request directory it left
/// is reclaimed by the handoff sweep, as is one a wrapper left when it died
/// between linking and handing off, and the staged blobs are orphans the
/// orphan sweep reclaims.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_dies_mid_store_leaves_no_entry_and_its_staging_is_swept() {
    let compiled = compiled("daemon-dies");
    let mut daemon = PendingDaemon::listen(&compiled.config);
    let hash = crate::cache_key::hash_file(&compiled.rlib).unwrap();

    let (outcome, _, _) = store_now(&compiled, Some(BUSY)).await;
    assert!(matches!(outcome, RustcStore::HandedOff));
    // The daemon goes away with the job queued: the job, its key lock and
    // its queue are dropped without a put.
    let rx = daemon.rx.take().unwrap();
    drop(rx);
    drop(daemon);
    let store = Store::open(&compiled.config).unwrap();
    // A wrapper that died after linking its blobs, before the hand-off.
    let orphan = crate::daemon_publish::snapshot_staged_blobs(
        &compiled.config,
        &[(store.blob_path(&hash), compiled.files[0].1.clone())],
    )
    .unwrap();
    assert!(handoff_requests(&compiled.config) >= 1);

    assert!(
        store.get(&compiled.key).unwrap().is_none(),
        "nothing committed"
    );
    assert!(
        matches!(
            store.claim_build(&compiled.key).unwrap(),
            BuildClaim::Acquired(_)
        ),
        "the key is free for the next build"
    );
    let swept = crate::daemon_publish::sweep_orphaned_handoffs(&compiled.config, Duration::ZERO);
    assert!(swept.removed >= 1);
    assert_eq!(
        handoff_requests(&compiled.config),
        0,
        "every request directory is gone"
    );
    assert!(!Path::new(&orphan[0].path).exists());
    assert_eq!(store.sweep_orphan_blobs(Duration::ZERO).unwrap().removed, 1);
    assert!(
        !store.blob_path(&hash).exists(),
        "the staged blob is reclaimed"
    );
    assert_eq!(
        std::fs::read(&compiled.rlib).unwrap(),
        vec![b'r'; 96 << 10],
        "the sweeps never touch the output the blob linked"
    );
}

/// Hold the index's write lock from another connection for `hold`, as a
/// burst of stores or a sweep does. The flag is set just before it lets go.
fn hold_index_lock(
    config: &Config,
    hold: Duration,
) -> (
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    hold_index_lock_while(config, move || std::thread::sleep(hold))
}

/// [`hold_index_lock`] until the test lets go: when it sends on the sender,
/// or drops it (a failed assertion lets go too).
fn hold_index_lock_until_let_go(
    config: &Config,
) -> (std::sync::mpsc::Sender<()>, std::thread::JoinHandle<()>) {
    let (let_go, wait) = std::sync::mpsc::channel::<()>();
    let (_, thread) = hold_index_lock_while(config, move || {
        let _ = wait.recv();
    });
    (let_go, thread)
}

/// Hold the index's write lock from another connection while `hold` runs.
fn hold_index_lock_while(
    config: &Config,
    hold: impl FnOnce() + Send + 'static,
) -> (
    Arc<std::sync::atomic::AtomicBool>,
    std::thread::JoinHandle<()>,
) {
    drop(Store::open(config).unwrap());
    let index = config.cache_dir.join("index.db");
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (ready, held) = std::sync::mpsc::channel();
    let thread = {
        let released = Arc::clone(&released);
        std::thread::spawn(move || {
            let writer = rusqlite::Connection::open(index).unwrap();
            writer.execute_batch("BEGIN IMMEDIATE").unwrap();
            ready.send(()).unwrap();
            hold();
            released.store(true, std::sync::atomic::Ordering::SeqCst);
            writer.execute_batch("ROLLBACK").unwrap();
        })
    };
    held.recv().unwrap();
    (released, thread)
}

/// Store a compile as the wrapper does after a miss, with the index's write
/// lock held elsewhere for `busy` when given. Also says whether the wrapper
/// was done while that lock was still held.
async fn store_now(
    compiled: &Compiled,
    busy: Option<Duration>,
) -> (RustcStore, Option<KeyLock>, bool) {
    tokio::task::block_in_place(|| {
        let store = Store::open(&compiled.config).unwrap();
        let compiler = RustcCompiler::new();
        let mut lock = match store.claim_build(&compiled.key).unwrap() {
            BuildClaim::Acquired(lock) => Some(lock),
            _ => panic!("a fresh key"),
        };
        let held = busy.map(|hold| hold_index_lock(&compiled.config, hold));
        let outcome = store_within(
            &compiled.config,
            &store,
            &mut lock,
            handoff(compiled, &compiler, std::time::Instant::now()),
            Duration::from_secs(10),
            true,
        );
        let while_held = held.is_some_and(|(released, thread)| {
            let while_held = !released.load(std::sync::atomic::Ordering::SeqCst);
            thread.join().unwrap();
            while_held
        });
        (outcome, lock, while_held)
    })
}

/// With a daemon running and the index free, a miss stores as it always
/// did: before the wrapper returns, under its own key lock, with nothing
/// handed to the daemon.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_a_free_index_the_miss_is_stored_before_the_wrapper_returns() {
    let compiled = compiled("free-index");
    let daemon = PendingDaemon::listen(&compiled.config);
    let written = std::fs::read(&compiled.rlib).unwrap();

    let (outcome, lock, _) = store_now(&compiled, None).await;
    assert!(
        matches!(outcome, RustcStore::Stored(Ok(_))),
        "stored by the wrapper, not handed off"
    );
    assert!(lock.is_some(), "the wrapper kept its key lock");
    let store = Store::open(&compiled.config).unwrap();
    let meta = store
        .get(&compiled.key)
        .unwrap()
        .expect("visible when the wrapper returns");
    assert_eq!(
        std::fs::read(store.blob_path(&meta.files[0].hash)).unwrap(),
        written
    );
    assert_eq!(daemon.daemon.publish_queue().progress().pending(), 0);
    assert_eq!(
        handoff_requests(&compiled.config),
        0,
        "no snapshot was taken"
    );
}

/// With the index's write lock held by another connection, the wrapper does
/// not wait for it: it hands the staged entry to the daemon and returns
/// while the lock is still held. Nothing is committed and a peer waits on
/// the key until the daemon registers the entry, which then holds exactly
/// the bytes the compiler wrote; the event is the daemon's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn on_a_busy_index_the_wrapper_returns_and_the_daemon_commits_after() {
    let compiled = compiled("busy-index");
    let mut daemon = PendingDaemon::listen(&compiled.config);
    let written = std::fs::read(&compiled.rlib).unwrap();

    let (outcome, lock, while_held) = store_now(&compiled, Some(BUSY)).await;
    assert!(matches!(outcome, RustcStore::HandedOff));
    assert!(
        while_held,
        "the wrapper returned while another connection still held the index"
    );
    assert!(lock.is_none(), "the daemon holds the key now");

    let store = Store::open(&compiled.config).unwrap();
    assert!(
        store.get(&compiled.key).unwrap().is_none(),
        "nothing committed yet"
    );
    assert!(!store.contains(&compiled.key));
    assert!(
        matches!(
            store.claim_build(&compiled.key).unwrap(),
            BuildClaim::Contended
        ),
        "a peer finds the key held and waits for the commit"
    );
    assert!(
        crate::events::read_events(&compiled.config.event_log_path())
            .map_or(true, |events| events.is_empty()),
        "the event waits for the store"
    );

    daemon.publish_all().await;

    let meta = store.get(&compiled.key).unwrap().expect("committed");
    assert_eq!(meta.files.len(), 1);
    assert_eq!(meta.stderr, "warning: kept\n");
    assert_eq!(
        std::fs::read(store.blob_path(&meta.files[0].hash)).unwrap(),
        written
    );
    assert!(
        matches!(
            store.claim_build(&compiled.key).unwrap(),
            BuildClaim::Committed(_)
        ),
        "a peer now restores the entry"
    );
    // The output shares the read-only blob, as after a synchronous put, and
    // its memo row lets dependents skip hashing it.
    assert!(
        kache_fs::file_identity(&compiled.rlib).unwrap()
            == kache_fs::file_identity(&store.blob_path(&meta.files[0].hash)).unwrap()
    );
    assert!(matches!(
        store.file_hash_cache().lookup_cached(&compiled.rlib),
        crate::cache_key::FileHashLookup::Hit(hash) if hash == meta.files[0].hash
    ));
    assert!(
        store
            .file_hash_cache()
            .has_entry_for_unit("foo", "0123456789abcdef")
            .unwrap(),
        "the entry records its unit"
    );
    let events = crate::events::read_events(&compiled.config.event_log_path()).unwrap();
    assert_eq!(events.len(), 1);
    assert!(events[0].store_handed_off);
    assert_eq!(events[0].result, EventResult::Miss);
    assert!(
        events[0].store_error.is_empty(),
        "{}",
        events[0].store_error
    );
    assert_eq!(handoff_requests(&compiled.config), 0, "no snapshot is left");
}

/// Store a compile as the wrapper does after a miss, with whatever the
/// caller holds of the index; how long the store took the wrapper.
fn hand_off_now(compiled: &Compiled) -> (RustcStore, Option<KeyLock>, Duration) {
    tokio::task::block_in_place(|| {
        let store = Store::open(&compiled.config).unwrap();
        let compiler = RustcCompiler::new();
        let mut lock = match store.claim_build(&compiled.key).unwrap() {
            BuildClaim::Acquired(lock) => Some(lock),
            _ => panic!("a fresh key"),
        };
        let started = std::time::Instant::now();
        let outcome = store_within(
            &compiled.config,
            &store,
            &mut lock,
            handoff(compiled, &compiler, started),
            Duration::from_secs(10),
            true,
        );
        (outcome, lock, started.elapsed())
    })
}

/// The events the daemon logged, as written: field by field, whatever this
/// release's `BuildEvent` knows.
fn raw_events(config: &Config) -> Vec<serde_json::Value> {
    std::fs::read_to_string(config.event_log_path())
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

/// Hand-offs that queue while another connection holds the index's write
/// lock are registered together once it lets go: one transaction for all of
/// them, so a lock that is free only now and then is taken once, not once
/// per entry. Each entry is the compiler's bytes, and each event says how
/// many entries its transaction committed.
///
/// The test lets go of the lock itself once it has seen every hand-off
/// queued and started the worker, not after a fixed time: a loaded machine
/// took longer than any such time for the hand-offs alone.
#[test]
fn hand_offs_queued_on_a_busy_index_commit_in_one_transaction() {
    const COUNT: usize = 12;
    let (_root, compiled) = compiled_many(COUNT);
    crate::test_support::on_a_runtime_of_its_own(
        crate::test_support::Flavor::MultiThread(4),
        async {
            let config = &compiled[0].config;
            let mut daemon = PendingDaemon::listen_queueing(config, 64);

            let (let_go, holder) = hold_index_lock_until_let_go(config);
            for one in &compiled {
                let (outcome, lock, _) = hand_off_now(one);
                assert!(matches!(outcome, RustcStore::HandedOff));
                assert!(lock.is_none());
            }
            let progress = daemon.daemon.publish_queue().progress();
            assert_eq!(
                (progress.pending(), progress.done()),
                (COUNT, 0),
                "every hand-off queued while the index was held"
            );
            let worker = daemon.start_worker();
            let_go.send(()).unwrap();
            tokio::task::block_in_place(|| holder.join().unwrap());
            daemon.drain(worker).await;

            let store = Store::open(config).unwrap();
            for one in &compiled {
                let meta = store.get(&one.key).unwrap().expect("committed");
                assert_eq!(
                    std::fs::read(store.blob_path(&meta.files[0].hash)).unwrap(),
                    std::fs::read(&one.rlib).unwrap()
                );
            }
            let events = raw_events(config);
            assert_eq!(events.len(), COUNT);
            for event in &events {
                assert_eq!(event["store_error"], serde_json::Value::Null, "{event}");
                assert_eq!(event["daemon_store_batch"], COUNT, "{event}");
            }
            assert_eq!(handoff_requests(config), 0, "no snapshot is left");
        },
    );
}

/// An output rewritten in place after its wrapper returned costs its own
/// entry only: the rest of the batch commits, and the changed blob is gone.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_rewritten_output_costs_only_its_own_entry_in_a_batch() {
    use std::os::unix::fs::PermissionsExt;
    const COUNT: usize = 6;
    let (_root, compiled) = compiled_many(COUNT);
    let config = &compiled[0].config;
    let mut daemon = PendingDaemon::listen_queueing(config, 64);
    let bad = &compiled[2];
    let bad_hash = crate::cache_key::hash_file(&bad.rlib).unwrap();

    let (_, holder) = hold_index_lock(config, BUSY);
    for one in &compiled {
        assert!(matches!(hand_off_now(one).0, RustcStore::HandedOff));
    }
    std::fs::set_permissions(&bad.rlib, std::fs::Permissions::from_mode(0o644)).unwrap();
    {
        use std::io::{Seek, Write};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .open(&bad.rlib)
            .unwrap();
        file.seek(std::io::SeekFrom::Start(1000)).unwrap();
        file.write_all(b"post-processed").unwrap();
    }
    let worker = daemon.start_worker();
    tokio::task::block_in_place(|| holder.join().unwrap());
    daemon.drain(worker).await;

    let store = Store::open(config).unwrap();
    assert!(store.get(&bad.key).unwrap().is_none(), "no entry");
    assert!(!store.blob_path(&bad_hash).exists(), "no changed blob");
    assert!(matches!(
        store.claim_build(&bad.key).unwrap(),
        BuildClaim::Acquired(_)
    ));
    for one in compiled.iter().filter(|one| one.key != bad.key) {
        assert!(store.get(&one.key).unwrap().is_some(), "committed");
    }
    let events = raw_events(config);
    assert_eq!(events.len(), COUNT);
    let (failed, stored): (Vec<_>, Vec<_>) = events
        .iter()
        .partition(|event| event["store_error"].is_string());
    assert_eq!(failed.len(), 1);
    assert!(
        failed[0]["store_error"]
            .as_str()
            .unwrap()
            .contains("changed after the compile"),
        "{}",
        failed[0]
    );
    for event in stored {
        assert_eq!(event["daemon_store_batch"], COUNT - 1, "{event}");
    }
    assert_eq!(handoff_requests(config), 0, "no snapshot is left");
}

/// The daemon answers a hand-off at once whatever else it is doing: its
/// worker waiting for the index's write lock with an entry, and its own
/// store connection taken by other work, cost the wrapper nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_daemon_answers_a_hand_off_while_its_worker_and_store_are_busy() {
    let (_root, compiled) = compiled_many(2);
    let config = &compiled[0].config;
    let mut daemon = PendingDaemon::listen_queueing(config, 64);
    let (released, holder) = hold_index_lock(config, BUSY);
    assert!(matches!(
        hand_off_now(&compiled[0]).0,
        RustcStore::HandedOff
    ));
    let worker = daemon.start_worker();
    // Other work holds the daemon's store connection for longer than any
    // wrapper should wait.
    let (take, taken) = std::sync::mpsc::channel();
    let busy_store = {
        let daemon = Arc::clone(&daemon.daemon);
        std::thread::spawn(move || {
            daemon
                .with_store(|_| {
                    take.send(()).unwrap();
                    std::thread::sleep(BUSY);
                    Ok(())
                })
                .unwrap();
        })
    };
    taken.recv().unwrap();

    let (outcome, _, took) = hand_off_now(&compiled[1]);
    assert!(matches!(outcome, RustcStore::HandedOff));
    assert!(took < Duration::from_secs(1), "the wrapper waited {took:?}");
    assert!(
        !released.load(std::sync::atomic::Ordering::SeqCst),
        "answered while the worker still waited for the index"
    );

    tokio::task::block_in_place(|| {
        holder.join().unwrap();
        busy_store.join().unwrap();
    });
    daemon.drain(worker).await;
    let store = Store::open(config).unwrap();
    for one in &compiled {
        assert!(store.get(&one.key).unwrap().is_some(), "committed");
    }
}
