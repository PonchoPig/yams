use std::fs::{self, Permissions};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use tempfile::{TempDir, tempdir};
use yams_wiki::{
    LOCK_NAME, LOCK_TIMEOUT, LockError, LockLease, LockMode, UnisolatedReason, acquire_lock,
    acquire_lock_with_timeout,
};

const HELPER_ENV: &str = "MEMORY_WIKI_LOCK_HELPER";
const HELPER_CORPUS_ENV: &str = "MEMORY_WIKI_LOCK_HELPER_CORPUS";
const HELPER_MODE_ENV: &str = "MEMORY_WIKI_LOCK_HELPER_MODE";
const HELPER_ACTION_ENV: &str = "MEMORY_WIKI_LOCK_HELPER_ACTION";
const HELPER_CORPUS_ID_ENV: &str = "MEMORY_WIKI_LOCK_HELPER_CORPUS_ID";
const HELPER_LOCK_ID_ENV: &str = "MEMORY_WIKI_LOCK_HELPER_LOCK_ID";
const READY: &str = "MEMORY_WIKI_LOCK_READY";

fn isolated(lease: LockLease) -> yams_wiki::LockGuard {
    match lease {
        LockLease::Isolated(guard) => guard,
        LockLease::Unisolated(unisolated) => {
            panic!("expected isolated lease, got {unisolated:?}")
        }
    }
}

fn lock_path(corpus: &Path) -> PathBuf {
    corpus.canonicalize().unwrap().join(LOCK_NAME)
}

fn file_id(path: &Path) -> String {
    let metadata = fs::metadata(path).unwrap();
    format!("{}:{}", metadata.dev(), metadata.ino())
}

fn own_fd_dir() -> &'static Path {
    let dev = Path::new("/dev/fd");
    if dev.is_dir() {
        dev
    } else {
        Path::new("/proc/self/fd")
    }
}

fn helper_assert_fds_closed(corpus_id: &str, lock_id: &str) {
    assert_fd_identity_closed(corpus_id, "corpus directory fd survived exec");
    assert_fd_identity_closed(lock_id, "lock fd survived exec");
}

fn assert_fd_identity_closed(identity: &str, message: &str) {
    for entry in fs::read_dir(own_fd_dir()).unwrap() {
        let entry = entry.unwrap();
        let Ok(metadata) = fs::metadata(entry.path()) else {
            continue;
        };
        let found = format!("{}:{}", metadata.dev(), metadata.ino());
        assert_ne!(found, identity, "{message}");
    }
}

#[test]
fn memory_wiki_lock_helper() {
    if std::env::var_os(HELPER_ENV).is_none() {
        return;
    }

    let action = std::env::var(HELPER_ACTION_ENV).unwrap();
    let lease = if action == "hold" {
        let corpus = PathBuf::from(std::env::var_os(HELPER_CORPUS_ENV).unwrap());
        let mode = match std::env::var(HELPER_MODE_ENV).unwrap().as_str() {
            "shared" => LockMode::Shared,
            "exclusive" => LockMode::Exclusive,
            other => panic!("unknown helper lock mode {other}"),
        };
        Some(isolated(acquire_lock(&corpus, mode).unwrap()))
    } else {
        assert_eq!(action, "idle");
        helper_assert_fds_closed(
            &std::env::var(HELPER_CORPUS_ID_ENV).unwrap(),
            &std::env::var(HELPER_LOCK_ID_ENV).unwrap(),
        );
        None
    };

    println!("{READY}");
    std::io::stdout().flush().unwrap();
    let mut release = String::new();
    std::io::stdin().read_line(&mut release).unwrap();
    assert_eq!(release, "release\n");
    drop(lease);
}

struct Helper {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    _stdout: BufReader<ChildStdout>,
}

impl Helper {
    fn spawn_holder(corpus: &Path, mode: LockMode) -> Self {
        let mode = match mode {
            LockMode::Shared => "shared",
            LockMode::Exclusive => "exclusive",
        };
        Self::spawn(
            "hold",
            [
                (HELPER_CORPUS_ENV, corpus.as_os_str()),
                (HELPER_MODE_ENV, std::ffi::OsStr::new(mode)),
            ],
        )
    }

    fn spawn_idle(corpus: &Path) -> Self {
        let path = lock_path(corpus);
        let corpus_id = file_id(&corpus.canonicalize().unwrap());
        let lock_id = file_id(&path);
        Self::spawn(
            "idle",
            [
                (HELPER_CORPUS_ID_ENV, std::ffi::OsStr::new(&corpus_id)),
                (HELPER_LOCK_ID_ENV, std::ffi::OsStr::new(&lock_id)),
                (HELPER_CORPUS_ENV, corpus.as_os_str()),
            ],
        )
    }

