use std::ffi::{OsStr, OsString};
use std::path::PathBuf;

use yams_cli::{
    DirsOverride, Environment, Platform, ResolvedDirsOverride, RuntimeInputs, RuntimeLayout,
    prepare_direct,
};

fn inputs() -> RuntimeInputs {
    RuntimeInputs {
        cwd: PathBuf::from("/fictional/work/copper-garden"),
        home: PathBuf::from("/fictional/home/alex"),
        temporary_directory: PathBuf::from("/fictional/tmp"),
        uid: 42,
        platform: Platform::MacOs,
    }
}

#[test]
fn default_macos_layout_uses_external_standard_directories() {
    let env = Environment::resolve(std::iter::empty::<(OsString, OsString)>());
    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();

    assert_eq!(
        layout.application_support_dir,
        PathBuf::from("/fictional/home/alex/Library/Application Support/yams")
    );
    assert_eq!(
        layout.query_log,
        layout.application_support_dir.join("queries.jsonl")
    );
    assert_eq!(
        layout.cache_dir,
        PathBuf::from("/fictional/home/alex/Library/Caches/yams")
    );
    assert_eq!(layout.store_dir, layout.cache_dir.join("rust-v1"));
    assert_eq!(layout.indexes_dir, layout.store_dir.join("indexes"));
    assert_eq!(
        layout.vectors_path,
        layout.store_dir.join("vectors.sqlite3")
    );
    assert_eq!(layout.model_cache_dir, layout.store_dir.join("models"));
    assert_eq!(layout.model_lock_dir, layout.store_dir.join("locks"));
    assert_eq!(layout.runtime_dir, PathBuf::from("/fictional/tmp/yams-42"));
    assert_eq!(
        layout.service_socket,
        layout.runtime_dir.join("service.sock")
    );
    assert_eq!(layout.cwd, inputs().cwd);
}

#[test]
fn yams_home_collapses_mutable_state_under_an_explicit_flat_base() {
    let env = Environment::resolve([("YAMS_HOME", "/sandbox/state")]);
    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();
    assert_eq!(
        layout.application_support_dir,
        PathBuf::from("/sandbox/state")
    );
    assert_eq!(layout.cache_dir, PathBuf::from("/sandbox/state"));
    assert_eq!(layout.store_dir, PathBuf::from("/sandbox/state/rust-v1"));
    assert_eq!(
        layout.query_log,
        PathBuf::from("/sandbox/state/queries.jsonl")
    );
    assert_eq!(layout.runtime_dir, PathBuf::from("/sandbox/state"));
    assert_eq!(
        layout.service_socket,
        PathBuf::from("/sandbox/state/service.sock")
    );
}

#[test]
fn generic_compatibility_environment_variables_are_ignored() {
    let env = Environment::resolve([
        ("MEMORY_SEARCH_HOME", "/fictional/state"),
        ("MEMORY_SEARCH_DIRS", "/fictional/corpus"),
        ("MEMORY_SEARCH_ALLOW_NET", "1"),
        ("MEMORY_SEARCH_NO_SERVICE", "1"),
        ("MEMORY_SEARCH_SERVICE_SOCKET", "/fictional/service.sock"),
    ]);

    assert_eq!(env.home(), None);
    assert_eq!(env.dirs_override(), &DirsOverride::Absent);
    assert!(!env.allow_net());
    assert!(!env.no_service());
    assert_eq!(env.service_socket(), None);
}

#[test]
fn a_relative_explicit_home_is_resolved_against_injected_cwd() {
    let env = Environment::resolve([("YAMS_HOME", "state")]);
    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();
    assert_eq!(
        layout.store_dir,
        PathBuf::from("/fictional/work/copper-garden/state/rust-v1")
    );
}

