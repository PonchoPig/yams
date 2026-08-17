use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tempfile::{TempDir, tempdir};
use yams_core::{ExitCode, MAX_FILE_BYTES};
use yams_wiki::{ReindexOptions, reindex_wiki, write_json};

const ALPHA_PAGE: &str = "---\n\
slug: alpha\n\
title: Alpha\n\
type: gotcha\n\
status: historical\n\
owner: shared\n\
updated: 2026-07-28\n\
verified: 2026-07-28\n\
summary: a real trap\n\
---\n\n\
body.\n";

const INITIAL_INDEX: &str = "<!-- BEGIN GENERATED INDEX — edited by yams-wiki catalog, not by hand -->\n\n\
## Gotchas\n\n\
- [alpha](pages/alpha.md) — a real trap\n\n\
<!-- END GENERATED INDEX -->\n";

const CREATE_JSON: &str = r#"{
  "title": "A lunar console requires blue mode",
  "type": "gotcha",
  "owner": "codex",
  "fact": "A fictional lunar console rejects jobs unless blue mode is enabled.",
  "why": "Synthetic observations show jobs succeed only after enabling blue mode.",
  "how_to_apply": "Enable blue mode before submitting a fictional job.",
  "falsified_by": "A fictional job succeeding with blue mode disabled.",
  "summary": "fictional jobs require blue mode",
  "related": ["alpha", "not-written-yet", "not-written-yet"]
}"#;

const CREATED_PAGE: &str = "---\n\
slug: a-lunar-console-requires-blue-mode\n\
title: A lunar console requires blue mode\n\
type: gotcha\n\
status: current\n\
owner: codex\n\
updated: 2026-08-07\n\
verified: 2026-08-07\n\
summary: fictional jobs require blue mode\n\
---\n\n\
A fictional lunar console rejects jobs unless blue mode is enabled.\n\n\
**Why:** Synthetic observations show jobs succeed only after enabling blue mode.\n\n\
**How to apply:** Enable blue mode before submitting a fictional job.\n\n\
**Falsified by:** A fictional job succeeding with blue mode disabled.\n\n\
Related: [[alpha]], [[not-written-yet]]\n";

const CREATED_INDEX: &str = "<!-- BEGIN GENERATED INDEX — edited by yams-wiki catalog, not by hand -->\n\n\
## Gotchas\n\n\
- [a-lunar-console-requires-blue-mode](pages/a-lunar-console-requires-blue-mode.md) — fictional jobs require blue mode\n\
- [alpha](pages/alpha.md) — a real trap\n\n\
<!-- END GENERATED INDEX -->\n";

struct Fixture {
    _tmp: TempDir,
    memory: PathBuf,
}

impl Fixture {
    fn one_page() -> Self {
        let tmp = tempdir().unwrap();
        let memory = tmp.path().join(".agents/memory");
        fs::create_dir_all(memory.join("pages")).unwrap();
        fs::write(memory.join("pages/alpha.md"), ALPHA_PAGE).unwrap();
        fs::write(memory.join("INDEX.md"), INITIAL_INDEX).unwrap();
        Self { _tmp: tmp, memory }
    }

    fn page(&self, slug: &str) -> PathBuf {
        self.memory.join("pages").join(format!("{slug}.md"))
    }
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().permissions().mode() & 0o7777
}

fn create_value(title: &str) -> Value {
    json!({
        "title": title,
        "type": "gotcha",
        "owner": "codex",
        "fact": "A fact.",
        "why": "Because evidence says so.",
        "how_to_apply": "Apply the evidence.",
        "falsified_by": "Contrary evidence.",
        "summary": "a concise summary",
        "related": [],
    })
}

fn update_value(expected_sha256: Option<&str>) -> Value {
    let mut value = json!({
        "title": "Alpha",
        "type": "gotcha",
        "fact": "A revised body.",
        "why": "The evidence changed.",
        "how_to_apply": "Use the revised body.",
        "falsified_by": "The old evidence returning.",
        "summary": "a real trap",
        "related": [],
        "update": true,
        "target": "alpha",
    });
    if let Some(expected) = expected_sha256 {
        value["expected_sha256"] = json!(expected);
    }
    value
}

