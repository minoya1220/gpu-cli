use super::*;
use crate::testing::*;
#[test]
fn launch_jobs_refresh_even_new_caches_and_keep_offline_choices() {
    use crate::picker::{testing::*, JobState, Next, Opt, Outcome, Screen};
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };
    let env = TestEnv::new();
    let count = Arc::new(AtomicUsize::new(0));
    let shared = count.clone();
    let runner = crate::tests::gcp_transport::runner(move |url| {
        if url.contains("/locations?") {
            let n = shared.fetch_add(1, Ordering::SeqCst);
            if n >= 2 {
                return (503, "{}".into());
            }
            return (200, r#"{"locations":[{"locationId":"us-east5-b"}]}"#.into());
        }
        assert!(url.contains("/locations/us-east5-b/acceleratorTypes?"));
        (
            200,
            format!(
                r#"{{"acceleratorTypes":[{{"type":"v6e-{}"}}]}}"#,
                shared.load(Ordering::SeqCst)
            ),
        )
    });
    let ctx = env.ctx().runner(&runner).build().for_gcp_setup();
    let cfg = dict(
        r#"{"gcp":{"project":"test-project","setup":{"version":1,"project":"test-project"}}}"#,
    );
    for expected in ["v6e-1", "v6e-2", "v6e-2"] {
        let job = types_job(&ctx, &cfg);
        let rows = job.clone();
        let observer = job.observer();
        let screen = Screen::new(
            "types",
            move || {
                rows.result()
                    .unwrap_or_default()
                    .into_iter()
                    .map(|s| Opt::new(s.clone(), s))
                    .collect()
            },
            Next::Done,
        )
        .load(job);
        let result = drive(screen, move |keys| {
            keys.wait(|_| observer.state() == JobState::Done);
            keys.press(RIGHT);
        });
        assert_eq!(result, Outcome::Done(expected.to_string()));
    }
    assert_eq!(count.load(Ordering::SeqCst), 3);
}
#[test]
fn empty_locations_are_not_offered_and_type_filtering_uses_live_catalogs() {
    let env = TestEnv::new();
    let runner = crate::tests::gcp_transport::runner(|url| {
        if url.contains("/locations?") {
            return (200, r#"{"locations":[{"locationId":"us-west1-b"},{"locationId":"us-east5-b"},{"locationId":"us-central1-b"},{"locationId":"us-west1-c"}]}"#.into());
        }
        let rows = if url.contains("/locations/us-west1-b/") {
            "[]"
        } else if url.contains("/locations/us-west1-c/") {
            r#"[{"type":"v5litepod-1"},{"type":"v6e-1"}]"#
        } else if url.contains("/locations/us-central1-b/") {
            r#"[{"type":"v2-8"}]"#
        } else if url.contains("/locations/us-east5-b/") {
            r#"[{"type":"v6e-1"}]"#
        } else {
            panic!("unexpected catalog REST request: {url}")
        };
        (200, format!(r#"{{"acceleratorTypes":{rows}}}"#))
    });
    let ctx = env
        .ctx()
        .runner(&runner)
        .env(env.env().with("TZ", "America/Los_Angeles"))
        .build()
        .for_gcp_setup();
    let all = zones(&ctx, "test-project", None).unwrap();
    assert_eq!(
        all.iter().map(|z| z.name.as_str()).collect::<Vec<_>>(),
        ["us-west1-c", "us-central1-b", "us-east5-b"]
    );
    assert_eq!(
        zones(&ctx, "test-project", Some("v2-8")).unwrap()[0].name,
        "us-central1-b"
    );
    assert_eq!(
        zones(&ctx, "test-project", Some("v5e-1")).unwrap()[0].name,
        "us-west1-c"
    );
    let message = verify(&ctx, "test-project", "us-west1-b", "v2-8")
        .unwrap_err()
        .text()
        .to_string();
    assert!(message.contains("v2-8 unavailable in us-west1-b"));
    assert!(message.contains("us-central1-b"));
}
