use std::fs;

use crate::cli::{self, Command, Parsed, RunFlags};
use crate::error::ExitCode;
use crate::modal::*;
use crate::process::{Output, RunError};
use crate::pyjson::{self, Value};
use crate::run::{RunBackend, RunPlan};
use crate::testing::{dict, FakeRunner, TestEnv};

const RATES: &str = r#"{"gpu_hour_cost_t4":"0.59","gpu_hour_cost_h100":"3.95","cpu_hour_cost_sandbox":"0.1419","mem_gib_hour_cost_sandbox":"0.024"}"#;
fn flags(extra: &[&str]) -> RunFlags {
    let mut args = vec![
        "gpu",
        "run",
        "--provider",
        "modal",
        "--gpu",
        "T4",
        "--expire",
        "30m",
        "--dry-run",
    ];
    args.extend(extra);
    args.extend(["--", "true"]);
    let args = args.into_iter().map(String::from).collect::<Vec<_>>();
    match cli::parse(&args) {
        Parsed::Run(command) => match *command {
            Command::Run(f) => f,
            _ => panic!(),
        },
        _ => panic!("invalid flags"),
    }
}
fn rates_of(data: &str) -> Rates {
    Rates {
        prices: dict(data),
        diagnostic: String::new(),
    }
}
fn fake_rates() -> Rates {
    rates_of(RATES)
}

#[test]
fn gpu_validation_and_total_price() {
    assert_eq!(gpu("a100 80gb", 8).unwrap().0, "A100-80GB");
    assert_eq!(gpu("h100!", 1).unwrap().1, "h100");
    for (name, count) in [
        ("RTX 4090", 1),
        ("T4", 0),
        ("T4", 9),
        ("A10", 5),
        ("RTX-PRO-6000", 2),
    ] {
        assert_eq!(gpu(name, count).unwrap_err().exit_code(), 2);
    }
    assert!((fake_rates().price("T4", 2, 4.0, 16.0).unwrap() - 2.1316).abs() < 1e-9);
    for bad in ["null", "-1", "\"NaN\"", "\"inf\""] {
        let data = RATES.replace("\"0.59\"", bad);
        assert!(rates_of(&data).price("T4", 1, 4.0, 16.0).is_err());
    }
    assert!(rates_of("{}").price("T4", 1, 4.0, 16.0).is_err());
}

#[test]
fn modal_usage_errors_make_no_dependency_calls() {
    let t = TestEnv::new();
    let cwd = t.root.join("source");
    fs::create_dir(&cwd).unwrap();
    let ctx = t.ctx().build();
    for options in [
        vec!["--offer", "1"],
        vec!["--disk", "50"],
        vec!["--reliability", "0.9"],
        vec!["--unverified"],
        vec!["--cpu", "0"],
        vec!["--memory", "NaN"],
        vec!["--memory", "0.00001"],
        vec!["--expire", "25h"],
    ] {
        // Replace rather than repeat --expire for the lifetime case.
        let f = if options[0] == "--expire" {
            let mut f = flags(&[]);
            f.expire = Some("25h".into());
            f
        } else {
            flags(&options)
        };
        assert_eq!(
            crate::run::run(&ctx, &f, &crate::config::defaults(), &cwd)
                .unwrap_err()
                .exit_code(),
            2,
            "{options:?}"
        );
    }
}

