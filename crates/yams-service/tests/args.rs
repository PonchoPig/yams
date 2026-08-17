use std::ffi::OsString;
use std::time::Duration;

use yams_service::parse_service_args;

fn to_os(args: &[&str]) -> Vec<OsString> {
    args.iter().map(OsString::from).collect()
}

#[test]
fn idle_timeout_forms_share_validation_and_never_panic() {
    let ok = |args: &[&str], secs: u64| {
        assert_eq!(
            parse_service_args(to_os(args)).unwrap().1,
            Duration::from_secs(secs)
        )
    };
    let err = |args: &[&str], needle: &str| {
        let error = parse_service_args(to_os(args)).unwrap_err();
        assert!(error.contains(needle), "{error:?} missing {needle:?}");
    };
    ok(&["--socket", "/tmp/s", "--idle-timeout", "1200"], 1200);
    ok(&["--socket", "/tmp/s", "--idle-timeout=1200"], 1200);
    ok(&["--socket", "/tmp/s", "--idle-timeout=5"], 5);
    err(
        &["--socket", "/tmp/s", "--idle-timeout=0"],
        "greater than zero",
    );
    err(
        &["--socket", "/tmp/s", "--idle-timeout="],
        "requires a value",
    );
    err(
        &["--socket", "/tmp/s", "--idle-timeout=abc"],
        "nonnegative integer",
    );
    err(
        &["--socket", "/tmp/s", "--idle-timeout=99999999999999999999"],
        "nonnegative integer",
    );
    err(&["--idle-timeout", "0"], "greater than zero");
    err(
        &["--socket", "/tmp/s", "--idle-timeout", ""],
        "requires a value",
    );
}

#[test]
fn version_is_recognized_as_a_completion() {
    assert_eq!(
        parse_service_args(to_os(&["--version"])).unwrap_err(),
        "version"
    );
}

#[test]
fn socket_omission_is_distinct_from_an_explicit_option_without_a_value() {
    assert!(parse_service_args(to_os(&[])).is_ok());

    let error = parse_service_args(to_os(&["--socket"])).unwrap_err();
    assert!(error.contains("requires a value"), "{error:?}");
}
