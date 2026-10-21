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
    let deps = dir.path().join("target/debug/deps");
    std::fs::create_dir_all(&deps).unwrap();
    let rlib = deps.join("libfoo-0123456789abcdef.rlib");
    // Large enough for the file-hash memo to keep a row for it.
    std::fs::write(&rlib, vec![b'r'; 96 << 10]).unwrap();
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
        use interprocess::local_socket::traits::tokio::Listener as _;
        let daemon = Arc::new(Daemon::new(config.clone()));
        let (tx, rx) = tokio::sync::mpsc::channel(8);
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
        let worker = crate::daemon_publish::spawn_publish_worker(
            Arc::clone(&self.daemon),
            self.rx.take().unwrap(),
        )
        .unwrap();
        self.daemon.publish_queue().close();
        tokio::time::timeout(Duration::from_secs(30), worker)
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

async fn hand_off_now(compiled: &Compiled) -> (CcHandoffOutcome, Option<KeyLock>) {
    // The wrapper is a blocking process; the daemon answers on this runtime's
    // other workers meanwhile.
    tokio::task::block_in_place(|| {
        let store = Store::open(&compiled.config).unwrap();
        let compiler = RustcCompiler::new();
        let mut lock = match store.claim_build(&compiled.key).unwrap() {
            BuildClaim::Acquired(lock) => Some(lock),
            _ => panic!("a fresh key"),
        };
        // A loaded test machine can take longer than the production budget
        // to answer; what these tests pin is what happens once it has.
        let outcome = hand_off_within(
            &compiled.config,
            &store,
            &mut lock,
            handoff(compiled, &compiler, std::time::Instant::now()),
            Duration::from_secs(10),
        );
        (outcome, lock)
    })
}

fn handoff_requests(config: &Config) -> usize {
    std::fs::read_dir(crate::daemon_publish::handoff_dir(config)).map_or(0, Iterator::count)
}

/// The wrapper is done once the daemon accepted: nothing is committed yet,
/// and a peer looking the key up meanwhile finds no entry (never a partial
/// one) and the key held, so it waits. Once the daemon stores, the entry
/// holds exactly the bytes the compiler wrote, and the event is the daemon's.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn the_wrapper_returns_before_the_store_commits_and_a_peer_waits() {
    let compiled = compiled("returns-before-commit");
    let mut daemon = PendingDaemon::listen(&compiled.config);
    let written = std::fs::read(&compiled.rlib).unwrap();

    let (outcome, lock) = hand_off_now(&compiled).await;
    assert!(matches!(outcome, CcHandoffOutcome::Accepted));
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
        !store
            .wait_for_committed_with_timeout(&compiled.key, Duration::from_millis(50))
            .unwrap(),
        "and finds nothing while the store is pending"
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

/// An output rewritten in place after the wrapper returned (the snapshot is
/// a link to the same inode) is never stored under the compile's key: the
/// daemon's put sees other bytes than the wrapper hashed and commits
/// nothing, and the key is free again for the next build to compile.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_output_rewritten_after_the_wrapper_returned_is_not_stored() {
    let compiled = compiled("rewritten-after-return");
    let mut daemon = PendingDaemon::listen(&compiled.config);

    let (outcome, _) = hand_off_now(&compiled).await;
    assert!(matches!(outcome, CcHandoffOutcome::Accepted));
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
/// touch the snapshot's inode: the entry is what the compiler wrote, and the
/// new file gets no memo row under the stored digest.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn an_output_replaced_after_the_wrapper_returned_keeps_the_compiled_bytes() {
    let compiled = compiled("replaced-after-return");
    let mut daemon = PendingDaemon::listen(&compiled.config);
    let written = std::fs::read(&compiled.rlib).unwrap();

    let (outcome, _) = hand_off_now(&compiled).await;
    assert!(matches!(outcome, CcHandoffOutcome::Accepted));
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
/// not wanted, and offering it anyway gives the key back and leaves no
/// snapshot behind.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_a_daemon_the_wrapper_stores_itself() {
    let compiled = compiled("no-daemon");
    let store = Store::open(&compiled.config).unwrap();
    assert!(!wanted(&compiled.config, &store));

    let (outcome, lock) = hand_off_now(&compiled).await;
    assert!(matches!(outcome, CcHandoffOutcome::Publish));
    assert!(lock.is_some(), "the wrapper stores under its own lock");
    assert_eq!(handoff_requests(&compiled.config), 0, "no snapshot is left");
    assert!(store.get(&compiled.key).unwrap().is_none());
}

/// A daemon from before rustc hand-offs answers the unknown request with an
/// error; the wrapper then stores the compile itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_older_daemon_leaves_the_store_to_the_wrapper() {
    let compiled = compiled("older-daemon");
    let older = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let server = {
        use interprocess::local_socket::traits::tokio::{Listener as _, Stream as _};
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
                let (read, mut write) = stream.split();
                let mut line = String::new();
                let _ = tokio::io::BufReader::new(read).read_line(&mut line).await;
                older.lock().unwrap().push(line.clone());
                // What an older daemon says to a variant it does not know.
                let _ = write
                    .write_all(b"{\"ok\":false,\"error\":\"invalid request: unknown variant\"}\n")
                    .await;
            }
        })
    };

    let (outcome, lock) = hand_off_now(&compiled).await;
    server.abort();
    assert!(matches!(outcome, CcHandoffOutcome::Publish));
    assert!(lock.is_some());
    let requests = older.lock().unwrap();
    assert!(
        requests
            .iter()
            .any(|line| line.contains("\"publish_rustc\"")),
        "{requests:?}"
    );
    assert_eq!(handoff_requests(&compiled.config), 0, "no snapshot is left");
}

/// A daemon that dies with a rustc store queued commits nothing: dropping
/// the job releases the key, no entry appears, and the request directory it
/// left is reclaimed by the handoff sweep, as is one a wrapper left when it
/// died between snapshotting and handing off.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_daemon_that_dies_mid_store_leaves_no_entry_and_its_staging_is_swept() {
    let compiled = compiled("daemon-dies");
    let mut daemon = PendingDaemon::listen(&compiled.config);

    let (outcome, _) = hand_off_now(&compiled).await;
    assert!(matches!(outcome, CcHandoffOutcome::Accepted));
    // The daemon goes away with the job queued: the job, its key lock and
    // its queue are dropped without a put.
    let rx = daemon.rx.take().unwrap();
    drop(rx);
    drop(daemon);
    // A wrapper that died after its snapshot, before the hand-off.
    let (orphan, _) =
        crate::daemon_publish::snapshot_rustc_outputs(&compiled.config, &compiled.files).unwrap();
    assert!(handoff_requests(&compiled.config) >= 1);

    let store = Store::open(&compiled.config).unwrap();
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
    assert_eq!(
        std::fs::read(&compiled.rlib).unwrap(),
        vec![b'r'; 96 << 10],
        "the sweep never touches the output a snapshot linked"
    );
}
