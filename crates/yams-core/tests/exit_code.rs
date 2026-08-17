use yams_core::ExitCode;

#[test]
fn exit_codes_match_the_documented_cli_contract() {
    assert_eq!(i32::from(ExitCode::Ok), 0);
    assert_eq!(i32::from(ExitCode::Empty), 1);
    assert_eq!(i32::from(ExitCode::Usage), 2);
    assert_eq!(i32::from(ExitCode::Unsure), 3);
    assert_eq!(i32::from(ExitCode::Operational), 4);
}
