use std::path::PathBuf;

use yams_cli::{DirectOperation, ParseOutcome, parse_direct_request};
use yams_core::ExitCode;

fn request(args: &[&str]) -> yams_cli::DirectRequest {
    match parse_direct_request(args.iter().copied()) {
        ParseOutcome::Request(request) => request,
        ParseOutcome::Completion(completion) => panic!(
            "expected request, got exit {:?}, stdout {:?}, stderr {:?}",
            completion.exit_code, completion.stdout, completion.stderr
        ),
    }
}

fn completion(args: &[&str]) -> yams_cli::DirectCompletion {
    match parse_direct_request(args.iter().copied()) {
        ParseOutcome::Request(request) => panic!("expected completion, got {request:?}"),
        ParseOutcome::Completion(completion) => completion,
    }
}

#[test]
fn every_argparse_long_prefix_is_accepted() {
    let cases: &[(&[&str], &[&str])] = &[
        (&["--j", "--js", "--jso", "--json"], &["--json"]),
        (&["--f", "--fu", "--ful", "--full"], &["--full"]),
        (&["--i", "--in", "--ind", "--inde", "--index"], &["--index"]),
        (&["--w", "--wr", "--wri", "--writ", "--write"], &["--write"]),
        (&["--s", "--st", "--sta", "--stat", "--stats"], &["--stats"]),
        (&["--a", "--al", "--all"], &["--all"]),
        (
            &[
                "--n",
                "--no",
                "--no-",
                "--no-g",
                "--no-ga",
                "--no-gat",
                "--no-gate",
            ],
            &["--no-gate"],
        ),
        (
            &["--e", "--ex", "--exp", "--expl", "--expla", "--explain"],
            &["--explain"],
        ),
        (
            &[
                "--mi",
                "--min",
                "--min-",
                "--min-s",
                "--min-sc",
                "--min-sco",
                "--min-scor",
                "--min-score",
            ],
            &["--min-score", "0.25"],
        ),
        (
            &[
                "--ma",
                "--max",
                "--max-",
                "--max-g",
                "--max-ga",
                "--max-gap",
            ],
            &["--max-gap", "0.25"],
        ),
        (&["--g", "--gc"], &["--gc"]),
    ];

    for (prefixes, canonical) in cases {
        for prefix in *prefixes {
            let mut argv = vec!["yams", *prefix];
            argv.extend_from_slice(&canonical[1..]);
            match canonical[0] {
                "--index" | "--write" | "--stats" | "--gc" => {}
                "--all" => argv.push("Fictional query"),
                _ => argv.push("Fictional query"),
            }
            let parsed = request(&argv);

            let mut expected_argv = vec!["yams"];
            expected_argv.extend_from_slice(canonical);
            match canonical[0] {
                "--index" | "--write" | "--stats" | "--gc" => {}
                _ => expected_argv.push("Fictional query"),
            }
            assert_eq!(parsed, request(&expected_argv), "{prefix}");
        }
    }

    for prefix in ["--h", "--he", "--hel", "--help"] {
        let done = completion(&["anything", prefix]);
        assert_eq!(done.exit_code, ExitCode::Ok, "{prefix}");
        assert!(
            done.stdout.starts_with("Semantic search"),
            "{prefix}: {}",
            done.stdout
        );
        assert!(
            done.stdout.contains("Usage: yams"),
            "{prefix}: {}",
            done.stdout
        );
    }
}

#[test]
fn version_prefixes_print_the_package_version_and_exit_ok() {
    let expected = format!("yams {}\n", env!("CARGO_PKG_VERSION"));
    for prefix in [
        "--v",
        "--ve",
        "--ver",
        "--vers",
        "--versi",
        "--versio",
        "--version",
    ] {
        let done = completion(&["anything", prefix]);
        assert_eq!(done.exit_code, ExitCode::Ok, "{prefix}");
        assert_eq!(done.stdout, expected, "{prefix}");
        assert_eq!(done.stderr, "", "{prefix}");
    }
}

#[test]
fn short_k_forms_and_equals_long_values_are_preserved() {
    for args in [
        vec!["alias", "-k", "7", "Silver orchard"],
        vec!["alias", "-k7", "Silver orchard"],
        vec!["alias", "-k=7", "Silver orchard"],
    ] {
        assert_eq!(request(&args).k, 7, "{args:?}");
    }
    assert_eq!(
        request(&["alias", "--min-score=0.2", "Silver", "orchard"]).query(),
        Some("Silver orchard")
    );
}