#[test]
fn plan_caps_resources_and_exposes_only_env_names() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    let mut f = flags(&[
        "--cpu",
        "2",
        "--memory",
        "8",
        "--num-gpus",
        "2",
        "--env",
        "TOKEN",
    ]);
    let deadline = ctx.clock.now() + jiff::Span::new().minutes(30);
    let backend = ModalBackend::new(Some(fake_rates()));
    let RunPlan { request, rate, .. } = backend
        .plan(
            &ctx,
            &f,
            &crate::config::defaults(),
            deadline,
            "repo".into(),
        )
        .unwrap();
    assert!((rate - 1.6558).abs() < 1e-9);
    let request = request.unwrap();
    let request = request.as_dict().unwrap();
    assert_eq!(request.get("gpu"), Some(&Value::str("T4:2")));
    assert_eq!(request.get("memory"), Some(&Value::Int(8192)));
    assert_eq!(request.get("timeout"), Some(&Value::Int(1800)));
    assert_eq!(
        request.get("env_names"),
        Some(&Value::List(vec![Value::str("TOKEN")]))
    );
    f.max_price = Some(1.0);
    assert_eq!(
        backend
            .plan(
                &ctx,
                &f,
                &crate::config::defaults(),
                deadline,
                "repo".into()
            )
            .unwrap_err()
            .exit_code(),
        11
    );
}