#[test]
fn explicit_home_is_platform_independent_but_default_layout_is_not() {
    let mut other = inputs();
    other.platform = Platform::Unsupported("fictional-os");

    let explicit = Environment::resolve([("YAMS_HOME", "/sandbox/state")]);
    assert_eq!(
        RuntimeLayout::resolve(&explicit, &other).unwrap().store_dir,
        PathBuf::from("/sandbox/state/rust-v1")
    );

    let default = Environment::resolve(std::iter::empty::<(OsString, OsString)>());
    assert_eq!(
        RuntimeLayout::resolve(&default, &other)
            .unwrap_err()
            .to_string(),
        "Yams's default runtime layout is not defined for fictional-os"
    );
}

#[test]
fn explicit_home_does_not_require_an_injected_user_home() {
    let mut no_home = inputs();
    no_home.home = PathBuf::new();
    let explicit = Environment::resolve([("YAMS_HOME", "/sandbox/state")]);
    assert!(RuntimeLayout::resolve(&explicit, &no_home).is_ok());

    let default = Environment::resolve(std::iter::empty::<(OsString, OsString)>());
    assert_eq!(
        RuntimeLayout::resolve(&default, &no_home)
            .unwrap_err()
            .to_string(),
        "HOME is unset or empty"
    );
}

#[test]
fn empty_and_nonempty_yams_values_are_resolved() {
    let env = Environment::resolve([
        ("YAMS_HOME", ""),
        ("YAMS_DIRS", "/a:/b"),
        ("YAMS_ALLOW_NET", "1"),
        ("YAMS_NO_SERVICE", "1"),
        ("YAMS_SERVICE_SOCKET", "/tmp/custom.sock"),
    ]);
    assert_eq!(env.home(), None);
    assert_eq!(env.dirs(), Some(OsStr::new("/a:/b")));
    assert!(env.allow_net());
    assert!(env.no_service());
    assert_eq!(env.service_socket(), Some(OsStr::new("/tmp/custom.sock")));

    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();
    assert_eq!(
        layout.service_socket,
        PathBuf::from("/tmp")
            .canonicalize()
            .unwrap()
            .join("custom.sock")
    );
}

#[test]
fn dirs_override_preserves_absent_empty_and_nonempty_provenance() {
    let absent = Environment::resolve(std::iter::empty::<(OsString, OsString)>());
    assert_eq!(absent.dirs_override(), &DirsOverride::Absent);

    let primary_empty = Environment::resolve([("YAMS_DIRS", "")]);
    assert_eq!(
        primary_empty.dirs_override(),
        &DirsOverride::SetEmpty {
            variable: "YAMS_DIRS"
        }
    );
    assert_eq!(
        RuntimeLayout::resolve(&primary_empty, &inputs())
            .unwrap()
            .corpus_dirs,
        ResolvedDirsOverride::SetEmpty {
            variable: "YAMS_DIRS"
        }
    );

    let separators = Environment::resolve([("YAMS_DIRS", ":")]);
    assert_eq!(
        RuntimeLayout::resolve(&separators, &inputs())
            .unwrap()
            .corpus_dirs,
        ResolvedDirsOverride::NonEmpty(Vec::new())
    );
}

#[test]
fn service_routing_accessors_preserve_override_truthiness() {
    let default = Environment::resolve(std::iter::empty::<(OsString, OsString)>());
    assert_eq!(default.home(), None);
    assert_eq!(default.service_socket(), None);

    let explicit = Environment::resolve([
        ("YAMS_HOME", "/tmp/home"),
        ("YAMS_SERVICE_SOCKET", "/tmp/service.sock"),
    ]);
    assert_eq!(explicit.home(), Some(OsStr::new("/tmp/home")));
    assert_eq!(
        explicit.service_socket(),
        Some(OsStr::new("/tmp/service.sock"))
    );

    let empty_socket =
        Environment::resolve([("YAMS_HOME", "/tmp/home"), ("YAMS_SERVICE_SOCKET", "")]);
    assert_eq!(empty_socket.home(), Some(OsStr::new("/tmp/home")));
    assert_eq!(empty_socket.service_socket(), None);

    let exact_flags =
        Environment::resolve([("YAMS_ALLOW_NET", "true"), ("YAMS_NO_SERVICE", "true")]);
    assert!(!exact_flags.allow_net());
    assert!(!exact_flags.no_service());
}