    fn spawn<const N: usize>(action: &str, environment: [(&str, &std::ffi::OsStr); N]) -> Self {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "memory_wiki_lock_helper",
                "--nocapture",
                "--test-threads=1",
            ])
            .env(HELPER_ENV, "1")
            .env(HELPER_ACTION_ENV, action)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        for (key, value) in environment {
            command.env(key, value);
        }

        let mut child = command.spawn().unwrap();
        let stdin = child.stdin.take().unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        loop {
            line.clear();
            let read = stdout.read_line(&mut line).unwrap();
            if read == 0 {
                let status = child.wait().unwrap();
                panic!("lock helper exited before ready: {status}");
            }
            if line.contains(READY) {
                break;
            }
        }

        Self {
            child: Some(child),
            stdin: Some(stdin),
            _stdout: stdout,
        }
    }

    fn release(mut self) {
        writeln!(self.stdin.as_mut().unwrap(), "release").unwrap();
        drop(self.stdin.take());
        let status = self.child.as_mut().unwrap().wait().unwrap();
        assert!(status.success(), "lock helper failed: {status}");
        self.child = None;
    }

    fn kill(mut self) {
        self.child.as_mut().unwrap().kill().unwrap();
        let status = self.child.as_mut().unwrap().wait().unwrap();
        assert!(!status.success(), "killed helper unexpectedly succeeded");
        self.child = None;
    }
}

impl Drop for Helper {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

struct RestoreModes(Vec<(PathBuf, Permissions)>);

impl RestoreModes {
    fn new(paths: impl IntoIterator<Item = PathBuf>) -> Self {
        Self(
            paths
                .into_iter()
                .map(|path| {
                    let permissions = fs::metadata(&path).unwrap().permissions();
                    (path, permissions)
                })
                .collect(),
        )
    }
}

impl Drop for RestoreModes {
    fn drop(&mut self) {
        for (path, permissions) in &self.0 {
            let _ = fs::set_permissions(path, permissions.clone());
        }
    }
}

fn writable_corpus() -> (TempDir, PathBuf) {
    let tmp = tempdir().unwrap();
    let corpus = tmp.path().join("memory");
    fs::create_dir(&corpus).unwrap();
    fs::create_dir(corpus.join("pages")).unwrap();
    (tmp, corpus)
}

#[test]
fn lock_name_and_default_timeout_are_contract_values() {
    assert_eq!(LOCK_NAME, ".write.lock");
    assert_eq!(LOCK_TIMEOUT.as_secs(), 10);
}

#[test]
fn two_process_real_shared_holders_coexist() {
    let (_tmp, corpus) = writable_corpus();
    let first = Helper::spawn_holder(&corpus, LockMode::Shared);
    let second = Helper::spawn_holder(&corpus, LockMode::Shared);

    second.release();
    first.release();
}

#[test]
fn a_process_real_exclusive_holder_excludes_both_modes() {
    let (_tmp, corpus) = writable_corpus();
    let holder = Helper::spawn_holder(&corpus, LockMode::Exclusive);

    for mode in [LockMode::Shared, LockMode::Exclusive] {
        assert!(matches!(
            acquire_lock_with_timeout(&corpus, mode, Duration::ZERO),
            Err(LockError::Busy { mode: found, .. }) if found == mode
        ));
    }

    holder.release();
}

#[test]
fn a_process_real_shared_holder_excludes_an_exclusive_holder() {
    let (_tmp, corpus) = writable_corpus();
    let holder = Helper::spawn_holder(&corpus, LockMode::Shared);

    assert!(matches!(
        acquire_lock_with_timeout(&corpus, LockMode::Exclusive, Duration::ZERO),
        Err(LockError::Busy {
            mode: LockMode::Exclusive,
            ..
        })
    ));

    holder.release();
}

#[test]
fn timeout_is_bounded_and_reports_the_canonical_lock_path() {
    let (_tmp, corpus) = writable_corpus();
    let holder = Helper::spawn_holder(&corpus, LockMode::Exclusive);
    let timeout = Duration::from_millis(35);
    let started = Instant::now();

    let error = acquire_lock_with_timeout(&corpus, LockMode::Shared, timeout).unwrap_err();
    let elapsed = started.elapsed();

    assert!(elapsed >= timeout, "returned early after {elapsed:?}");
    assert!(
        elapsed < Duration::from_millis(500),
        "overshot: {elapsed:?}"
    );
    assert!(matches!(
        error,
        LockError::Busy {
            ref path,
            mode: LockMode::Shared,
            timeout: found,
        } if path == &lock_path(&corpus) && found == timeout
    ));
    holder.release();
}

#[test]
fn dropped_and_killed_holders_release_without_stale_state() {
    let (_tmp, corpus) = writable_corpus();
    let guard = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
    drop(guard);
    drop(isolated(
        acquire_lock_with_timeout(&corpus, LockMode::Exclusive, Duration::from_secs(1)).unwrap(),
    ));

    let holder = Helper::spawn_holder(&corpus, LockMode::Exclusive);
    holder.kill();
    drop(isolated(
        acquire_lock_with_timeout(&corpus, LockMode::Exclusive, Duration::from_secs(1)).unwrap(),
    ));

    assert!(
        lock_path(&corpus).is_file(),
        "the persistent lock was removed"
    );
}

#[test]
fn new_lock_is_private_single_link_and_existing_read_only_lock_is_preserved() {
    let (_tmp, corpus) = writable_corpus();
    drop(isolated(acquire_lock(&corpus, LockMode::Shared).unwrap()));
    let path = lock_path(&corpus);
    let metadata = fs::symlink_metadata(&path).unwrap();
    assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
    assert_eq!(metadata.nlink(), 1);

    fs::write(&path, b"caller bytes").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o444)).unwrap();
    let holder = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
    assert_eq!(
        fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    assert!(matches!(
        acquire_lock_with_timeout(&corpus, LockMode::Shared, Duration::ZERO),
        Err(LockError::Busy { .. })
    ));
    drop(holder);
    assert_eq!(
        fs::symlink_metadata(path).unwrap().permissions().mode() & 0o777,
        0o444
    );
    assert_eq!(fs::read(lock_path(&corpus)).unwrap(), b"caller bytes");
}