#[test]
fn negative_number_shaped_tokens_remain_positional_queries() {
    for query in ["-1", "-.5"] {
        let parsed = request(&["yams", query]);
        assert_eq!(parsed.query(), Some(query), "{query:?}");
    }
}

#[test]
fn separated_option_values_follow_argparse_negative_number_lexing() {
    let project_integer = request(&["yams", "--project", "-1", "q"]);
    assert_eq!(project_integer.project, Some(PathBuf::from("-1")));
    let project_decimal = request(&["yams", "--project", "-.5", "q"]);
    assert_eq!(project_decimal.project, Some(PathBuf::from("-.5")));

    assert_eq!(
        request(&["yams", "--min-score", "-1", "q"]).min_score,
        Some(-1.0)
    );
    assert_eq!(
        request(&["yams", "--min-score", "-.5", "q"]).min_score,
        Some(-0.5)
    );

    for value in ["-1", "-.5"] {
        let count = completion(&["yams", "-k", value, "q"]);
        assert!(
            count.stderr.contains(if value == "-1" {
                "-k must be 1 or greater; got -1"
            } else {
                "invalid int value: '-.5'"
            }),
            "{value:?}: {}",
            count.stderr
        );
        assert!(!count.stderr.contains("none was supplied"), "{value:?}");

        let gap = completion(&["yams", "--max-gap", value, "q"]);
        assert!(
            gap.stderr.contains("--max-gap must be 0.0 or greater"),
            "{value:?}: {}",
            gap.stderr
        );
        assert!(!gap.stderr.contains("none was supplied"), "{value:?}");
    }

    for option in ["-k", "--project", "--min-score", "--max-gap"] {
        for value in ["-1e-2", "-foo", "--json"] {
            let done = completion(&["yams", option, value, "q"]);
            assert_eq!(done.exit_code, ExitCode::Usage, "{option} {value}");
            assert!(
                done.stderr.contains("a value is required for"),
                "{option} {value}: {}",
                done.stderr
            );
            assert!(
                done.stderr.contains("none was supplied"),
                "{option} {value}: {}",
                done.stderr
            );
        }
    }
}

#[test]
fn ascii_space_makes_a_hyphen_token_positional_to_argparse() {
    assert_eq!(
        request(&["yams", "--project", "-foo bar", "q"]).project,
        Some(PathBuf::from("-foo bar"))
    );
    assert_eq!(
        request(&["yams", "--min-score", "-1e-2 ", "q"]).min_score,
        Some(-0.01)
    );

    for (args, expected) in [
        (
            vec!["yams", "-k", "-1 ", "q"],
            "-k must be 1 or greater; got -1",
        ),
        (
            vec!["yams", "--max-gap", "-.5 ", "q"],
            "--max-gap must be 0.0 or greater",
        ),
    ] {
        let done = completion(&args);
        assert!(done.stderr.contains(expected), "{args:?}: {}", done.stderr);
        assert!(!done.stderr.contains("none was supplied"), "{args:?}");
    }

    assert_eq!(request(&["yams", "-foo bar"]).query(), Some("-foo bar"));

    for compact_short in ["-k foo", "-h foo"] {
        let done = completion(&["yams", "--project", compact_short, "q"]);
        assert!(
            done.stderr.contains("a value is required for '--project"),
            "{compact_short:?}: {}",
            done.stderr
        );
    }
}