#[test]
fn compatible_paths_expand_tilde_and_resolve_relative_dot_components() {
    let env = Environment::resolve([
        ("YAMS_HOME", "~/state/../yams-state"),
        ("YAMS_DIRS", "./shared:~/private::nested/../third"),
        ("YAMS_SERVICE_SOCKET", "./run/../yams.sock"),
    ]);
    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();

    assert_eq!(
        layout.application_support_dir,
        PathBuf::from("/fictional/home/alex/yams-state")
    );
    assert_eq!(
        layout.service_socket,
        PathBuf::from("/fictional/work/copper-garden/yams.sock")
    );
    assert_eq!(
        layout.corpus_dirs,
        ResolvedDirsOverride::NonEmpty(vec![
            PathBuf::from("/fictional/work/copper-garden/shared"),
            PathBuf::from("/fictional/home/alex/private"),
            PathBuf::from("/fictional/work/copper-garden/third"),
        ])
    );
}

#[cfg(unix)]
#[test]
fn existing_path_components_are_canonicalized() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let real = temporary.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = temporary.path().join("link");
    symlink(&real, &link).unwrap();
    let env = Environment::resolve([
        (
            OsString::from("YAMS_HOME"),
            link.join("missing/../state").into_os_string(),
        ),
        (
            OsString::from("YAMS_SERVICE_SOCKET"),
            link.join("run/service.sock").into_os_string(),
        ),
    ]);
    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();
    let real = real.canonicalize().unwrap();
    assert_eq!(layout.application_support_dir, real.join("state"));
    assert_eq!(layout.service_socket, real.join("run/service.sock"));
}

#[cfg(unix)]
#[test]
fn symlinks_are_resolved_before_parent_components() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("caller");
    let target = temporary.path().join("elsewhere/deep");
    std::fs::create_dir_all(&cwd).unwrap();
    std::fs::create_dir_all(&target).unwrap();
    symlink(&target, cwd.join("link")).unwrap();

    let mut runtime = inputs();
    runtime.cwd = cwd.clone();
    let env = Environment::resolve([("YAMS_HOME", "link/missing/..")]);
    let layout = RuntimeLayout::resolve(&env, &runtime).unwrap();
    assert_eq!(
        layout.application_support_dir,
        target.canonicalize().unwrap()
    );
}

#[cfg(unix)]
#[test]
fn compatible_weak_resolution_follows_dangling_links_and_allows_missing_suffixes() {
    use std::os::unix::fs::symlink;

    let temporary = tempfile::tempdir().unwrap();
    let cwd = temporary.path().join("caller");
    std::fs::create_dir(&cwd).unwrap();
    let target = temporary.path().join("elsewhere/missing");
    symlink(&target, cwd.join("dangling")).unwrap();
    let regular = cwd.join("regular");
    std::fs::write(&regular, b"fictional").unwrap();

    let mut runtime = inputs();
    runtime.cwd = cwd.clone();
    let dangling = Environment::resolve([("YAMS_HOME", "dangling/child")]);
    assert_eq!(
        RuntimeLayout::resolve(&dangling, &runtime)
            .unwrap()
            .application_support_dir,
        temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("elsewhere/missing/child")
    );

    let file_suffix = Environment::resolve([("YAMS_HOME", "regular/child")]);
    assert_eq!(
        RuntimeLayout::resolve(&file_suffix, &runtime)
            .unwrap()
            .application_support_dir,
        cwd.canonicalize().unwrap().join("regular/child")
    );
}

#[test]
fn default_layout_rejects_relative_home_and_tmpdir_inputs() {
    let environment = Environment::resolve(std::iter::empty::<(OsString, OsString)>());
    let mut relative_home = inputs();
    relative_home.home = PathBuf::from("relative-home");
    assert_eq!(
        RuntimeLayout::resolve(&environment, &relative_home)
            .unwrap_err()
            .to_string(),
        "HOME must be an absolute path"
    );

    let mut relative_tmp = inputs();
    relative_tmp.temporary_directory = PathBuf::from("relative-tmp");
    assert_eq!(
        RuntimeLayout::resolve(&environment, &relative_tmp)
            .unwrap_err()
            .to_string(),
        "TMPDIR must be an absolute path"
    );
}

