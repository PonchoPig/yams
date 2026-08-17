#[allow(unreachable_pub)]
#[path = "../src/query_log.rs"]
mod query_log;

use std::fs::{self, OpenOptions};
use std::io::Read;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::Path;
use std::process::Command;
use std::sync::{Arc, Barrier};
use std::thread;

use query_log::{
    QueryLogEligibility, QueryLogOutcome, QueryLogRecord, QueryLogSkip, TestWriteBehavior,
    append_query_log, append_query_log_with_test_policy, query_hash,
    unicode_15_is_case_ignorable_for_test, unicode_15_is_cased_for_test,
    unicode_15_lowercase_for_test,
};
use sha2::{Digest, Sha256};

const STAMP: &str = "2026-08-09T23:59:58.123Z";

fn private_log(path: &Path) {
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).unwrap();
}

fn record<'a>(project: &'a Path, query: &'a str) -> QueryLogRecord<'a> {
    QueryLogRecord {
        timestamp: STAMP,
        project,
        query,
        k: 5,
        rc: 3,
        hits: 0,
        gate: true,
        explain: false,
        min_score: None,
        max_gap: None,
        all: false,
    }
}

#[test]
fn query_hash_matches_python_lowercase_and_strip_semantics() {
    assert_eq!(query_hash("  Alpha  "), "8ed3f6ad685b959e");
    assert_eq!(query_hash("\u{001c}alpha\u{001f}"), "8ed3f6ad685b959e");
    assert_eq!(query_hash("ΟΣ"), "37966cce5c2e6c42");
    assert_eq!(query_hash("ΟΣΑ"), "aab41f24fca3e3cf");
    assert_eq!(query_hash("İ"), "fbbdb347060082cf");
    assert_eq!(query_hash("ẞ"), "cd3a7e92a9114307");
}

#[test]
fn final_sigma_uses_the_complete_project_python_context_properties() {
    assert_eq!(query_hash("AΣ\u{0374}B"), "c2beea87ed14127a");
    assert_eq!(query_hash("A:Σ"), "13c25750cc32c058");
    assert_eq!(query_hash("A-Σ"), "93d9efa31fb3c32f");
    assert_eq!(query_hash("A\u{0374}Σ"), "9ee5d8aa2f81f70e");
    assert_eq!(query_hash("1\u{0374}Σ"), "669faeeca7abf731");
}

#[test]
fn lowercase_mapping_is_pinned_to_project_python_unicode_15_not_rust_unicode_17() {
    assert_eq!(query_hash("\u{a7ce}"), "5897d4855f31f3e7");
}

#[test]
fn lowercase_mapping_stays_on_the_project_python_unicode_15_oracle() {
    for (codepoint, expected) in [
        ('\u{1c89}', "bf429adf97e7be21"),
        ('\u{a7cb}', "5a98ee6e1970a69a"),
        ('\u{a7cc}', "ab9131b9000ec2fa"),
        ('\u{a7da}', "f8faa8b5135ce961"),
        ('\u{a7dc}', "be78a3832714b095"),
    ] {
        assert_eq!(query_hash(&codepoint.to_string()), expected);
    }

    let garay_capitals = (0x10d50..=0x10d65)
        .map(|codepoint| char::from_u32(codepoint).unwrap())
        .collect::<String>();
    assert_eq!(query_hash(&garay_capitals), "a73e197c9d8dd391");
}

#[test]
fn complete_unicode_15_lowercase_table_has_the_project_python_fingerprint() {
    // Frozen from Python 3.12.13 with unicodedata 15.0.0. Each scalar is
    // framed by code point and UTF-8 byte length before its lowercase bytes.
    let mut fingerprint = Sha256::new();
    for codepoint in 0..=0x10ffff {
        let Some(character) = char::from_u32(codepoint) else {
            continue;
        };
        let lowered = unicode_15_lowercase_for_test(&character.to_string());
        fingerprint.update(codepoint.to_be_bytes());
        fingerprint.update((lowered.len() as u32).to_be_bytes());
        fingerprint.update(lowered.as_bytes());
    }

    assert_eq!(
        hex(fingerprint.finalize().as_slice()),
        "2fadea61c774c801475c765b4d4858eaff3870ef996d5bfbf32303c376d26abf"
    );
}