#[test]
fn equals_option_values_are_never_reinterpreted_as_options() {
    for value in ["-1", "-.5", "-1e-2", "-foo", "--json", ""] {
        let spelling = format!("--project={value}");
        assert_eq!(
            request(&["yams", &spelling, "q"]).project,
            Some(PathBuf::from(value)),
            "{spelling:?}"
        );
    }

    assert_eq!(
        request(&["yams", "--min-score=-1e-2", "q"]).min_score,
        Some(-0.01)
    );
    for spelling in ["--min-score=-1", "--min-score=-.5"] {
        assert!(request(&["yams", spelling, "q"]).min_score.unwrap() < 0.0);
    }

    let negative_count = completion(&["yams", "-k=-1", "q"]);
    assert!(negative_count.stderr.contains("-k must be 1 or greater"));
    assert!(!negative_count.stderr.contains("none was supplied"));
    for spelling in ["--max-gap=-1", "--max-gap=-.5", "--max-gap=-1e-2"] {
        let done = completion(&["yams", spelling, "q"]);
        assert!(done.stderr.contains("--max-gap must be 0.0 or greater"));
        assert!(!done.stderr.contains("none was supplied"));
    }

    for spelling in [
        "-k=-.5",
        "-k=-1e-2",
        "-k=-foo",
        "-k=--json",
        "-k=",
        "--min-score=-foo",
        "--min-score=--json",
        "--min-score=",
        "--max-gap=-foo",
        "--max-gap=--json",
        "--max-gap=",
    ] {
        let done = completion(&["yams", spelling, "q"]);
        assert_eq!(done.exit_code, ExitCode::Usage, "{spelling}");
        assert!(
            done.stderr.contains("invalid "),
            "{spelling}: {}",
            done.stderr
        );
        assert!(!done.stderr.contains("none was supplied"), "{spelling}");
    }
}

#[test]
fn generated_argument_placeholders_cannot_collide_with_user_input() {
    let collision = "\u{e000}yams-argument-0\u{e001}";
    assert_eq!(
        request(&["yams", collision, "-1"]).query(),
        Some(format!("{collision} -1").as_str())
    );

    let done = completion(&["yams", "--unknown", collision]);
    assert!(
        done.stderr.contains("unrecognized arguments: --unknown"),
        "{}",
        done.stderr
    );
    assert_eq!(done.stderr.matches("--unknown").count(), 1);
}

#[cfg(unix)]
#[test]
fn project_values_preserve_non_utf8_os_strings() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let project = OsString::from_vec(vec![b'p', 0xff]);
    let parsed = match parse_direct_request(vec![
        OsString::from("yams"),
        OsString::from("--project"),
        project.clone(),
        OsString::from("query"),
    ]) {
        ParseOutcome::Request(request) => request,
        ParseOutcome::Completion(completion) => panic!("unexpected completion: {completion:?}"),
    };
    assert_eq!(parsed.project, Some(PathBuf::from(project)));
}

#[test]
fn repeated_options_use_argparse_self_override_semantics() {
    let parsed = request(&[
        "yams",
        "-k",
        "2",
        "-k=7",
        "--project",
        "/fictional/first",
        "--project=/fictional/second",
        "--min-score",
        "0.2",
        "--mi=0.3",
        "--max-gap=0.4",
        "--ma",
        "0.5",
        "--json",
        "--j",
        "--full",
        "--f",
        "query",
    ]);
    assert_eq!(parsed.k, 7);
    assert_eq!(parsed.project, Some(PathBuf::from("/fictional/second")));
    assert_eq!(parsed.min_score, Some(0.3));
    assert_eq!(parsed.max_gap, Some(0.5));
    assert!(parsed.json);
    assert!(parsed.full);

    assert_eq!(
        request(&["yams", "--all", "--a", "query"]).operation,
        DirectOperation::All
    );
    assert_eq!(
        request(&["yams", "--write", "--w"]).operation,
        DirectOperation::Write
    );
    let conflict = completion(&["yams", "--stats", "--stats", "--gc"]);
    assert_eq!(
        conflict.stderr,
        "choose exactly one operation; cannot combine --stats, --gc\n"
    );
}

#[test]
fn a_variadic_query_cannot_resume_after_an_option_interrupts_it() {
    for args in [
        vec!["yams", "a", "--json", "b"],
        vec!["yams", "a", "-k", "2", "b"],
    ] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert_eq!(done.stdout, "", "{args:?}");
        assert!(
            done.stderr.contains("unrecognized arguments: b"),
            "{args:?}: {}",
            done.stderr
        );
        assert!(done.stderr.contains("Usage: yams"), "{args:?}");
    }
}

