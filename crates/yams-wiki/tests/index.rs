use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::time::Duration;

use sha2::{Digest, Sha256};
use tempfile::tempdir;
use yams_wiki::{
    BEGIN_MARKER, END_MARKER, IndexPage, LockLease, LockMode, PageType, ReindexOptions,
    acquire_lock, adopt_legacy, canonical_index_digest, check_index, parse_index_page,
    rebuild_index, reindex_wiki,
};

fn page(slug: &str, page_type: PageType, summary: &str) -> IndexPage {
    IndexPage {
        slug: slug.to_owned(),
        page_type,
        summary: summary.to_owned(),
    }
}

fn marked(region: &str) -> String {
    format!("Preamble.\n{BEGIN_MARKER}\n{region}{END_MARKER}\nFooter.\n")
}

fn retired_begin_marker() -> String {
    let brand = String::from_utf8(vec![0x6d, 0x6f, 0x6e, 0x65, 0x74, 0x61]).unwrap();
    format!("<!-- BEGIN GENERATED INDEX — edited by {brand}-wiki reindex, not by hand -->")
}

#[test]
fn generated_entries_follow_fixed_groups_slug_order_and_exact_lines() {
    let current = marked("old\n");
    let pages = vec![
        page("zeta", PageType::Gotcha, "last"),
        page("alpha", PageType::Gotcha, "first"),
        page("pattern", PageType::Pattern, "shape"),
        page("decision", PageType::Decision, "choice"),
        page("workflow", PageType::Workflow, "steps"),
        page("state", PageType::ProjectState, "now"),
        page("feature", PageType::Feature, "pointer"),
    ];

    let rebuilt = rebuild_index(&current, &pages).unwrap();
    assert_eq!(
        rebuilt,
        format!(
            "Preamble.\n{BEGIN_MARKER}\n\n\
## Gotchas\n\n\
- [alpha](pages/alpha.md) — first\n\
- [zeta](pages/zeta.md) — last\n\n\
## Patterns\n\n\
- [pattern](pages/pattern.md) — shape\n\n\
## Decisions\n\n\
- [decision](pages/decision.md) — choice\n\n\
## Workflow\n\n\
- [workflow](pages/workflow.md) — steps\n\n\
## Project state\n\n\
- [state](pages/state.md) — now\n\n\
## Features — architecture pointers\n\n\
- [feature](pages/feature.md) — pointer\n\n\
{END_MARKER}\nFooter.\n"
        )
    );
}

#[test]
fn empty_groups_are_omitted() {
    let rebuilt = rebuild_index(&marked(""), &[page("only", PageType::Decision, "one")]).unwrap();
    assert!(rebuilt.contains("## Decisions\n"));
    for absent in [
        "## Gotchas\n",
        "## Patterns\n",
        "## Workflow\n",
        "## Project state\n",
        "## Features — architecture pointers\n",
    ] {
        assert!(!rebuilt.contains(absent), "{absent:?}");
    }
}

#[test]
fn marker_rebuild_preserves_every_byte_outside_the_owned_region() {
    for separator in ["\n", "\r\n", "\r"] {
        let prefix = format!("Preamble.{separator}{BEGIN_MARKER}");
        let suffix = format!(
            "{END_MARKER}{separator}## Curated{separator}{separator}\
- [alpha](pages/alpha.md) — curated footer prose{separator}"
        );
        let current = format!("{prefix}{separator}old{separator}{suffix}");
        let rebuilt =
            rebuild_index(&current, &[page("alpha", PageType::Gotcha, "generated")]).unwrap();
        assert!(rebuilt.starts_with(&prefix), "{separator:?}");
        assert!(rebuilt.ends_with(&suffix), "{separator:?}");
        assert!(rebuilt.contains("- [alpha](pages/alpha.md) — generated\n"));
    }
}