#[test]
fn complete_unicode_15_context_properties_have_frozen_fingerprints() {
    // Frozen from the matching Unicode 15 DerivedCoreProperties data.
    let mut cased = Sha256::new();
    let mut case_ignorable = Sha256::new();
    for codepoint in 0..=0x10ffff {
        let Some(character) = char::from_u32(codepoint) else {
            continue;
        };
        if unicode_15_is_cased_for_test(character) {
            cased.update(codepoint.to_be_bytes());
        }
        if unicode_15_is_case_ignorable_for_test(character) {
            case_ignorable.update(codepoint.to_be_bytes());
        }
    }

    assert_eq!(
        hex(cased.finalize().as_slice()),
        "fe1d1a8c52c5b0bf83c56fe3b04a4eea09b39620dd2c95b1594d165d28987bb9"
    );
    assert_eq!(
        hex(case_ignorable.finalize().as_slice()),
        "e5f56f66ac93cbe5f9b7ca4ce4a697f49d25d71d1cfa18b75c9a1b47718ee710"
    );
}

#[test]
fn final_sigma_context_tables_match_project_python_for_every_unicode_scalar() {
    // These two Python 3.12 fingerprints distinguish Cased from
    // Case_Ignorable: a terminal scalar and the same scalar before a cased B.
    let mut terminal = Sha256::new();
    let mut before_cased = Sha256::new();
    for codepoint in 0..=0x10ffff {
        let Some(character) = char::from_u32(codepoint) else {
            continue;
        };

        let mut input = String::from("AΣ");
        input.push(character);
        let lowered = unicode_15_lowercase_for_test(&input);
        update_fingerprint(&mut terminal, codepoint, &lowered);

        input.push('B');
        let lowered = unicode_15_lowercase_for_test(&input);
        update_fingerprint(&mut before_cased, codepoint, &lowered);
    }

    assert_eq!(
        hex(terminal.finalize().as_slice()),
        "ec5544aaa187ebc474fca897e011ab031d593828ed4417925965e9d6ba41bec8"
    );
    assert_eq!(
        hex(before_cased.finalize().as_slice()),
        "d9df71a1299ea3c9a9a76d46e25f51c83fde1eb298bf54e048c5a973dd05f7df"
    );
}

fn update_fingerprint(fingerprint: &mut Sha256, codepoint: u32, value: &str) {
    fingerprint.update(codepoint.to_be_bytes());
    fingerprint.update((value.len() as u32).to_be_bytes());
    fingerprint.update(value.as_bytes());
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[test]
fn missing_log_is_disabled_and_never_created() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    let project = Path::new("/fictional/acme");

    assert_eq!(
        append_query_log(
            &log,
            QueryLogEligibility::SearchAttempted,
            &record(project, "alpha")
        ),
        QueryLogOutcome::Disabled
    );
    assert!(!log.exists());
}

#[test]
fn one_compact_json_line_has_the_exact_composition_fields() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let project = Path::new("/fictional/acme\"project");
    let mut input = record(project, "kubernetes rollback");
    input.k = 3;
    input.rc = 0;
    input.hits = 2;
    input.gate = false;
    input.explain = true;
    input.min_score = Some(0.5);
    input.max_gap = Some(0.2);
    input.all = true;

    assert_eq!(
        append_query_log(&log, QueryLogEligibility::SearchAttempted, &input),
        QueryLogOutcome::Appended
    );
    assert_eq!(
        fs::read_to_string(&log).unwrap(),
        concat!(
            "{\"ts\":\"2026-08-09T23:59:58.123Z\",",
            "\"project\":\"/fictional/acme\\\"project\",",
            "\"q\":\"eecc861af14e6f61\",\"k\":3,\"rc\":0,\"hits\":2,",
            "\"gate\":false,\"explain\":true,\"min_score\":0.5,",
            "\"max_gap\":0.2,\"all\":true}\n"
        )
    );
}

#[test]
fn append_uses_the_injected_utc_millisecond_timestamp_verbatim() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let project = Path::new("/fictional/acme");
    let mut input = record(project, "alpha");
    input.timestamp = "2026-08-10T12:34:56.789Z";

    assert_eq!(
        append_query_log(&log, QueryLogEligibility::SearchAttempted, &input),
        QueryLogOutcome::Appended
    );
    let logged = fs::read_to_string(&log).unwrap();
    assert!(logged.contains("\"ts\":\"2026-08-10T12:34:56.789Z\""));
}