#[test]
fn k_accepts_python_decimal_integer_spellings_without_an_i64_ceiling() {
    for (spelling, expected) in [
        ("1_000", 1_000usize),
        ("１２", 12),
        ("١٢", 12),
        ("+10", 10),
        (" 12 ", 12),
    ] {
        assert_eq!(
            request(&["yams", "-k", spelling, "query"]).k,
            expected,
            "{spelling:?}"
        );
    }

    let huge = "999999999999999999999999999999999999999999999999999999999999";
    let huge_request = request(&["yams", "-k", huge, "query"]);
    assert_eq!(huge_request.k, usize::MAX);
    assert_eq!(huge_request.requested_k, huge);

    for (spelling, diagnostic) in [
        ("-1_0", "-k must be 1 or greater; got -10"),
        ("００", "-k must be 1 or greater; got 0"),
    ] {
        let option = format!("-k={spelling}");
        assert_eq!(
            completion(&["yams", &option, "query"]).stderr,
            format!("{diagnostic}\n")
        );
    }
    for spelling in ["²", "1__0", "_10", "10_"] {
        let done = completion(&["yams", "-k", spelling, "query"]);
        assert_eq!(done.exit_code, ExitCode::Usage);
        assert!(
            done.stderr
                .contains(&format!("invalid int value: '{spelling}'")),
            "{spelling:?}: {}",
            done.stderr
        );
    }

    let information_separators = "\u{1c}1\u{1f}";
    let done = completion(&["yams", "-k", information_separators, "query"]);
    assert_eq!(done.exit_code, ExitCode::Usage);
    assert!(done.stderr.contains("invalid int value"), "{}", done.stderr);
}

#[test]
fn k_enforces_pythons_default_decimal_digit_limit() {
    let accepted = "9".repeat(4_300);
    let parsed = request(&["yams", "-k", &accepted, "query"]);
    assert_eq!(parsed.k, usize::MAX);
    assert_eq!(parsed.requested_k, accepted);

    for refused in ["9".repeat(4_301), "０".repeat(4_301)] {
        let done = completion(&["yams", "-k", &refused, "query"]);
        assert_eq!(done.exit_code, ExitCode::Usage);
        assert!(done.stderr.contains("invalid int value"), "{}", done.stderr);
    }

    // Underscores do not count toward CPython's conversion limit.
    let underscored = std::iter::repeat_n("1", 4_300)
        .collect::<Vec<_>>()
        .join("_");
    let parsed = request(&["yams", "-k", &underscored, "query"]);
    assert_eq!(parsed.k, usize::MAX);
    assert_eq!(parsed.requested_k, "1".repeat(4_300));
}

#[test]
fn floats_accept_exact_python_312_spellings() {
    for (spelling, expected) in [
        ("\u{a0}+١_٢.３_４e+５_６\u{3000}", 1.234e57),
        (".５", 0.5),
        ("１.", 1.0),
        ("1_2.3_4e5_6", 1.234e57),
        ("\t-0.0\n", -0.0),
    ] {
        let parsed = request(&["yams", "--max-gap", spelling, "query"]);
        let actual = parsed.max_gap.unwrap();
        assert_eq!(actual, expected, "{spelling:?}");
        assert_eq!(actual.is_sign_negative(), expected.is_sign_negative());
    }

    for spelling in ["-Infinity", "+NaN"] {
        let done = completion(&["yams", "--max-gap", spelling, "query"]);
        assert_eq!(done.exit_code, ExitCode::Usage, "{spelling:?}");
        assert!(
            !done.stderr.contains("invalid float value"),
            "{}",
            done.stderr
        );
    }
}

#[test]
fn floats_reject_exact_python_312_invalid_shapes() {
    for spelling in [
        "\u{1c}1\u{1f}",
        "1__0",
        "_1",
        "1_.0",
        "1._0",
        "1e_2",
        "1e2_",
        "nan_",
        "in_f",
        "0x1p2",
        "\u{200b}1\u{200b}",
    ] {
        let done = completion(&["yams", "--max-gap", spelling, "query"]);
        assert_eq!(done.exit_code, ExitCode::Usage, "{spelling:?}");
        assert!(
            done.stderr.contains("invalid float value"),
            "{spelling:?}: {}",
            done.stderr
        );
    }
}