#[test]
fn preparing_resolves_and_canonicalizes_the_selected_project() {
    let temporary = tempfile::tempdir().unwrap();
    let home = temporary.path().join("home");
    let project = home.join("work/copper-garden");
    std::fs::create_dir_all(&project).unwrap();
    let cwd = project.join("nested");
    std::fs::create_dir(&cwd).unwrap();
    let mut runtime = inputs();
    runtime.cwd = cwd;
    runtime.home = home;
    runtime.temporary_directory = temporary.path().join("tmp");
    std::fs::create_dir(&runtime.temporary_directory).unwrap();

    let (request, _, _) = prepare_direct(
        ["yams", "--project", "~/work/copper-garden/./", "query"],
        [("YAMS_HOME", temporary.path().join("state").as_os_str())],
        &runtime,
    )
    .unwrap();
    assert_eq!(request.project, Some(project.canonicalize().unwrap()));
}

#[test]
fn project_resolution_diagnostics_are_terminal_sanitized() {
    let temporary = tempfile::tempdir().unwrap();
    let mut runtime = inputs();
    runtime.cwd = temporary.path().to_owned();
    let hostile = "missing\nforged\u{1b}[2J";
    let completion = prepare_direct(
        ["yams", "--project", hostile, "query"],
        [("YAMS_HOME", temporary.path().as_os_str())],
        &runtime,
    )
    .unwrap_err();
    assert_eq!(completion.stdout, "");
    assert_eq!(completion.stderr.lines().count(), 1);
    assert!(completion.stderr.contains("missingforged[2J"));
    assert!(!completion.stderr.contains('\u{1b}'));
}

#[cfg(unix)]
#[test]
fn environment_equality_is_raw_os_string_equality() {
    use std::os::unix::ffi::OsStringExt;

    let first = OsString::from_vec(vec![b'/', 0x80]);
    let accepted = Environment::resolve([(OsString::from("YAMS_HOME"), first.clone())]);
    assert_eq!(accepted.home(), Some(first.as_os_str()));
}

#[cfg(unix)]
#[test]
fn non_utf8_explicit_home_remains_a_path_without_lossy_conversion() {
    use std::os::unix::ffi::OsStringExt;

    let home = OsString::from_vec(vec![b'/', b's', b't', b'a', b't', b'e', b'/', 0x80]);
    let env = Environment::resolve([(OsString::from("YAMS_HOME"), home.clone())]);
    let layout = RuntimeLayout::resolve(&env, &inputs()).unwrap();
    assert_eq!(layout.store_dir, PathBuf::from(home).join("rust-v1"));
}

#[test]
fn environment_resolution_does_not_read_the_real_process_environment() {
    // A deliberately tiny injected environment remains empty regardless of the
    // test runner's actual HOME and service variables.
    let env = Environment::resolve([("UNRELATED", "value")]);
    assert_eq!(env.home(), None);
    assert_eq!(env.dirs(), None);
    assert!(!env.allow_net());
    assert!(!env.no_service());
    assert_eq!(env.service_socket(), None);
}

#[test]
fn preparing_a_request_creates_no_runtime_state() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("not-created");
    let env_home = state.as_os_str().to_owned();
    let mut runtime = inputs();
    runtime.cwd = temporary.path().to_owned();
    let (request, _, layout) = prepare_direct(
        ["yams", "fictional query"],
        [(OsString::from("YAMS_HOME"), env_home)],
        &runtime,
    )
    .unwrap();
    assert_eq!(request.query(), Some("fictional query"));
    assert_eq!(
        layout.store_dir,
        temporary
            .path()
            .canonicalize()
            .unwrap()
            .join("not-created/rust-v1")
    );
    assert!(!state.exists());
}