#[test]
fn canonical_bytes_match_python_for_every_marker_line_ending() {
    let cases = [
        (
            "\n",
            format!("Preamble\n{BEGIN_MARKER}\n\n\n\n{END_MARKER}\nFooter\n"),
            format!(
                "Preamble\n{BEGIN_MARKER}\n\n## Gotchas\n\n- [alpha](pages/alpha.md) — summary\n\n{END_MARKER}\nFooter\n"
            ),
        ),
        (
            "\r\n",
            format!("Preamble\r\n{BEGIN_MARKER}\r\n\n\n{END_MARKER}\r\nFooter\r\n"),
            format!(
                "Preamble\r\n{BEGIN_MARKER}\r\n## Gotchas\n\n- [alpha](pages/alpha.md) — summary\n\n{END_MARKER}\r\nFooter\r\n"
            ),
        ),
        (
            "\r",
            format!("Preamble\r{BEGIN_MARKER}\r\n\n\n{END_MARKER}\rFooter\r"),
            format!(
                "Preamble\r{BEGIN_MARKER}\r\n## Gotchas\n\n- [alpha](pages/alpha.md) — summary\n\n{END_MARKER}\rFooter\r"
            ),
        ),
    ];
    for (separator, empty, one) in cases {
        let current = format!(
            "Preamble{separator}{BEGIN_MARKER}{separator}old{separator}{END_MARKER}{separator}Footer{separator}"
        );
        assert_eq!(
            rebuild_index(&current, &[]).unwrap(),
            empty,
            "empty {separator:?}"
        );
        assert_eq!(
            rebuild_index(&current, &[page("alpha", PageType::Gotcha, "summary")]).unwrap(),
            one,
            "one page {separator:?}"
        );
    }
}

#[test]
fn every_ambiguous_marker_layout_is_refused() {
    let cases = [
        "no markers\n".to_owned(),
        format!("{BEGIN_MARKER}\n"),
        format!("{END_MARKER}\n"),
        format!("{END_MARKER}\n{BEGIN_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{BEGIN_MARKER}\n{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{END_MARKER}\n{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\n{BEGIN_MARKER}\n{END_MARKER}\n{END_MARKER}\n"),
        format!("inline {BEGIN_MARKER}\n{END_MARKER}\n"),
        format!("{BEGIN_MARKER}\ninline {END_MARKER} suffix\n"),
    ];
    for current in cases {
        assert!(rebuild_index(&current, &[]).is_err(), "{current:?}");
    }
}

#[test]
fn pages_are_validated_before_rendering() {
    let current = marked("");
    let cases = [
        vec![
            page("same", PageType::Gotcha, "one"),
            page("same", PageType::Pattern, "two"),
        ],
        vec![page("Not-A-Slug", PageType::Gotcha, "one")],
        vec![page("okay", PageType::Gotcha, "")],
        vec![page("okay", PageType::Gotcha, "hides <!-- everything")],
        vec![page("okay", PageType::Gotcha, "forges (pages/ghost.md)")],
    ];
    for pages in cases {
        assert!(rebuild_index(&current, &pages).is_err(), "{pages:?}");
    }
}

#[test]
fn canonical_slug_boundary_is_shared_by_parsed_and_rendered_index_pages() {
    let current = marked("");
    for length in 1..=64 {
        let slug = "a".repeat(length);
        let source = page_source(&slug, "gotcha", "summary");
        assert_eq!(
            parse_index_page(&format!("{slug}.md"), &source).unwrap(),
            page(&slug, PageType::Gotcha, "summary"),
            "{length}"
        );
        assert!(
            rebuild_index(&current, &[page(&slug, PageType::Gotcha, "summary")]).is_ok(),
            "{length}"
        );
    }

    let too_long = "a".repeat(65);
    let filename = format!("{too_long}.md");
    let source = page_source(&too_long, "gotcha", "summary");
    assert_eq!(
        parse_index_page(&filename, &source)
            .unwrap_err()
            .to_string(),
        format!("invalid index page {filename}: slug must be at most 64 bytes")
    );
    assert_eq!(
        rebuild_index(&current, &[page(&too_long, PageType::Gotcha, "summary")])
            .unwrap_err()
            .to_string(),
        format!("invalid index page {too_long}: slug must be at most 64 bytes")
    );
}