#[test]
fn human_diagnostics_strip_terminal_controls_without_forging_lines() {
    let hostile = "--unknown=\u{1b}[2J\nforged\u{85}";
    let done = completion(&["yams", hostile, "query"]);
    assert_eq!(done.exit_code, ExitCode::Usage);
    assert_eq!(
        done.stderr,
        "error: unrecognized arguments: --unknown=[2Jforged\n\nUsage: yams [OPTIONS] [QUERY]...\n"
    );
    assert!(!done.stderr.contains('\u{1b}'));

    let write = completion(&["yams", "--write", hostile]);
    let body: serde_json::Value = serde_json::from_str(write.stdout.trim()).unwrap();
    assert_eq!(
        body["error"],
        "unrecognized arguments: --unknown=\u{1b}[2J\nforged\u{85}"
    );
    assert!(write.stdout.contains("\\u001b[2J\\nforged\\u0085"));
    assert!(!write.stdout.contains('\u{1b}'));
    assert!(!write.stdout.contains('\u{85}'));
    assert_eq!(write.stderr, "");

    let invalid_float = "x\n\ny\u{1b}[2J";
    let write = completion(&["yams", "--write", "--max-gap", invalid_float]);
    let body: serde_json::Value = serde_json::from_str(write.stdout.trim()).unwrap();
    assert_eq!(
        body["error"],
        "invalid value 'x\n\ny\u{1b}[2J' for '--max-gap <FLOAT>': invalid float value: 'x\n\ny\u{1b}[2J'"
    );
    assert!(write.stdout.contains("x\\n\\ny\\u001b[2J"));

    let human = completion(&["yams", "--max-gap", invalid_float, "query"]);
    assert_eq!(human.stderr.lines().count(), 3);
    assert!(human.stderr.starts_with(
        "error: invalid value 'xy[2J' for '--max-gap <FLOAT>': invalid float value: 'xy[2J'\n"
    ));

    for (args, expected) in [
        (
            vec!["yams", "--write=x\n\ny\u{1b}[2J"],
            "unexpected value 'x\n\ny\u{1b}[2J' for '--write' found; no more were expected",
        ),
        (
            vec!["yams", "--write", "-z\u{1b}[2J"],
            "unrecognized arguments: -z\u{1b}[2J",
        ),
    ] {
        let done = completion(&args);
        let body: serde_json::Value = serde_json::from_str(done.stdout.trim()).unwrap();
        assert_eq!(body["error"], expected, "{args:?}");
        assert!(!done.stdout.contains('\u{1b}'));
        assert!(done.stdout.contains("\\u001b[2J"));
    }
}

#[test]
fn exact_project_options_and_positional_joining_are_preserved() {
    let parsed = request(&[
        "memory-search",
        "--project",
        "/fictional/copper-garden",
        "silver",
        "orchard",
    ]);
    assert_eq!(
        parsed.project,
        Some(PathBuf::from("/fictional/copper-garden"))
    );
    assert_eq!(parsed.query(), Some("silver orchard"));

    assert!(matches!(
        request(&["yams", "--projects"]).operation,
        DirectOperation::Projects
    ));
}

#[test]
fn ambiguous_unknown_and_missing_options_are_usage_errors() {
    let mut cases = vec![
        (
            vec!["yams", "--m", "0.2", "query"],
            "ambiguous option: --m could match --min-score, --max-gap".to_owned(),
        ),
        (
            vec!["yams", "--unknown", "query"],
            "unrecognized arguments: --unknown".to_owned(),
        ),
        (
            vec!["yams", "--unknown=value", "query"],
            "unrecognized arguments: --unknown=value".to_owned(),
        ),
        (vec!["yams", "--project"], "--project".to_owned()),
        (vec!["yams", "-k"], "-k".to_owned()),
    ];
    for prefix in ["--p", "--pr", "--pro", "--proj", "--proje", "--projec"] {
        cases.push((
            vec!["yams", prefix, "path", "query"],
            format!("ambiguous option: {prefix} could match --project, --projects"),
        ));
    }
    for (args, fragment) in cases {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert_eq!(done.stdout, "", "{args:?}");
        assert!(done.stderr.contains(&fragment), "{args:?}: {}", done.stderr);
        assert!(
            done.stderr.contains("Usage: yams"),
            "{args:?}: {}",
            done.stderr
        );
    }
}

