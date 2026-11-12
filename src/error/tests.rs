use super::*;

#[test]
fn next_renders_the_hint_and_keeps_it_out_of_the_summary() {
    let error =
        Error::status(ExitCode::NotFound, "TPU was not found").next("gpu ls --provider gcp");
    assert_eq!(
        error.text(),
        "TPU was not found. Next: gpu ls --provider gcp"
    );
    assert_eq!(error.summary(), "TPU was not found");
    assert_eq!(error.next_step(), Some("gpu ls --provider gcp"));
    assert_eq!(error.exit_code(), ExitCode::NotFound as i32);
    assert!(matches!(error, Error::Status(ExitCode::NotFound, _)));
}

#[test]
fn a_later_next_step_replaces_the_earlier_one() {
    let error = Error::msg("Setup failed")
        .next("gpu ls")
        .next_line("gpu setup");
    assert_eq!(error.text(), "Setup failed\nNext: gpu setup");
    assert_eq!(error.summary(), "Setup failed");
    assert_eq!(error.next_step(), Some("gpu setup"));
}

#[test]
fn recovers_with_records_a_command_without_changing_the_text() {
    let error =
        Error::msg("Timer failed. Run gpu expire 7 show.").recovers_with("gpu expire 7 show");
    assert_eq!(error.text(), "Timer failed. Run gpu expire 7 show.");
    assert_eq!(error.summary(), error.text());
    assert_eq!(error.next_step(), Some("gpu expire 7 show"));
}

#[test]
fn with_text_keeps_a_stable_status_and_drops_other_kinds_to_handled_errors() {
    let status = Error::status(ExitCode::NoCapacity, "No capacity").rejected();
    let rewrapped = status.with_text("Create rejected");
    assert_eq!(
        rewrapped,
        Error::status(ExitCode::NoCapacity, "Create rejected")
    );
    assert!(!rewrapped.is_rejection());
    assert_eq!(
        Error::unavailable("Retry later").with_text("Listing failed"),
        Error::msg("Listing failed")
    );
}

#[test]
fn errors_without_a_message_ignore_hints() {
    let error = Error::Cancelled(130).next("gpu ls").rejected();
    assert_eq!(error, Error::Cancelled(130));
    assert_eq!(error.summary(), "");
    assert_eq!(error.next_step(), None);
}