#[test]
fn permissive_core_frontmatter_is_enough_for_reindex_page_input() {
    let source =
        "---\nslug: alpha\n  nested: ignored\ntype: gotcha\nsummary: a real page\n---\nbody\n";
    assert_eq!(
        parse_index_page("alpha.md", source).unwrap(),
        page("alpha", PageType::Gotcha, "a real page")
    );

    for (name, source) in [
        (
            "beta.md",
            "---\nslug: alpha\ntype: gotcha\nsummary: okay\n---\n",
        ),
        (
            "alpha.md",
            "---\nslug: alpha\ntype: unknown\nsummary: okay\n---\n",
        ),
        (
            "alpha.md",
            "---\nslug: alpha\ntype: gotcha\nsummary: \n---\n",
        ),
    ] {
        assert!(
            parse_index_page(name, source).is_err(),
            "{name}: {source:?}"
        );
    }
}

#[test]
fn empty_index_link_destination_is_not_link_shaped() {
    let parsed = parse_index_page(
        "alpha.md",
        "---\nslug: alpha\ntype: gotcha\nsummary: see (pages/.md) literally\n---\n",
    )
    .unwrap();
    assert_eq!(parsed.summary, "see (pages/.md) literally");
}

#[test]
fn check_returns_canonical_equality_and_internal_unified_diff() {
    let pages = [page("alpha", PageType::Gotcha, "summary")];
    let drifted = marked("old\n");
    let check = check_index(&drifted, &pages).unwrap();
    assert!(!check.canonical);
    let diff = check.diff.expect("drift includes a diagnostic diff");
    assert!(diff.starts_with("--- INDEX.md\n+++ canonical INDEX.md\n@@"));
    assert!(diff.contains("-old"));
    assert!(diff.contains("+- [alpha](pages/alpha.md) — summary"));

    let rebuilt = rebuild_index(&drifted, &pages).unwrap();
    assert_eq!(
        check_index(&rebuilt, &pages).unwrap(),
        yams_wiki::IndexCheck {
            canonical: true,
            diff: None,
        }
    );
}

const LEGACY: &str = "Preamble with CRLF.\r\n\r\n\
## Gotchas — tooling and environment\r\n\r\n\
- [Human label](pages/alpha.md) — a trap\r\n\r\n\
## Gotchas — retrieval traps\r\n\r\n\
- [beta](pages/beta.md) — another trap\r\n\r\n\
## Decisions\r\n\r\n\
## Patterns\r\n\r\n\
## Workflow\r\n\r\n\
## Features — architecture pointers\r\n";

#[test]
fn legacy_adoption_preserves_preamble_and_introduces_one_marker_pair() {
    let adopted = adopt_legacy(LEGACY).unwrap();
    assert!(adopted.starts_with("Preamble with CRLF.\r\n\r\n"));
    assert_eq!(adopted.matches(BEGIN_MARKER).count(), 1);
    assert_eq!(adopted.matches(END_MARKER).count(), 1);
    assert!(!adopted.contains("pages/alpha.md"));
    assert!(adopt_legacy(&adopted).is_err());
}

#[test]
fn retired_marked_adoption_preserves_every_byte_outside_the_generated_region() {
    let retired = retired_begin_marker();
    let original = format!(
        "Preamble with CRLF.\r\n\r\n{retired}\r\n\n## Gotchas\n\n\
- [alpha](pages/alpha.md) — a trap\n\n{END_MARKER}\r\nFooter stays byte-exact.\r\n"
    );

    let adopted = adopt_legacy(&original).unwrap();
    let expected = original.replacen(&retired, BEGIN_MARKER, 1);
    assert_eq!(adopted, expected);
    assert_eq!(adopted.matches(BEGIN_MARKER).count(), 1);
    assert_eq!(adopted.matches(END_MARKER).count(), 1);
}

#[test]
fn previous_yams_reindex_marker_is_adoptable() {
    let retired = "<!-- BEGIN GENERATED INDEX — edited by yams-wiki reindex, not by hand -->";
    let original = format!(
        "Preamble.\n{retired}\n\n## Gotchas\n\n- [alpha](pages/alpha.md) — a trap\n\n{END_MARKER}\nFooter.\n"
    );

    let adopted = adopt_legacy(&original).unwrap();

    assert_eq!(adopted, original.replacen(retired, BEGIN_MARKER, 1));
    assert!(adopted.contains(BEGIN_MARKER));
    assert!(!adopted.contains(retired));
}