#[test]
fn help_ambiguity_and_unknown_errors_follow_argparse_order() {
    for args in [
        vec!["yams", "--help", "--unknown"],
        vec!["yams", "--unknown", "--help"],
        vec!["yams", "--help", "--m"],
    ] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Ok, "{args:?}");
        assert!(done.stdout.contains("Usage: yams"), "{args:?}");
        assert_eq!(done.stderr, "", "{args:?}");
    }

    let ambiguous = completion(&["yams", "--m", "--help"]);
    assert_eq!(ambiguous.exit_code, ExitCode::Usage);
    assert_eq!(ambiguous.stdout, "");
    assert!(
        ambiguous
            .stderr
            .contains("ambiguous option: --m could match --min-score, --max-gap")
    );

    let interrupted = completion(&["yams", "a", "--unknown", "b"]);
    assert_eq!(interrupted.exit_code, ExitCode::Usage);
    assert_eq!(interrupted.stdout, "");
    assert!(
        interrupted
            .stderr
            .contains("unrecognized arguments: --unknown b"),
        "{}",
        interrupted.stderr
    );
}

#[test]
fn missing_option_values_precede_later_help_ambiguity_and_unknowns() {
    for args in [
        vec!["yams", "--project", "--help"],
        vec!["yams", "--min-score", "--m", "--help"],
        vec!["yams", "--unknown", "-k", "--help"],
    ] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert_eq!(done.stdout, "", "{args:?}");
        assert!(
            done.stderr.contains("a value is required for"),
            "{args:?}: {}",
            done.stderr
        );
        assert!(done.stderr.contains("none was supplied"), "{args:?}");
        assert!(!done.stderr.contains("Usage: yams\n"), "{args:?}");
    }
}

#[test]
fn earlier_parser_errors_precede_later_ambiguity() {
    for (args, expected) in [
        (
            vec!["yams", "-k", "foo", "--m", "--help"],
            "invalid int value: 'foo'",
        ),
        (
            vec!["yams", "--min-score=-foo", "--m"],
            "invalid float value: '-foo'",
        ),
        (
            vec!["yams", "--json=x", "--m"],
            "unexpected value 'x' for '--json'",
        ),
    ] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert!(done.stderr.contains(expected), "{args:?}: {}", done.stderr);
        assert!(!done.stderr.contains("ambiguous option"), "{args:?}");
    }
}

#[test]
fn clustered_short_help_follows_argparse_order() {
    for args in [
        vec!["yams", "-hk", "--help"],
        vec!["yams", "-h=foo", "--help"],
    ] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert_eq!(done.stdout, "", "{args:?}");
    }

    for args in [vec!["yams", "-hk2"], vec!["yams", "-hfoo", "--m"]] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Ok, "{args:?}");
        assert!(done.stdout.contains("Usage: yams"), "{args:?}");
        assert_eq!(done.stderr, "", "{args:?}");
    }
}

#[test]
fn write_unknown_aggregation_remains_sanitized_machine_json() {
    let hostile = "--unknown=\u{1b}[2J\nforged";
    let done = completion(&["yams", "--write", "a", hostile, "b"]);
    assert_eq!(done.exit_code, ExitCode::Usage);
    assert_eq!(done.stderr, "");
    let body: serde_json::Value = serde_json::from_str(done.stdout.trim()).unwrap();
    assert_eq!(
        body["error"],
        format!("unrecognized arguments: {hostile} b")
    );
    assert!(!done.stdout.contains('\u{1b}'));
    assert!(done.stdout.contains("\\u001b[2J\\nforged"));
}

#[test]
fn help_always_uses_yams_even_for_the_compatibility_binary() {
    for program in ["yams", "memory-search", "/tmp/renamed-debug-binary"] {
        let done = completion(&[program, "-h"]);
        assert_eq!(done.exit_code, ExitCode::Ok);
        assert!(done.stdout.contains("Usage: yams"), "{}", done.stdout);
        assert!(!done.stdout.contains("Usage: memory-search"));
        assert_eq!(done.stderr, "");
    }
}