#[test]
fn create_writes_exact_page_and_index_and_reports_only_replaced_paths() {
    let fixture = Fixture::one_page();
    let page = fixture.page("a-lunar-console-requires-blue-mode");
    let index = fixture.memory.join("INDEX.md");
    let ordinary_mode = mode(&fixture.page("alpha"));
    let index_mode = mode(&index);

    let result = write_json(&fixture.memory, CREATE_JSON.as_bytes(), "2026-08-07");

    assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
    assert_eq!(
        result.body,
        json!({
            "ok": true,
            "slug": "a-lunar-console-requires-blue-mode",
            "paths": [page, index],
            "index_regenerated": true,
            "forward_refs": ["not-written-yet"],
        })
    );
    assert_eq!(fs::read(&page).unwrap(), CREATED_PAGE.as_bytes());
    assert_eq!(fs::read(&index).unwrap(), CREATED_INDEX.as_bytes());
    assert_eq!(mode(&page), ordinary_mode);
    assert_eq!(mode(&index), index_mode);
}

#[test]
fn cas_update_preserves_mode_owner_and_status_and_moves_changed_dates() {
    let fixture = Fixture::one_page();
    let page = fixture.page("alpha");
    let index = fixture.memory.join("INDEX.md");
    let index_mode = mode(&index);
    fs::set_permissions(&page, fs::Permissions::from_mode(0o600)).unwrap();
    let digest = format!("{:x}", Sha256::digest(ALPHA_PAGE.as_bytes()));
    let request = format!(
        r#"{{
  "title": "Alpha",
  "type": "gotcha",
  "fact": "A revised body.",
  "why": "The newer evidence supersedes the old body.",
  "how_to_apply": "Use the revised evidence.",
  "falsified_by": "The old body becoming true again.",
  "summary": "the revised trap",
  "related": [],
  "update": true,
  "target": "alpha",
  "expected_sha256": "{digest}"
}}"#
    );
    let expected_page = "---\n\
slug: alpha\n\
title: Alpha\n\
type: gotcha\n\
status: historical\n\
owner: shared\n\
updated: 2026-08-09\n\
verified: 2026-08-09\n\
summary: the revised trap\n\
---\n\n\
A revised body.\n\n\
**Why:** The newer evidence supersedes the old body.\n\n\
**How to apply:** Use the revised evidence.\n\n\
**Falsified by:** The old body becoming true again.\n";
    let expected_index = "<!-- BEGIN GENERATED INDEX — edited by yams-wiki catalog, not by hand -->\n\n\
## Gotchas\n\n\
- [alpha](pages/alpha.md) — the revised trap\n\n\
<!-- END GENERATED INDEX -->\n";

    let result = write_json(&fixture.memory, request.as_bytes(), "2026-08-09");

    assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
    assert_eq!(
        result.body,
        json!({
            "ok": true,
            "slug": "alpha",
            "paths": [page, index],
            "index_regenerated": true,
            "forward_refs": [],
        })
    );
    assert_eq!(fs::read(&page).unwrap(), expected_page.as_bytes());
    assert_eq!(fs::read(&index).unwrap(), expected_index.as_bytes());
    assert_eq!(mode(&page), 0o600);
    assert_eq!(mode(&index), index_mode);
}

#[test]
fn a_schema_valid_oversized_stored_page_can_be_updated() {
    let fixture = Fixture::one_page();
    let page = fixture.page("alpha");
    let oversized_fact = "x".repeat(MAX_FILE_BYTES as usize + 1);
    let oversized = ALPHA_PAGE.replace("body.", &oversized_fact);
    assert!(oversized.len() as u64 > MAX_FILE_BYTES);
    fs::write(&page, oversized.as_bytes()).unwrap();

    let result = write_json(
        &fixture.memory,
        &serde_json::to_vec(&update_value(None)).unwrap(),
        "2026-08-09",
    );

    assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
    assert_eq!(result.body["paths"], json!([page]));
    assert_eq!(result.body["index_regenerated"], false);
    assert!(
        fs::read_to_string(fixture.page("alpha"))
            .unwrap()
            .contains("A revised body.")
    );
}

#[test]
fn exact_input_cap_is_accepted_and_cap_plus_one_is_refused_before_parsing() {
    let fixture = Fixture::one_page();
    let mut value = create_value("Exact Cap");
    value["fact"] = json!("");
    let empty = serde_json::to_vec(&value).unwrap();
    value["fact"] = json!("x".repeat(MAX_FILE_BYTES as usize - empty.len()));
    let exact = serde_json::to_vec(&value).unwrap();
    assert_eq!(exact.len(), MAX_FILE_BYTES as usize);

    let accepted = write_json(&fixture.memory, &exact, "2026-08-07");
    assert_eq!(accepted.exit_code, ExitCode::Ok, "{}", accepted.body);

    let second = Fixture::one_page();
    let mut too_large = exact;
    too_large.push(b' ');
    let refused = write_json(&second.memory, &too_large, "2026-08-07");
    assert_eq!(refused.exit_code, ExitCode::Usage);
    assert!(
        refused.body["error"]
            .as_str()
            .unwrap()
            .contains("MAX_FILE_BYTES")
    );
    assert!(!second.page("exact-cap").exists());
}