#[test]
fn query_text_never_reaches_the_file() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let secret = "fictional-pluto-rollback";

    assert_eq!(
        append_query_log(
            &log,
            QueryLogEligibility::SearchAttempted,
            &record(Path::new("/fictional/acme"), secret)
        ),
        QueryLogOutcome::Appended
    );
    assert!(!fs::read_to_string(log).unwrap().contains(secret));
}

#[test]
fn eligibility_makes_non_search_outcomes_unloggable() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let project = Path::new("/fictional/acme");

    for eligibility in [
        QueryLogEligibility::Management,
        QueryLogEligibility::Write,
        QueryLogEligibility::BlankOrInvalidQuery,
        QueryLogEligibility::PreSearchFailure,
        QueryLogEligibility::OperationalFailure,
    ] {
        assert_eq!(
            append_query_log(&log, eligibility, &record(project, "alpha")),
            QueryLogOutcome::Skipped(QueryLogSkip::Ineligible(eligibility))
        );
    }
    assert_eq!(fs::metadata(log).unwrap().len(), 0);
}

#[test]
fn blank_query_and_usage_or_operational_exit_are_rejected_defensively() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let project = Path::new("/fictional/acme");

    for query in ["", " \t\n", "\u{001c}\u{001f}"] {
        assert_eq!(
            append_query_log(
                &log,
                QueryLogEligibility::SearchAttempted,
                &record(project, query)
            ),
            QueryLogOutcome::Skipped(QueryLogSkip::InvalidRecord)
        );
    }
    for rc in [2, 4, -1, 5] {
        let mut input = record(project, "alpha");
        input.rc = rc;
        assert_eq!(
            append_query_log(&log, QueryLogEligibility::SearchAttempted, &input),
            QueryLogOutcome::Skipped(QueryLogSkip::InvalidRecord)
        );
    }
    assert_eq!(fs::metadata(log).unwrap().len(), 0);
}

#[test]
fn record_inputs_are_bounded_and_nonfinite_numbers_are_skipped() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let project = Path::new("/fictional/acme");
    let oversized_query = "q".repeat(query_log::MAX_QUERY_BYTES + 1);
    let oversized_project = format!("/fictional/{}", "p".repeat(query_log::MAX_PROJECT_BYTES));

    for input in [
        record(project, &oversized_query),
        record(Path::new(&oversized_project), "alpha"),
    ] {
        assert_eq!(
            append_query_log(&log, QueryLogEligibility::SearchAttempted, &input),
            QueryLogOutcome::Skipped(QueryLogSkip::Oversized)
        );
    }
    let mut input = record(project, "alpha");
    input.min_score = Some(f64::NAN);
    assert_eq!(
        append_query_log(&log, QueryLogEligibility::SearchAttempted, &input),
        QueryLogOutcome::Skipped(QueryLogSkip::InvalidRecord)
    );
    let mut input = record(project, "alpha");
    input.max_gap = Some(f64::INFINITY);
    assert_eq!(
        append_query_log(&log, QueryLogEligibility::SearchAttempted, &input),
        QueryLogOutcome::Skipped(QueryLogSkip::InvalidRecord)
    );
    assert_eq!(fs::metadata(log).unwrap().len(), 0);
}

#[test]
fn hostile_file_objects_are_rejected_without_touching_their_targets() {
    let temp = tempfile::tempdir().unwrap();
    let project = Path::new("/fictional/acme");
    let target = temp.path().join("target");
    private_log(&target);
    fs::write(&target, b"preserve me").unwrap();

    let symlink_path = temp.path().join("symlink.jsonl");
    symlink(&target, &symlink_path).unwrap();
    assert_eq!(
        append_query_log(
            &symlink_path,
            QueryLogEligibility::SearchAttempted,
            &record(project, "alpha")
        ),
        QueryLogOutcome::Rejected
    );

    let hardlink_path = temp.path().join("hardlink.jsonl");
    fs::hard_link(&target, &hardlink_path).unwrap();
    assert_eq!(
        append_query_log(
            &hardlink_path,
            QueryLogEligibility::SearchAttempted,
            &record(project, "alpha")
        ),
        QueryLogOutcome::Rejected
    );
    assert_eq!(fs::read(&target).unwrap(), b"preserve me");
}