#[test]
fn retired_marked_adoption_fails_closed_for_ambiguous_shapes() {
    let retired = retired_begin_marker();
    let cases = [
        format!("{retired}\n"),
        format!("{END_MARKER}\n"),
        format!("{END_MARKER}\n{retired}\n"),
        format!("{retired}\n{retired}\n{END_MARKER}\n"),
        format!("{retired}\n{END_MARKER}\n{END_MARKER}\n"),
        format!("inline {retired}\n{END_MARKER}\n"),
        format!("{retired}\ninline {END_MARKER}\n"),
        format!("{retired}\n{BEGIN_MARKER}\n{END_MARKER}\n"),
    ];
    for current in cases {
        assert!(adopt_legacy(&current).is_err(), "{current:?}");
    }
}

#[test]
fn legacy_adoption_refuses_every_unrecognised_tail_shape() {
    let cases = [
        format!("{LEGACY}footer prose\n"),
        LEGACY.replace("## Workflow", "## Unknown"),
        LEGACY.replace(
            "- [beta](pages/beta.md) — another trap",
            "prefix - [beta](pages/beta.md) — another trap",
        ),
        LEGACY.replace(
            "- [beta](pages/beta.md) — another trap",
            "- [beta](pages/beta.md) no em dash",
        ),
        LEGACY.replace("\r\n\r\n## Workflow", "\r\nnot blank\r\n## Workflow"),
        format!("{LEGACY}{BEGIN_MARKER}\n"),
    ];
    for legacy in cases {
        assert!(adopt_legacy(&legacy).is_err(), "{legacy:?}");
    }
}

fn page_source(slug: &str, page_type: &str, summary: &str) -> String {
    format!(
        "---\nslug: {slug}\ntitle: ignored by reindex\ntype: {page_type}\nsummary: {summary}\n---\nbody\n"
    )
}

fn wiki_fixture() -> tempfile::TempDir {
    let tmp = tempdir().unwrap();
    fs::create_dir(tmp.path().join("pages")).unwrap();
    fs::write(
        tmp.path().join("pages/alpha.md"),
        page_source("alpha", "gotcha", "first summary"),
    )
    .unwrap();
    fs::write(tmp.path().join("INDEX.md"), marked("old\n")).unwrap();
    tmp
}

#[test]
fn public_reindex_rewrites_then_check_and_digest_bind_the_same_bytes() {
    let tmp = wiki_fixture();
    let result = reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
    assert!(result.changed);
    assert!(result.diff.is_some());
    assert!(result.isolation_note.is_none());

    let bytes = fs::read(tmp.path().join("INDEX.md")).unwrap();
    assert_eq!(result.index_sha256, format!("{:x}", Sha256::digest(&bytes)));
    let checked = reindex_wiki(
        tmp.path(),
        &ReindexOptions {
            check_only: true,
            ..ReindexOptions::default()
        },
    )
    .unwrap();
    assert!(!checked.changed);
    assert!(checked.diff.is_none());
    assert_eq!(
        canonical_index_digest(tmp.path()).unwrap().digest,
        Some(checked.index_sha256)
    );
}