#[test]
fn symlinked_and_dangling_lock_paths_are_unsafe_and_untouched() {
    for dangling in [false, true] {
        let (_tmp, corpus) = writable_corpus();
        let target = corpus.join("target");
        if !dangling {
            fs::write(&target, b"caller bytes").unwrap();
        }
        let path = corpus.join(LOCK_NAME);
        let reported_path = lock_path(&corpus);
        symlink(&target, &path).unwrap();
        let link_target = fs::read_link(&path).unwrap();

        let result = acquire_lock(&corpus, LockMode::Exclusive);
        assert!(
            matches!(
                result,
                Err(LockError::Unsafe { path: ref found, .. }) if found == &reported_path
            ),
            "unexpected result: {result:?}"
        );

        assert_eq!(fs::read_link(&path).unwrap(), link_target);
        if !dangling {
            assert_eq!(fs::read(&target).unwrap(), b"caller bytes");
        }
    }
}

#[test]
fn directory_fifo_and_hardlink_lock_objects_are_unsafe_and_untouched() {
    let hostile = ["directory", "fifo", "hardlink"];
    for kind in hostile {
        let (_tmp, corpus) = writable_corpus();
        let path = corpus.join(LOCK_NAME);
        let reported_path = lock_path(&corpus);
        let victim = corpus.join("victim");
        match kind {
            "directory" => {
                fs::create_dir(&path).unwrap();
                fs::write(path.join("sentinel"), b"caller bytes").unwrap();
            }
            "fifo" => {
                assert!(
                    Command::new("mkfifo")
                        .arg(&path)
                        .status()
                        .unwrap()
                        .success()
                );
                assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());
            }
            "hardlink" => {
                fs::write(&victim, b"caller bytes").unwrap();
                fs::hard_link(&victim, &path).unwrap();
            }
            _ => unreachable!(),
        }

        assert!(matches!(
            acquire_lock(&corpus, LockMode::Exclusive),
            Err(LockError::Unsafe { path: ref found, .. }) if found == &reported_path
        ));

        match kind {
            "directory" => assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"caller bytes"),
            "fifo" => assert!(fs::symlink_metadata(path).unwrap().file_type().is_fifo()),
            "hardlink" => {
                assert_eq!(fs::read(victim).unwrap(), b"caller bytes");
                assert_eq!(fs::symlink_metadata(path).unwrap().nlink(), 2);
            }
            _ => unreachable!(),
        }
    }
}