#[test]
fn fifo_and_public_file_are_rejected_without_blocking_or_writing() {
    let temp = tempfile::tempdir().unwrap();
    let project = Path::new("/fictional/acme");
    let fifo = temp.path().join("queries.fifo");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );
    assert_eq!(
        append_query_log(
            &fifo,
            QueryLogEligibility::SearchAttempted,
            &record(project, "alpha")
        ),
        QueryLogOutcome::Rejected
    );

    let public = temp.path().join("public.jsonl");
    private_log(&public);
    fs::set_permissions(&public, fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        append_query_log(
            &public,
            QueryLogEligibility::SearchAttempted,
            &record(project, "alpha")
        ),
        QueryLogOutcome::Rejected
    );
    assert_eq!(fs::metadata(public).unwrap().len(), 0);
}

#[test]
fn foreign_owner_policy_is_testable_without_privileged_chown() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let actual = rustix::process::geteuid().as_raw();

    assert_eq!(
        append_query_log_with_test_policy(
            &log,
            QueryLogEligibility::SearchAttempted,
            &record(Path::new("/fictional/acme"), "alpha"),
            actual.wrapping_add(1),
            TestWriteBehavior::Normal,
        ),
        QueryLogOutcome::Rejected
    );
    assert_eq!(fs::metadata(log).unwrap().len(), 0);
}

#[test]
fn late_name_rebinding_is_rejected_before_the_append() {
    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let owner = rustix::process::geteuid().as_raw();

    assert_eq!(
        append_query_log_with_test_policy(
            &log,
            QueryLogEligibility::SearchAttempted,
            &record(Path::new("/fictional/acme"), "alpha"),
            owner,
            TestWriteBehavior::RebindBeforeFinalValidation,
        ),
        QueryLogOutcome::Rejected
    );
    assert_eq!(fs::metadata(&log).unwrap().len(), 0);
    assert_eq!(
        fs::metadata(temp.path().join("queries.jsonl.displaced"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn one_injected_failed_or_short_write_is_swallowed_without_retry() {
    for behavior in [TestWriteBehavior::Fail, TestWriteBehavior::Short] {
        let temp = tempfile::tempdir().unwrap();
        let log = temp.path().join("queries.jsonl");
        private_log(&log);
        let owner = rustix::process::geteuid().as_raw();

        assert_eq!(
            append_query_log_with_test_policy(
                &log,
                QueryLogEligibility::SearchAttempted,
                &record(Path::new("/fictional/acme"), "alpha"),
                owner,
                behavior,
            ),
            QueryLogOutcome::Failed
        );
        let len = fs::metadata(log).unwrap().len();
        match behavior {
            TestWriteBehavior::Fail => assert_eq!(len, 0),
            TestWriteBehavior::Short => assert!(len > 0),
            TestWriteBehavior::Normal | TestWriteBehavior::RebindBeforeFinalValidation => {
                unreachable!()
            }
        }
    }
}

#[test]
fn concurrent_single_write_appends_never_interleave_records() {
    const THREADS: usize = 24;
    const RECORDS: usize = 20;

    let temp = tempfile::tempdir().unwrap();
    let log = temp.path().join("queries.jsonl");
    private_log(&log);
    let barrier = Arc::new(Barrier::new(THREADS));
    let mut workers = Vec::new();

    for worker in 0..THREADS {
        let log = log.clone();
        let barrier = Arc::clone(&barrier);
        workers.push(thread::spawn(move || {
            barrier.wait();
            let project = Path::new("/fictional/acme");
            for item in 0..RECORDS {
                let query = format!("worker-{worker}-record-{item}");
                assert_eq!(
                    append_query_log(
                        &log,
                        QueryLogEligibility::SearchAttempted,
                        &record(project, &query)
                    ),
                    QueryLogOutcome::Appended
                );
            }
        }));
    }
    for worker in workers {
        worker.join().unwrap();
    }

    let mut bytes = Vec::new();
    OpenOptions::new()
        .read(true)
        .open(log)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    let text = String::from_utf8(bytes).unwrap();
    let lines = text.lines().collect::<Vec<_>>();
    assert_eq!(lines.len(), THREADS * RECORDS);
    assert!(text.ends_with('\n'));
    for line in lines {
        assert!(line.starts_with("{\"ts\":"));
        assert!(line.ends_with("\"all\":false}"));
        assert_eq!(line.matches("\"ts\":").count(), 1);
        assert_eq!(line.matches("\"q\":").count(), 1);
    }
}