#[test]
fn stale_expected_digest_wins_over_invalid_utf8() {
    let tmp = wiki_fixture();
    fs::write(tmp.path().join("INDEX.md"), b"\xff\xfe").unwrap();
    let error = reindex_wiki(
        tmp.path(),
        &ReindexOptions {
            expected_sha256: Some("0".repeat(64)),
            ..ReindexOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("changed between"), "{error}");
}

#[test]
fn adoption_diff_compares_the_original_legacy_bytes_to_the_final_index() {
    for with_page in [false, true] {
        let tmp = wiki_fixture();
        if !with_page {
            fs::remove_file(tmp.path().join("pages/alpha.md")).unwrap();
        }
        fs::write(tmp.path().join("INDEX.md"), LEGACY).unwrap();
        let result = reindex_wiki(
            tmp.path(),
            &ReindexOptions {
                check_only: true,
                adopt: true,
                ..ReindexOptions::default()
            },
        )
        .unwrap();
        let diff = result
            .diff
            .expect("adoption check exposes the real rewrite");
        assert!(
            diff.contains("-## Gotchas — tooling and environment"),
            "{with_page}: {diff}"
        );
        assert!(
            diff.contains(&format!("+{BEGIN_MARKER}")),
            "{with_page}: {diff}"
        );
        assert!(
            diff.contains(&format!("+{END_MARKER}")),
            "{with_page}: {diff}"
        );
    }
}

#[test]
fn pages_and_index_are_opened_without_following_symlinks() {
    for target in ["pages", "INDEX.md"] {
        let tmp = wiki_fixture();
        let outside = tempdir().unwrap();
        if target == "pages" {
            fs::remove_dir_all(tmp.path().join("pages")).unwrap();
            symlink(outside.path(), tmp.path().join("pages")).unwrap();
        } else {
            fs::write(outside.path().join("index"), marked("")).unwrap();
            fs::remove_file(tmp.path().join("INDEX.md")).unwrap();
            symlink(outside.path().join("index"), tmp.path().join("INDEX.md")).unwrap();
        }
        let error = reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap_err();
        assert!(error.to_string().contains(target), "{target}: {error}");
    }
}

#[test]
fn rewrite_preserves_the_exact_target_mode() {
    for mode in [0o600, 0o644, 0o4755, 0o2755, 0o1755] {
        let tmp = wiki_fixture();
        fs::set_permissions(
            tmp.path().join("INDEX.md"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        let installed = fs::metadata(tmp.path().join("INDEX.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        if installed != mode {
            eprintln!("host refused requested mode {mode:o}; installed {installed:o}");
            continue;
        }
        reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
        assert_eq!(
            fs::metadata(tmp.path().join("INDEX.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            mode
        );
    }
}

#[test]
fn an_immediate_regular_lowercase_markdown_page_is_the_only_page_input() {
    let tmp = wiki_fixture();
    fs::write(
        tmp.path().join("pages/beta.MD"),
        page_source("beta", "gotcha", "ignored uppercase extension"),
    )
    .unwrap();
    fs::create_dir(tmp.path().join("pages/nested")).unwrap();
    fs::write(
        tmp.path().join("pages/nested/gamma.md"),
        page_source("gamma", "gotcha", "ignored nested page"),
    )
    .unwrap();
    reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
    let index = fs::read_to_string(tmp.path().join("INDEX.md")).unwrap();
    assert!(index.contains("pages/alpha.md"));
    assert!(!index.contains("pages/beta.MD"));
    assert!(!index.contains("pages/gamma.md"));
}

#[test]
fn check_only_uses_shared_lock_and_mutation_uses_exclusive_lock() {
    let tmp = wiki_fixture();
    reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
    let guard = match acquire_lock(tmp.path(), LockMode::Shared).unwrap() {
        LockLease::Isolated(guard) => guard,
        LockLease::Unisolated(value) => panic!("expected isolation, got {value:?}"),
    };
    let checked = reindex_wiki(
        tmp.path(),
        &ReindexOptions {
            check_only: true,
            lock_timeout: Duration::from_millis(20),
            ..ReindexOptions::default()
        },
    )
    .unwrap();
    assert!(!checked.changed);

    fs::write(
        tmp.path().join("pages/beta.md"),
        page_source("beta", "pattern", "new page"),
    )
    .unwrap();
    let error = reindex_wiki(
        tmp.path(),
        &ReindexOptions {
            lock_timeout: Duration::from_millis(20),
            ..ReindexOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("exclusive"), "{error}");
    drop(guard);
}

#[test]
fn adoption_is_exclusive_even_when_combined_with_check_only() {
    let tmp = wiki_fixture();
    fs::write(tmp.path().join("INDEX.md"), LEGACY).unwrap();
    let guard = match acquire_lock(tmp.path(), LockMode::Shared).unwrap() {
        LockLease::Isolated(guard) => guard,
        LockLease::Unisolated(value) => panic!("expected isolation, got {value:?}"),
    };
    let error = reindex_wiki(
        tmp.path(),
        &ReindexOptions {
            check_only: true,
            adopt: true,
            lock_timeout: Duration::from_millis(20),
            ..ReindexOptions::default()
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("exclusive"), "{error}");
    drop(guard);
}