#[test]
fn malformed_nonobject_duplicate_and_trailing_json_are_structured_preflight_refusals() {
    let root = tempdir().unwrap();
    let missing = root.path().join("missing-corpus");
    let cases: &[(&str, &[u8])] = &[
        ("invalid UTF-8", b"\xff"),
        ("malformed", b"{not json"),
        ("non-object", b"[]"),
        ("duplicate", br#"{"title":"first","title":"second"}"#),
        ("multiple", b"{} {}"),
        ("trailing value", b"{} true"),
    ];

    for (label, input) in cases {
        let result = write_json(&missing, input, "2026-08-07");
        assert_eq!(
            result.exit_code,
            ExitCode::Usage,
            "{label}: {}",
            result.body
        );
        assert_eq!(result.body["ok"], false, "{label}");
        assert_eq!(result.body["exit"], 2, "{label}");
        assert!(
            result.body.get("paths").is_none(),
            "{label}: {}",
            result.body
        );
    }
    assert!(
        !missing.exists(),
        "preflight must not acquire or create a lock"
    );
}

#[test]
fn routing_and_request_refusals_precede_missing_corpus_and_invalid_today_precedes_locking() {
    let root = tempdir().unwrap();
    let missing = root.path().join("missing-corpus");
    let mut routed = create_value("Would Be Create");
    routed["target"] = Value::Null;
    let result = write_json(
        &missing,
        &serde_json::to_vec(&routed).unwrap(),
        "2026-08-07",
    );
    assert_eq!(result.exit_code, ExitCode::Usage);
    assert!(
        result.body["error"]
            .as_str()
            .unwrap()
            .contains("owner is refused")
    );

    let invalid_today = write_json(
        &missing,
        &serde_json::to_vec(&create_value("Valid Request")).unwrap(),
        "August 7",
    );
    assert_eq!(invalid_today.exit_code, ExitCode::Operational);
    assert!(
        invalid_today.body["error"]
            .as_str()
            .unwrap()
            .contains("today")
    );
    assert!(!missing.exists());
}

#[test]
fn non_utf8_corpus_is_operational_after_lexical_preflight_and_before_mutation() {
    let root = tempdir().unwrap();
    let corpus = root
        .path()
        .join(OsString::from_vec(b"memory-\xff".to_vec()));

    let lexical = write_json(&corpus, b"\xff", "2026-08-07");
    assert_eq!(lexical.exit_code, ExitCode::Usage, "{}", lexical.body);
    assert!(
        lexical.body["error"]
            .as_str()
            .unwrap()
            .contains("stdin is not valid UTF-8")
    );

    let path_result = write_json(&corpus, CREATE_JSON.as_bytes(), "2026-08-07");
    assert_eq!(path_result.exit_code, ExitCode::Operational);
    assert_eq!(
        path_result.body,
        json!({
            "ok": false,
            "exit": 4,
            "error": "corpus path is not valid UTF-8",
            "hint": "use a UTF-8 corpus path",
        })
    );
    assert!(!corpus.exists());
    assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn cas_hashes_exact_bom_and_crlf_bytes_and_omission_skips_only_the_compare() {
    let fixture = Fixture::one_page();
    let raw = format!("\u{feff}{}", ALPHA_PAGE.replace('\n', "\r\n"));
    fs::write(fixture.page("alpha"), raw.as_bytes()).unwrap();
    let digest = format!("{:x}", Sha256::digest(raw.as_bytes()));

    let matched = write_json(
        &fixture.memory,
        &serde_json::to_vec(&update_value(Some(&digest))).unwrap(),
        "2026-08-09",
    );
    assert_eq!(matched.exit_code, ExitCode::Ok, "{}", matched.body);

    let mismatch_fixture = Fixture::one_page();
    let before = fs::read(mismatch_fixture.page("alpha")).unwrap();
    let mismatch = write_json(
        &mismatch_fixture.memory,
        &serde_json::to_vec(&update_value(Some(&"0".repeat(64)))).unwrap(),
        "2026-08-09",
    );
    assert_eq!(mismatch.exit_code, ExitCode::Usage);
    assert!(mismatch.body["error"].as_str().unwrap().contains("changed"));
    assert_eq!(fs::read(mismatch_fixture.page("alpha")).unwrap(), before);

    let unconditional_fixture = Fixture::one_page();
    let unconditional = write_json(
        &unconditional_fixture.memory,
        &serde_json::to_vec(&update_value(None)).unwrap(),
        "2026-08-09",
    );
    assert_eq!(
        unconditional.exit_code,
        ExitCode::Ok,
        "{}",
        unconditional.body
    );
}

#[test]
fn every_create_collision_shape_is_preserved_and_never_overwritten() {
    let regular = Fixture::one_page();
    let input = serde_json::to_vec(&create_value("Alpha")).unwrap();
    let before = fs::read(regular.page("alpha")).unwrap();
    let regular_result = write_json(&regular.memory, &input, "2026-08-07");
    assert_ne!(regular_result.exit_code, ExitCode::Ok);
    assert_eq!(fs::read(regular.page("alpha")).unwrap(), before);

    let directory = Fixture::one_page();
    let directory_target = directory.page("directory-target");
    fs::create_dir(&directory_target).unwrap();
    let directory_result = write_json(
        &directory.memory,
        &serde_json::to_vec(&create_value("Directory Target")).unwrap(),
        "2026-08-07",
    );
    assert_ne!(directory_result.exit_code, ExitCode::Ok);
    assert!(fs::symlink_metadata(&directory_target).unwrap().is_dir());

    let dangling = Fixture::one_page();
    let dangling_target = dangling.page("dangling-target");
    symlink("missing-destination", &dangling_target).unwrap();
    let dangling_result = write_json(
        &dangling.memory,
        &serde_json::to_vec(&create_value("Dangling Target")).unwrap(),
        "2026-08-07",
    );
    assert_ne!(dangling_result.exit_code, ExitCode::Ok);
    assert!(
        fs::symlink_metadata(&dangling_target)
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let linked = Fixture::one_page();
    let linked_target = linked.page("linked-target");
    fs::hard_link(linked.page("alpha"), &linked_target).unwrap();
    let inode = fs::metadata(&linked_target).unwrap().ino();
    let linked_result = write_json(
        &linked.memory,
        &serde_json::to_vec(&create_value("Linked Target")).unwrap(),
        "2026-08-07",
    );
    assert_ne!(linked_result.exit_code, ExitCode::Ok);
    assert_eq!(fs::metadata(&linked_target).unwrap().ino(), inode);

    let fifo = Fixture::one_page();
    let fifo_target = fifo.page("fifo-target");
    let status = Command::new("mkfifo").arg(&fifo_target).status().unwrap();
    assert!(status.success());
    let fifo_result = write_json(
        &fifo.memory,
        &serde_json::to_vec(&create_value("Fifo Target")).unwrap(),
        "2026-08-07",
    );
    assert_ne!(fifo_result.exit_code, ExitCode::Ok);
    assert!(
        fs::symlink_metadata(&fifo_target)
            .unwrap()
            .file_type()
            .is_fifo()
    );
}

#[test]
fn unchanged_index_response_omits_index_and_forward_refs_keep_first_seen_order() {
    let fixture = Fixture::one_page();
    let create = write_json(&fixture.memory, CREATE_JSON.as_bytes(), "2026-08-07");
    assert_eq!(create.exit_code, ExitCode::Ok);
    let page = fixture.page("a-lunar-console-requires-blue-mode");
    let digest = format!("{:x}", Sha256::digest(fs::read(&page).unwrap()));
    let mut update: Value = serde_json::from_str(CREATE_JSON).unwrap();
    update.as_object_mut().unwrap().remove("owner");
    update["update"] = json!(true);
    update["target"] = json!("a-lunar-console-requires-blue-mode");
    update["expected_sha256"] = json!(digest);
    update["related"] = json!(["missing-b", "alpha", "missing-a", "missing-b", "missing-a"]);

    let result = write_json(
        &fixture.memory,
        &serde_json::to_vec(&update).unwrap(),
        "2026-08-09",
    );

    assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
    assert_eq!(result.body["index_regenerated"], false);
    assert_eq!(result.body["paths"], json!([page]));
    assert_eq!(
        result.body["forward_refs"],
        json!(["missing-b", "missing-a"])
    );
    let stored = fs::read_to_string(fixture.page("a-lunar-console-requires-blue-mode")).unwrap();
    assert!(stored.contains("updated: 2026-08-09"));
    assert!(stored.contains("verified: 2026-08-09"));
}

#[test]
fn noncanonical_index_refuses_before_page_mutation_with_no_paths() {
    let fixture = Fixture::one_page();
    fs::write(
        fixture.memory.join("INDEX.md"),
        INITIAL_INDEX.replace("a real trap", "hand edited"),
    )
    .unwrap();
    let result = write_json(&fixture.memory, CREATE_JSON.as_bytes(), "2026-08-07");
    assert_eq!(result.exit_code, ExitCode::Usage);
    assert!(result.body["error"].as_str().unwrap().contains("canonical"));
    assert!(result.body.get("paths").is_none());
    assert!(!fixture.page("a-lunar-console-requires-blue-mode").exists());
}

#[test]
fn concurrent_public_writers_leave_same_and_different_slug_wikis_canonical() {
    let same = Arc::new(Fixture::one_page());
    let barrier = Arc::new(Barrier::new(3));
    let mut writers = Vec::new();
    for _ in 0..2 {
        let fixture = Arc::clone(&same);
        let barrier = Arc::clone(&barrier);
        writers.push(thread::spawn(move || {
            barrier.wait();
            write_json(&fixture.memory, CREATE_JSON.as_bytes(), "2026-08-07")
        }));
    }
    barrier.wait();
    let mut exits = writers
        .into_iter()
        .map(|writer| writer.join().unwrap().exit_code)
        .collect::<Vec<_>>();
    exits.sort_by_key(|exit| i32::from(*exit));
    assert_eq!(exits, vec![ExitCode::Ok, ExitCode::Usage]);

    let different = Arc::new(Fixture::one_page());
    let barrier = Arc::new(Barrier::new(3));
    let inputs = [
        serde_json::to_vec(&create_value("Writer One")).unwrap(),
        serde_json::to_vec(&create_value("Writer Two")).unwrap(),
    ];
    let writers = inputs
        .into_iter()
        .map(|input| {
            let fixture = Arc::clone(&different);
            let barrier = Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                write_json(&fixture.memory, &input, "2026-08-07")
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    for writer in writers {
        let result = writer.join().unwrap();
        assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
    }
    let checked = reindex_wiki(
        &different.memory,
        &ReindexOptions {
            check_only: true,
            ..ReindexOptions::default()
        },
    )
    .unwrap();
    assert!(!checked.changed);
}

#[test]
fn unsafe_lock_and_hardlinked_update_target_are_operational_without_paths() {
    let unsafe_lock = Fixture::one_page();
    symlink("INDEX.md", unsafe_lock.memory.join(".write.lock")).unwrap();
    let result = write_json(&unsafe_lock.memory, CREATE_JSON.as_bytes(), "2026-08-07");
    assert_eq!(result.exit_code, ExitCode::Operational);
    assert!(result.body.get("paths").is_none());
    assert!(
        fs::symlink_metadata(unsafe_lock.memory.join(".write.lock"))
            .unwrap()
            .file_type()
            .is_symlink()
    );

    let hardlinked = Fixture::one_page();
    let page = hardlinked.page("alpha");
    let alias = hardlinked.memory.join("alpha-alias");
    fs::hard_link(&page, &alias).unwrap();
    let before = fs::read(&page).unwrap();
    let result = write_json(
        &hardlinked.memory,
        &serde_json::to_vec(&update_value(None)).unwrap(),
        "2026-08-09",
    );
    assert_eq!(result.exit_code, ExitCode::Operational, "{}", result.body);
    assert!(result.body.get("paths").is_none());
    assert_eq!(fs::read(&page).unwrap(), before);
    assert_eq!(fs::read(alias).unwrap(), before);
}

#[test]
fn ordinary_create_permissions_follow_umask_in_an_isolated_process() {
    let status = Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg("ordinary_create_permissions_child")
        .arg("--test-threads=1")
        .env("MEMORY_WIKI_UMASK_CHILD", "1")
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
fn ordinary_create_permissions_child() {
    if std::env::var_os("MEMORY_WIKI_UMASK_CHILD").is_none() {
        return;
    }
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o027));
    let fixture = Fixture::one_page();
    let result = write_json(
        &fixture.memory,
        &serde_json::to_vec(&create_value("Umask Probe")).unwrap(),
        "2026-08-07",
    );
    assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
    assert_eq!(mode(&fixture.page("umask-probe")), 0o640);
}
