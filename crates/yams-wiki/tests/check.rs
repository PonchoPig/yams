use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::symlink;

use tempfile::{TempDir, tempdir};
use yams_wiki::{
    BEGIN_MARKER, CapturedPageOutcome, END_MARKER, ReindexOptions, capture_wiki, check_wiki,
    compat_wiki, reindex_wiki, validate_wiki,
};

#[allow(clippy::too_many_arguments)]
fn page(
    slug: &str,
    title: &str,
    page_type: &str,
    status: &str,
    owner: &str,
    updated: &str,
    verified: &str,
    summary: &str,
    body: &str,
) -> String {
    format!(
        "---\nslug: {slug}\ntitle: {title}\ntype: {page_type}\nstatus: {status}\nowner: {owner}\nupdated: {updated}\nverified: {verified}\nsummary: {summary}\n---\n\n{body}\n"
    )
}

fn good(slug: &str, summary: &str, body: &str) -> String {
    page(
        slug,
        "A title",
        "gotcha",
        "current",
        "shared",
        "2026-08-08",
        "2026-08-08",
        summary,
        body,
    )
}

fn fixture(pages: &[(&str, String)]) -> TempDir {
    let tmp = tempdir().unwrap();
    fs::create_dir(tmp.path().join("pages")).unwrap();
    fs::write(
        tmp.path().join("INDEX.md"),
        format!("Preamble.\n{BEGIN_MARKER}\nold\n{END_MARKER}\nFooter.\n"),
    )
    .unwrap();
    for (name, source) in pages {
        fs::write(tmp.path().join("pages").join(name), source).unwrap();
    }
    reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
    tmp
}

fn has(items: &[String], needle: &str) -> bool {
    items.iter().any(|item| item.contains(needle))
}

#[test]
fn one_locked_snapshot_validates_without_any_later_filesystem_access() {
    let tmp = fixture(&[("alpha.md", good("alpha", "A summary", "fact"))]);
    let snapshot = capture_wiki(tmp.path()).unwrap();
    fs::remove_dir_all(tmp.path()).unwrap();
    let report = validate_wiki(&snapshot);
    assert!(report.failures.is_empty(), "{report:?}");
    assert!(report.notes.is_empty(), "{report:?}");
}

#[test]
fn check_wrapper_reports_a_clean_canonical_wiki() {
    let tmp = fixture(&[("alpha.md", good("alpha", "A summary", "fact"))]);
    let report = check_wiki(tmp.path()).unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert!(report.notes.is_empty(), "{report:?}");
    assert_eq!(report.page_count, 1);
    assert!(report.isolated);
}

#[test]
fn checker_preflight_reports_missing_inputs_without_creating_a_lock() {
    let parent = tempdir().unwrap();
    let missing = parent.path().join("missing-corpus");
    let report = check_wiki(&missing).unwrap();
    assert_eq!(
        report.failures,
        [format!(
            "{} does not exist",
            missing.join("pages").display()
        )]
    );
    assert!(!missing.join(".write.lock").exists());

    let corpus = parent.path().join("corpus");
    fs::create_dir(&corpus).unwrap();
    let report = check_wiki(&corpus).unwrap();
    assert_eq!(
        report.failures,
        [format!("{} does not exist", corpus.join("pages").display())]
    );
    assert!(!corpus.join(".write.lock").exists());

    fs::create_dir(corpus.join("pages")).unwrap();
    let report = check_wiki(&corpus).unwrap();
    assert_eq!(
        report.failures,
        [format!(
            "{} does not exist",
            corpus.join("INDEX.md").display()
        )]
    );
    assert!(!corpus.join(".write.lock").exists());
}