#[test]
fn absent_and_non_directory_corpora_are_unsafe_without_creating_objects() {
    let tmp = tempdir().unwrap();
    for kind in ["absent", "file"] {
        let corpus = tmp.path().join(kind);
        if kind == "file" {
            fs::write(&corpus, b"caller bytes").unwrap();
        }

        assert!(matches!(
            acquire_lock(&corpus, LockMode::Shared),
            Err(LockError::Unsafe { ref path, .. }) if path == &corpus
        ));
        assert!(!corpus.join(LOCK_NAME).exists());
        if kind == "file" {
            assert_eq!(fs::read(corpus).unwrap(), b"caller bytes");
        }
    }
}

#[test]
fn denied_create_degrades_only_when_lock_is_absent_and_pages_are_unwritable() {
    let (_tmp, corpus) = writable_corpus();
    if fs::metadata(&corpus).unwrap().uid() == 0 {
        return;
    }
    let pages = corpus.join("pages");
    let _restore = RestoreModes::new([corpus.clone(), pages.clone()]);

    fs::set_permissions(&pages, Permissions::from_mode(0o777)).unwrap();
    fs::set_permissions(&corpus, Permissions::from_mode(0o555)).unwrap();
    assert!(matches!(
        acquire_lock(&corpus, LockMode::Exclusive),
        Err(LockError::Unsafe { .. })
    ));
    fs::write(pages.join("still-writable"), b"x").unwrap();

    fs::set_permissions(&pages, Permissions::from_mode(0o555)).unwrap();
    let lease = acquire_lock(&corpus, LockMode::Exclusive).unwrap();
    assert!(matches!(
        lease,
        LockLease::Unisolated(ref unisolated)
            if unisolated.reason == UnisolatedReason::UnwritableCorpus
    ));
    assert!(!corpus.join(LOCK_NAME).exists());
}

#[test]
fn an_existing_unreadable_lock_is_never_degraded() {
    let (_tmp, corpus) = writable_corpus();
    if fs::metadata(&corpus).unwrap().uid() == 0 {
        return;
    }
    let pages = corpus.join("pages");
    let path = corpus.join(LOCK_NAME);
    let reported_path = lock_path(&corpus);
    fs::write(&path, b"caller bytes").unwrap();
    let _restore = RestoreModes::new([pages.clone(), path.clone()]);
    fs::set_permissions(&pages, Permissions::from_mode(0o555)).unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o000)).unwrap();

    assert!(matches!(
        acquire_lock(&corpus, LockMode::Exclusive),
        Err(LockError::Unsafe { path: ref found, .. }) if found == &reported_path
    ));
    fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
    assert_eq!(fs::read(path).unwrap(), b"caller bytes");
}

#[test]
fn foreign_owned_lock_is_unsafe_where_the_host_can_construct_it() {
    let (_tmp, corpus) = writable_corpus();
    if fs::metadata(&corpus).unwrap().uid() != 0 {
        return;
    }
    let path = corpus.join(LOCK_NAME);
    let reported_path = lock_path(&corpus);
    fs::write(&path, b"caller bytes").unwrap();
    assert!(
        Command::new("chown")
            .arg("1")
            .arg(&path)
            .status()
            .unwrap()
            .success()
    );

    assert!(matches!(
        acquire_lock(&corpus, LockMode::Shared),
        Err(LockError::Unsafe { path: ref found, .. }) if found == &reported_path
    ));
    assert_eq!(fs::read(path).unwrap(), b"caller bytes");
}

#[test]
fn corpus_and_lock_descriptors_are_close_on_exec() {
    let (_tmp, corpus) = writable_corpus();
    let guard = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
    let helper = Helper::spawn_idle(&corpus);

    drop(guard);
    drop(isolated(
        acquire_lock_with_timeout(&corpus, LockMode::Exclusive, Duration::from_secs(1)).unwrap(),
    ));
    helper.release();
}

#[test]
fn unsafe_acquisition_does_not_leak_a_descriptor() {
    let (_tmp, corpus) = writable_corpus();
    let path = corpus.join(LOCK_NAME);
    symlink(corpus.join("missing"), &path).unwrap();
    let corpus_id = file_id(&corpus.canonicalize().unwrap());

    for _ in 0..20 {
        assert!(matches!(
            acquire_lock(&corpus, LockMode::Exclusive),
            Err(LockError::Unsafe { .. })
        ));
    }

    assert_fd_identity_closed(&corpus_id, "unsafe acquisition leaked the corpus fd");
}