#[test]
fn semantic_numbers_are_validated_before_modes() {
    let cases = [
        (
            vec!["yams", "--min-score", "1.5", "query"],
            "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got 1.5",
        ),
        (
            vec!["yams", "--min-score", "nan", "query"],
            "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got nan",
        ),
        (
            vec!["yams", "--min-score", "inf", "query"],
            "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got inf",
        ),
        (
            vec!["yams", "--max-gap", "-0.1", "query"],
            "--max-gap must be 0.0 or greater; got -0.1",
        ),
        (
            vec!["yams", "--max-gap", "nan", "query"],
            "--max-gap must be 0.0 or greater; got nan",
        ),
        (
            vec!["yams", "--max-gap", "inf", "query"],
            "--max-gap must be 0.0 or greater; got inf",
        ),
        (
            vec!["yams", "-k", "0", "query"],
            "-k must be 1 or greater; got 0",
        ),
    ];
    for (args, expected) in cases {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert_eq!(done.stdout, "");
        assert_eq!(done.stderr, format!("{expected}\n"), "{args:?}");
    }
}

#[test]
fn semantic_float_diagnostics_use_python_repr() {
    for (args, expected) in [
        (
            vec!["yams", "--min-score", "2", "query"],
            "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got 2.0",
        ),
        (
            vec!["yams", "--min-score", "1e20", "query"],
            "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got 1e+20",
        ),
        (
            vec!["yams", "--max-gap=-1e-7", "query"],
            "--max-gap must be 0.0 or greater; got -1e-07",
        ),
        (
            vec!["yams", "--max-gap=-inf", "query"],
            "--max-gap must be 0.0 or greater; got -inf",
        ),
    ] {
        assert_eq!(completion(&args).stderr, format!("{expected}\n"));
    }

    let negative_zero = request(&["yams", "--max-gap=-0.0", "query"])
        .max_gap
        .unwrap();
    assert_eq!(negative_zero, 0.0);
    assert!(negative_zero.is_sign_negative());

    let write = completion(&["yams", "--write", "--min-score=2"]);
    let body: serde_json::Value = serde_json::from_str(write.stdout.trim()).unwrap();
    assert_eq!(
        body["error"],
        "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got 2.0"
    );

    let tie = "--min-score=-1000000000000000.2";
    let expected = "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got -1000000000000000.2";
    assert_eq!(
        completion(&["yams", tie, "query"]).stderr,
        format!("{expected}\n")
    );
    let write = completion(&["yams", "--write", tie]);
    let body: serde_json::Value = serde_json::from_str(write.stdout.trim()).unwrap();
    assert_eq!(body["error"], expected);
}

#[test]
fn frozen_gate_combination_diagnostics_are_exact() {
    let cases = [
        (
            vec!["yams", "--no-gate", "--min-score", "0.5", "query"],
            "--min-score/--max-gap do nothing with --no-gate: the gate does not run, so they can neither filter nor annotate. Add --explain to see what they would have done, or drop --no-gate to apply them.",
        ),
        (
            vec!["yams", "--all", "--explain", "query"],
            "--explain covers one project at a time: every project has its own gate baseline, so one verdict cannot describe them. Drop --all — with --project PATH to pick a different one.",
        ),
    ];
    for (args, expected) in cases {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage);
        assert_eq!(done.stderr, format!("{expected}\n"));
    }
}

#[test]
fn exactly_one_operation_is_selected() {
    for (args, expected) in [
        (&["yams", "query"][..], DirectOperation::Search),
        (&["yams", "--", "index"][..], DirectOperation::Search),
        (&["yams", "--all", "query"][..], DirectOperation::All),
        (&["yams", "--write"][..], DirectOperation::Write),
        (&["yams", "--index"][..], DirectOperation::Index),
        (&["yams", "index"][..], DirectOperation::Index),
        (&["yams", "--stats"][..], DirectOperation::Stats),
        (&["yams", "--projects"][..], DirectOperation::Projects),
        (&["yams", "--gc"][..], DirectOperation::Gc),
    ] {
        assert_eq!(request(args).operation, expected, "{args:?}");
    }

    let done = completion(&["yams", "--stats", "--gc"]);
    assert_eq!(
        done.stderr,
        "choose exactly one operation; cannot combine --stats, --gc\n"
    );

    assert!(request(&["yams", "--stats", "--json"]).json);
    assert!(
        request(&["yams", "--all", "--min-score", "0.4", "query"])
            .min_score
            .is_some()
    );
}

