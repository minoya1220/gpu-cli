use super::*;

fn gcloud(stderr: &str) -> Error {
    gcp(Ok(Output::failed(1, stderr)), "gpu ls --provider gcp").unwrap_err()
}

#[test]
fn gcloud_refusals_are_typed_rejections_with_a_separate_next_step() {
    let capacity = gcloud("RESOURCE_EXHAUSTED: no more capacity");
    assert!(matches!(capacity, Error::Status(ExitCode::NoCapacity, _)));
    assert!(capacity.is_rejection());
    assert_eq!(capacity.summary(), "No TPU capacity in this zone");
    assert_eq!(
        capacity.text(),
        "No TPU capacity in this zone. Next: gpu ls --provider gcp"
    );
    for (stderr, summary) in [
        ("Quota exceeded", "Google Cloud quota is exhausted"),
        (
            "PERMISSION_DENIED",
            "Google Cloud denied access. Check project IAM and enabled APIs",
        ),
        ("NOT_FOUND: node", "Google Cloud resource was not found"),
    ] {
        let error = gcloud(stderr);
        assert!(error.is_rejection(), "{stderr}");
        assert_eq!(error.summary(), summary);
        assert_eq!(error.next_step(), Some("gpu ls --provider gcp"));
    }
}

#[test]
fn ambiguous_gcloud_failures_are_not_rejections() {
    let unavailable = gcloud("internal error");
    assert!(unavailable.retryable());
    assert!(!unavailable.is_rejection());
    let auth = gcloud("Reauthentication required");
    assert!(matches!(auth, Error::Status(ExitCode::AuthExpired, _)));
    assert!(!auth.is_rejection());
    assert_eq!(auth.next_step(), Some("gcloud auth login"));
    let timeout = gcp(Err(RunError::TimedOut), "gpu ls").unwrap_err();
    assert!(matches!(timeout, Error::Timeout(_)) && !timeout.is_rejection());
}