#[test]
fn helper_typed_failures_and_timeouts_are_redacted() {
    for (kind, code) in [
        ("MissingToken", 12),
        ("AuthError", 13),
        ("ResourceExhaustedError", 10),
        ("SchedulingTimeout", 10),
        ("NotFoundError", 16),
        ("ImageBuildError", 1),
        ("VersionError", 1),
        ("InstanceLost", 18),
        ("CleanupUnverified", 19),
        ("CommandTimeout", 20),
    ] {
        let kind = kind.to_string();
        let runner = FakeRunner::new(move |cmd| {
            assert_eq!(cmd.program(), "uv");
            assert_eq!(&cmd.argv[1..4], ["run", "--quiet", "--script"]);
            assert!(cmd.timeout.is_some());
            Ok(Output {
                code: 1,
                stdout: format!(r#"{{"error":{{"kind":"{kind}","message":"canary-value"}}}}"#),
                stderr: "canary-value".into(),
            })
        });
        let t = TestEnv::new();
        let ctx = t.ctx().runner(&runner).build();
        ctx.protect_secret("canary-value");
        let error = rates(&ctx).err().unwrap();
        assert_eq!(error.exit_code(), code);
        assert!(!error.text().contains("canary-value"));
    }
    for (error, code) in [(RunError::NotFound, 12), (RunError::TimedOut, 20)] {
        let runner = FakeRunner::new(move |_| Err(error.clone()));
        let t = TestEnv::new();
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(rates(&ctx).err().unwrap().exit_code(), code);
    }
    assert_eq!(ExitCode::CommandTimeout as i32, 20);
}

#[test]
fn readiness_reports_workspace_without_mutation() {
    let runner = FakeRunner::new(|cmd| {
        assert_eq!(cmd.argv.last().unwrap(), "check");
        Ok(Output::ok(
            r#"{"workspace":"example","environment":"dev","client_version":"1.6.3"}"#,
        ))
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build();
    check(&ctx).unwrap();
    assert_eq!(runner.calls().len(), 1);
    assert!(pyjson::loads(RATES).is_ok());
}

#[test]
fn modal_gpu_rows_show_live_total_and_skip_vast_offer_screen() {
    use crate::picker::{
        testing::{drive, RIGHT},
        Job, Next, Outcome,
    };
    use crate::wizard::{LaunchWizard, Picks, WizardOptions};
    let t = TestEnv::new();
    let flow = LaunchWizard::new(
        &t.env(),
        WizardOptions {
            repos: Job::new(|| Ok(Vec::new())),
            search: None,
            models: vec!["RTX 4090".into()],
            recent_gpus: Vec::new(),
            choose_offer: true,
            tpu_setup_error: Some("not available".into()),
            tpu_transition: None,
            tpu_prices: None,
            tpu_cached_prices: None,
            tpu_types: Vec::new(),
            tpu_catalog: None,
            owner: String::new(),
        },
    )
    .with_modal(Some((Job::new(|| Ok(fake_rates())), 1, 4.0, 16.0)), false);
    let picks = Picks::new(Some(crate::repos::Repo::NoRepo), Some("container"), None);
    let Next::Screen(first) = flow.next(picks) else {
        panic!("missing GPU screen");
    };
    let outcome = drive(first, |keys| {
        keys.wait(|s| s.title == "GPU");
        keys.type_text("modal t4");
        keys.wait(|s| s.labels.iter().any(|l| l.contains("$1.542/hr")));
        assert_eq!(keys.labels().len(), 1);
        keys.press(RIGHT);
    });
    let Outcome::Done(picks) = outcome else {
        panic!("Modal selection did not finish");
    };
    assert_eq!(picks.modal.as_deref(), Some("T4"));
    assert!(picks.offer.is_none());
}

#[test]
fn helper_stderr_is_carried_not_printed_until_the_plan_runs() {
    use crate::testing::err;
    let runner = FakeRunner::new(|_| {
        Ok(Output {
            code: 0,
            stdout: RATES.into(),
            stderr: "helper warning canary-value".into(),
        })
    });
    let t = TestEnv::new();
    let ctx = t.ctx().runner(&runner).build();
    ctx.protect_secret("canary-value");
    let fetched = rates(&ctx).unwrap();
    assert_eq!(err(&ctx), "");
    let deadline = ctx.clock.now() + jiff::Span::new().minutes(30);
    ModalBackend::new(Some(fetched))
        .plan(
            &ctx,
            &flags(&[]),
            &crate::config::defaults(),
            deadline,
            "repo".into(),
        )
        .unwrap();
    assert!(err(&ctx).contains("helper warning"));
    assert!(!err(&ctx).contains("canary-value"));

    let runner = FakeRunner::new(|_| {
        Ok(Output {
            code: 1,
            stdout: String::new(),
            stderr: "error: failed to download modal==1.6.3".into(),
        })
    });
    let ctx = t.ctx().runner(&runner).build();
    let error = rates(&ctx).err().unwrap();
    assert_eq!(err(&ctx), "");
    assert!(error.text().contains("failed to download modal"));
}

#[test]
fn explicit_modal_provider_picker_lists_only_modal_rows_with_a_short_failure_reason() {
    use crate::picker::{
        testing::{drive, CTRL_C},
        Job, Next, Outcome,
    };
    use crate::wizard::{LaunchWizard, Picks, WizardOptions};
    let t = TestEnv::new();
    let flow = LaunchWizard::new(
        &t.env(),
        WizardOptions {
            repos: Job::new(|| Ok(Vec::new())),
            search: None,
            models: vec!["RTX 4090".into()],
            recent_gpus: Vec::new(),
            choose_offer: true,
            tpu_setup_error: Some("not available".into()),
            tpu_transition: None,
            tpu_prices: None,
            tpu_cached_prices: None,
            tpu_types: Vec::new(),
            tpu_catalog: None,
            owner: String::new(),
        },
    )
    .with_modal(
        Some((
            Job::new(|| Err("Install uv. Next: gpu setup --provider modal\ndetail".into())),
            1,
            4.0,
            16.0,
        )),
        true,
    );
    let picks = Picks::new(Some(crate::repos::Repo::NoRepo), Some("container"), None);
    let Next::Screen(first) = flow.next(picks) else {
        panic!("missing GPU screen");
    };
    let outcome = drive(first, |keys| {
        keys.wait(|s| s.title == "GPU" && s.body.iter().any(|l| l.contains("Install uv")));
        let snapshot = keys.snapshot();
        assert!(!snapshot.body.iter().any(|l| l.contains("detail")));
        assert_eq!(snapshot.labels.len(), GPUS.len());
        assert!(snapshot.labels.iter().all(|l| l.starts_with("Modal ")));
        keys.type_text("rtx 4090");
        keys.wait(|s| s.labels.is_empty());
        keys.press(CTRL_C);
        keys.press(CTRL_C);
    });
    assert!(matches!(outcome, Outcome::Cancelled(_)));
}