#[test]
fn schema_filename_and_date_failures_are_accumulated() {
    let tmp = fixture(&[("alpha.md", good("alpha", "Initially valid", "fact"))]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    snapshot.pages[0].outcome = CapturedPageOutcome::Present(
        page(
            "wrong",
            "",
            "mystery",
            "stale",
            "nobody",
            "08/08/2026",
            "2026-01-01",
            "",
            "fact",
        )
        .into_bytes(),
    );
    let report = validate_wiki(&snapshot);
    for expected in [
        "declares slug",
        "missing a title",
        "missing summary",
        "has status",
        "has type",
        "has owner",
        "has updated",
    ] {
        assert!(has(&report.failures, expected), "{expected}: {report:?}");
    }
}

#[test]
fn empty_required_enum_and_date_fields_are_reported_as_missing() {
    let tmp = fixture(&[("alpha.md", good("alpha", "Initially valid", "fact"))]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    snapshot.pages[0].outcome = CapturedPageOutcome::Present(
        page("alpha", "Title", "", "", "", "", "", "Summary", "fact").into_bytes(),
    );
    let report = validate_wiki(&snapshot);
    for field in ["type", "status", "owner", "updated", "verified"] {
        assert!(
            report
                .failures
                .contains(&format!("pages/alpha.md is missing {field}")),
            "{field}: {report:?}"
        );
        assert!(
            !has(&report.failures, &format!("has {field}:")),
            "{field}: {report:?}"
        );
    }
}

#[test]
fn whole_index_membership_and_generated_region_are_independently_checked() {
    let tmp = fixture(&[
        ("alpha.md", good("alpha", "First", "[[beta]]")),
        ("beta.md", good("beta", "Second", "fact")),
    ]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    let index = String::from_utf8(snapshot.index.clone()).unwrap();
    snapshot.index = (index
        .replace(
            "- [alpha](pages/alpha.md) — First",
            "- [wrong](pages/alpha.md) — Swapped",
        )
        .replace("- [beta](pages/beta.md) — Second\n", "")
        + "\n- [beta](pages/beta.md) — curated footer mention\n")
        .into_bytes();
    let report = validate_wiki(&snapshot);
    assert!(has(&report.failures, "labels alpha as"), "{report:?}");
    assert!(has(&report.failures, "summary for alpha"), "{report:?}");
    assert!(has(&report.failures, "entry set differs"), "{report:?}");
    assert!(
        has(&report.failures, "differs from what catalog"),
        "{report:?}"
    );
}

#[test]
fn curated_footer_links_count_for_membership_but_not_generated_entries() {
    let tmp = fixture(&[("alpha.md", good("alpha", "First", "fact"))]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    snapshot.index.extend_from_slice(
        b"\n## Appendix\n\n- [alpha](pages/alpha.md) \xe2\x80\x94 curated prose\n",
    );
    let report = validate_wiki(&snapshot);
    assert!(report.failures.is_empty(), "{report:?}");
}

#[test]
fn line_reference_exemptions_forward_links_size_and_reachability_are_reported() {
    let large = format!(
        "URLs https://example.test/file.py:40 are exempt.\n```\nshown.py:2\n```\nreal.py:7\n[[future]]\n{}",
        "x".repeat(12_400)
    );
    let tmp = fixture(&[
        ("alpha.md", good("alpha", "First", &large)),
        ("beta.md", good("beta", "Second", "fact")),
    ]);
    let report = check_wiki(tmp.path()).unwrap();
    assert!(has(&report.failures, "real.py:7"), "{report:?}");
    assert!(!has(&report.failures, "file.py:40"), "{report:?}");
    assert!(!has(&report.failures, "shown.py:2"), "{report:?}");
    assert!(has(&report.notes, "not yet written"), "{report:?}");
    assert!(has(&report.notes, "oversized page"), "{report:?}");
    assert!(has(&report.notes, "no other page links"), "{report:?}");
}

#[test]
fn captured_shape_outcomes_are_typed_and_validation_names_each_problem() {
    let tmp = fixture(&[("alpha.md", good("alpha", "First", "fact"))]);
    fs::create_dir(tmp.path().join("pages/directory.md")).unwrap();
    let outside = tmp.path().join("outside.md");
    fs::write(&outside, good("linked", "Outside", "fact")).unwrap();
    symlink(&outside, tmp.path().join("pages/linked.md")).unwrap();
    let snapshot = capture_wiki(tmp.path()).unwrap();
    assert!(
        snapshot
            .pages
            .iter()
            .any(|page| matches!(page.outcome, CapturedPageOutcome::NotRegular))
    );
    assert!(
        snapshot
            .pages
            .iter()
            .any(|page| matches!(page.outcome, CapturedPageOutcome::Symlink))
    );
    let report = validate_wiki(&snapshot);
    assert!(
        has(&report.failures, "directory.md is not a regular file"),
        "{report:?}"
    );
    assert!(
        has(&report.failures, "linked.md is a symlink"),
        "{report:?}"
    );
}

#[test]
fn symlink_diagnostics_match_target_shape_without_following_content() {
    let tmp = fixture(&[("alpha.md", good("alpha", "First", "fact"))]);
    fs::write(
        tmp.path().join("regular-target"),
        good("linked", "Outside", "fact"),
    )
    .unwrap();
    fs::create_dir(tmp.path().join("directory-target")).unwrap();
    symlink(
        "../regular-target",
        tmp.path().join("pages/regular-link.md"),
    )
    .unwrap();
    symlink(
        "../directory-target",
        tmp.path().join("pages/directory-link.md"),
    )
    .unwrap();
    symlink(
        "../missing-target",
        tmp.path().join("pages/dangling-link.md"),
    )
    .unwrap();

    let snapshot = capture_wiki(tmp.path()).unwrap();
    let outcome = |name: &str| {
        &snapshot
            .pages
            .iter()
            .find(|page| page.name.as_bytes() == name.as_bytes())
            .unwrap()
            .outcome
    };
    assert!(matches!(
        outcome("regular-link.md"),
        CapturedPageOutcome::Symlink
    ));
    assert!(matches!(
        outcome("directory-link.md"),
        CapturedPageOutcome::NotRegular
    ));
    assert!(matches!(
        outcome("dangling-link.md"),
        CapturedPageOutcome::NotRegular
    ));
    let report = validate_wiki(&snapshot);
    assert!(
        has(&report.failures, "regular-link.md is a symlink"),
        "{report:?}"
    );
    assert!(
        has(&report.failures, "directory-link.md is not a regular file"),
        "{report:?}"
    );
    assert!(
        has(&report.failures, "dangling-link.md is not a regular file"),
        "{report:?}"
    );
}

#[test]
fn non_utf8_candidate_names_remain_distinct_and_are_escaped_in_diagnostics() {
    let tmp = fixture(&[("alpha.md", good("alpha", "First", "fact"))]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    for byte in [0x80, 0x81] {
        let name = OsString::from_vec(vec![b'b', b'a', b'd', b'-', byte, b'.', b'm', b'd']);
        snapshot.pages.push(yams_wiki::CapturedPage {
            name,
            outcome: CapturedPageOutcome::Present(good("bad", "Bad", "fact").into_bytes()),
        });
    }
    let raw_names = snapshot
        .pages
        .iter()
        .map(|page| page.name.as_bytes().to_vec())
        .collect::<Vec<_>>();
    assert!(
        raw_names.iter().any(|name| name.contains(&0x80)),
        "{raw_names:?}"
    );
    assert!(
        raw_names.iter().any(|name| name.contains(&0x81)),
        "{raw_names:?}"
    );
    let report = validate_wiki(&snapshot);
    assert!(has(&report.failures, r"bad-\x80.md"), "{report:?}");
    assert!(has(&report.failures, r"bad-\x81.md"), "{report:?}");
}

#[test]
fn malformed_index_and_non_utf8_pages_are_reported_from_captured_bytes() {
    let tmp = fixture(&[("alpha.md", good("alpha", "First", "fact"))]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    snapshot.index = b"\xff".to_vec();
    let report = validate_wiki(&snapshot);
    assert_eq!(
        report.failures,
        [format!(
            "{} is not valid UTF-8",
            snapshot.corpus.join("INDEX.md").display()
        )]
    );

    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    let alpha = snapshot
        .pages
        .iter_mut()
        .find(|page| page.name == "alpha.md")
        .unwrap();
    alpha.outcome = CapturedPageOutcome::Present(vec![0xff]);
    let report = validate_wiki(&snapshot);
    assert!(has(&report.failures, "alpha.md is not valid UTF-8"));
}

#[test]
fn generated_heading_order_is_checked_independently_of_the_renderer() {
    let tmp = fixture(&[
        ("alpha.md", good("alpha", "First", "fact")),
        (
            "beta.md",
            page(
                "beta",
                "Beta",
                "pattern",
                "current",
                "shared",
                "2026-08-08",
                "2026-08-08",
                "Second",
                "fact",
            ),
        ),
    ]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    let index = String::from_utf8(snapshot.index).unwrap();
    let canonical = "## Gotchas\n\n- [alpha](pages/alpha.md) — First\n\n## Patterns\n\n- [beta](pages/beta.md) — Second\n";
    let reversed = "## Patterns\n\n- [beta](pages/beta.md) — Second\n\n## Gotchas\n\n- [alpha](pages/alpha.md) — First\n";
    assert!(index.contains(canonical));
    snapshot.index = index.replace(canonical, reversed).into_bytes();
    let report = validate_wiki(&snapshot);
    assert!(
        has(&report.failures, "not in canonical order"),
        "{report:?}"
    );
}

#[test]
fn generated_slug_order_is_checked_independently_within_each_section() {
    let tmp = fixture(&[
        ("alpha.md", good("alpha", "First", "fact")),
        ("beta.md", good("beta", "Second", "[[alpha]]")),
    ]);
    let mut snapshot = capture_wiki(tmp.path()).unwrap();
    let index = String::from_utf8(snapshot.index).unwrap();
    let ordered = "- [alpha](pages/alpha.md) — First\n- [beta](pages/beta.md) — Second\n";
    let reversed = "- [beta](pages/beta.md) — Second\n- [alpha](pages/alpha.md) — First\n";
    assert!(index.contains(ordered));
    snapshot.index = index.replace(ordered, reversed).into_bytes();
    let report = validate_wiki(&snapshot);
    assert!(
        has(&report.failures, "entries are not in canonical order"),
        "{report:?}"
    );
}

#[test]
fn alias_and_heading_links_resolve_to_their_target_slug() {
    let tmp = fixture(&[
        (
            "alpha.md",
            good(
                "alpha",
                "A summary",
                "see [[beta|Beta page]] and [[gamma#Details]]",
            ),
        ),
        ("beta.md", good("beta", "B summary", "fact [[alpha]]")),
        ("gamma.md", good("gamma", "G summary", "fact [[alpha]]")),
    ]);
    let report = check_wiki(tmp.path()).unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert!(!has(&report.notes, "not yet written"), "{report:?}");
    assert!(!has(&report.notes, "unreachable"), "{report:?}");
}

#[test]
fn block_refs_and_embeds_create_no_link_graph_edges() {
    let tmp = fixture(&[
        (
            "alpha.md",
            good("alpha", "A summary", "see [[beta#^b1]] and ![[gamma]]"),
        ),
        ("beta.md", good("beta", "B summary", "fact [[alpha]]")),
        ("gamma.md", good("gamma", "G summary", "fact [[alpha]]")),
    ]);
    let report = check_wiki(tmp.path()).unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert!(
        has(&report.notes, "no other page links [[beta]]"),
        "{report:?}"
    );
    assert!(
        has(&report.notes, "no other page links [[gamma]]"),
        "{report:?}"
    );
}

fn compat_fixture(pages: &[(&str, String)]) -> TempDir {
    // Unlike `fixture`, no reindex: compat fixtures may be deliberately
    // unparseable, and the compat report does not validate the index.
    let tmp = tempdir().unwrap();
    fs::create_dir(tmp.path().join("pages")).unwrap();
    fs::write(
        tmp.path().join("INDEX.md"),
        format!("Preamble.\n{BEGIN_MARKER}\n\n{END_MARKER}\n"),
    )
    .unwrap();
    for (name, source) in pages {
        fs::write(tmp.path().join("pages").join(name), source).unwrap();
    }
    tmp
}

#[test]
fn obsidian_profile_violations_are_reported_without_rewriting() {
    let offending = good(
        "alpha",
        "A summary",
        "embed ![[beta]]\n\nblock ref [[beta#^b1]]\n\ntext ^b1\n\nlink [[Some Page]]",
    );
    let before = offending.clone();
    let tmp = compat_fixture(&[("alpha.md", offending)]);
    let report = compat_wiki(tmp.path()).unwrap();
    assert!(has(&report.violations, "embed"), "{report:?}");
    assert!(has(&report.violations, "block reference"), "{report:?}");
    assert!(has(&report.violations, "block ID"), "{report:?}");
    assert!(has(&report.violations, "not a Yams slug"), "{report:?}");
    let after = fs::read_to_string(tmp.path().join("pages/alpha.md")).unwrap();
    assert_eq!(after, before);
}

#[test]
fn within_profile_constructs_are_not_flagged() {
    let body = "> [!note] a callout\n\n==highlighted== and a #tag\n\n%% hidden but searchable %%\n\nlinks [[beta|Beta]] and [[gamma#Details]]";
    let tmp = compat_fixture(&[
        ("alpha.md", good("alpha", "A summary", body)),
        ("beta.md", good("beta", "B summary", "fact")),
        ("gamma.md", good("gamma", "G summary", "fact")),
    ]);
    let report = compat_wiki(tmp.path()).unwrap();
    assert!(report.violations.is_empty(), "{report:?}");
    assert_eq!(report.page_count, 3);
}

#[test]
fn obsidian_added_frontmatter_keys_are_violations() {
    let tagged = good("alpha", "A summary", "fact")
        .replace("summary: A summary", "summary: A summary\ntags: [x]");
    let tmp = compat_fixture(&[("alpha.md", tagged)]);
    let report = compat_wiki(tmp.path()).unwrap();
    assert!(
        has(&report.violations, "unknown frontmatter key: tags"),
        "{report:?}"
    );
}

#[test]
fn memory_base_dashboard_references_only_canonical_properties() {
    let path =
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/obsidian/Memory.base");
    let base = fs::read_to_string(path).unwrap();
    for key in [
        "title", "type", "status", "owner", "updated", "verified", "summary",
    ] {
        assert!(
            base.contains(&format!("      - {key}\n")),
            "Memory.base is missing canonical property {key}"
        );
    }
}