#[test]
fn write_refusals_are_compact_json_on_stdout() {
    for (args, error, hint) in [
        (
            vec!["yams", "--write", "--stats"],
            "--write cannot be combined with --stats",
            "run the write on its own",
        ),
        (
            vec!["yams", "--write", "--unknown"],
            "unrecognized arguments: --unknown",
            "fix the invocation and retry",
        ),
        (
            vec!["yams", "--write", "--min-score", "5"],
            "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got 5.0",
            "fix the invocation and retry",
        ),
    ] {
        let done = completion(&args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        assert_eq!(done.stderr, "", "{args:?}");
        let value: serde_json::Value = serde_json::from_str(done.stdout.trim()).unwrap();
        assert_eq!(
            value,
            serde_json::json!({"ok": false, "exit": 2, "error": error, "hint": hint}),
            "{args:?}"
        );
        assert!(!done.stdout.contains('\n') || done.stdout.ends_with('\n'));
    }
}

#[test]
fn write_detection_stops_at_the_option_terminator() {
    let done = completion(&["yams", "--stats", "--", "--write"]);
    assert_eq!(done.exit_code, ExitCode::Usage);
    assert_eq!(done.stdout, "");
    assert_eq!(done.stderr, "--stats does not accept query text\n");
}

#[test]
fn operation_modifier_matrix_refuses_no_effect_arguments() {
    let cases: &[(&[&str], &str)] = &[
        (
            &["yams", "--all", "--project", "/tmp/p", "query"],
            "--project is not valid with --all",
        ),
        (
            &["yams", "--projects", "--project", "/tmp/p"],
            "--project is not valid with --projects",
        ),
        (
            &["yams", "--gc", "--project", "/tmp/p"],
            "--project is not valid with --gc",
        ),
        (
            &["yams", "--stats", "trailing"],
            "--stats does not accept query text",
        ),
        (
            &["yams", "--write", "trailing"],
            "--write does not accept query text",
        ),
        (
            &["yams", "--gc", "--full"],
            "--full is only valid with search",
        ),
        (
            &["yams", "--stats", "-k", "3"],
            "-k is only valid with search",
        ),
        (
            &["yams", "--write", "--json"],
            "--json has no effect with --write",
        ),
        (
            &["yams", "--index", "--no-gate"],
            "--no-gate is only valid with search",
        ),
    ];
    for (args, expected) in cases {
        let done = completion(args);
        assert_eq!(done.exit_code, ExitCode::Usage, "{args:?}");
        if args.contains(&"--write") {
            let value: serde_json::Value = serde_json::from_str(done.stdout.trim()).unwrap();
            assert_eq!(value["error"], *expected, "{args:?}");
        } else {
            assert_eq!(done.stderr, format!("{expected}\n"), "{args:?}");
        }
    }
}

#[test]
fn missing_and_blank_queries_are_distinct_usage_completions() {
    let bare = completion(&["yams"]);
    assert_eq!(bare.exit_code, ExitCode::Usage);
    assert!(bare.stdout.contains("Usage: yams"));
    assert_eq!(bare.stderr, "");

    let blank = completion(&["yams", "  ", "\t"]);
    assert_eq!(blank.stderr, "empty query: nothing to search for\n");
    let all_blank = completion(&["yams", "--all", "  "]);
    assert_eq!(all_blank.stderr, "--all needs a query\n");
}

#[test]
fn blank_queries_use_python_string_strip_whitespace() {
    for separator in ['\u{001c}', '\u{001d}', '\u{001e}', '\u{001f}'] {
        let query = separator.to_string();
        let done = completion(&["yams", &query]);
        assert_eq!(
            done.stderr,
            "empty query: nothing to search for\n",
            "U+{:04X}",
            u32::from(separator)
        );
    }
}

#[test]
fn parser_only_produces_a_typed_request_without_dispatching() {
    let parsed = request(&[
        "yams",
        "--json",
        "--full",
        "--no-gate",
        "--explain",
        "--min-score",
        "-0.25",
        "--max-gap",
        "0.4",
        "-k",
        "9",
        "fictional",
        "query",
    ]);
    assert!(parsed.json);
    assert!(parsed.full);
    assert!(parsed.no_gate);
    assert!(parsed.explain);
    assert_eq!(parsed.min_score, Some(-0.25));
    assert_eq!(parsed.max_gap, Some(0.4));
    assert_eq!(parsed.k, 9);
    assert_eq!(parsed.query(), Some("fictional query"));
}
