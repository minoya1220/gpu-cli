//! Port of tests/test_instances.py. No test here reaches the live Vast
//! account, SSH, or Cursor.

use std::sync::{Arc, Mutex};

use crate::commands::edit_instance;
use crate::config::defaults;
use crate::instances::{instance_label, repo_name};
use crate::launch::{launch, UpArgs};
use crate::picker::testing::*;
use crate::picker::{Next, Opt, Outcome, Screen};
use crate::process::{Cmd, Output, RunError};
use crate::pyfmt::py_str;
use crate::pyjson::{self, Dict, Value};
use crate::repos::Repo;
use crate::ssh_config::{remove_host, update_host};
use crate::testing::*;

pub const INSTANCES: &str = r#"[
    {"id": 12, "gpu_name": "RTX 4090", "actual_status": "running", "dph_total": 0.5,
     "duration": 7342.81, "label": "ThunderKittens", "ssh_host": "ssh6.vast.ai", "ssh_port": 12345},
    {"id": 34, "gpu_name": "H100", "actual_status": "loading", "dph_total": 1.2,
     "duration": 60.5, "label": "no-repo", "ssh_host": "ssh7.vast.ai", "ssh_port": 2222}
]"#;

pub fn instances() -> Vec<Dict> {
    dicts(INSTANCES)
}

/// A stateful fake `vastai`: `show` lists the live instances, `destroy`
/// removes one, `stop` marks it stopped. `other` answers everything else.
pub fn fake_vast(
    live: Vec<Dict>,
    other: impl Fn(&Cmd) -> Result<Output, RunError> + Send + Sync + 'static,
) -> Arc<FakeRunner> {
    let live = Arc::new(Mutex::new(live));
    FakeRunner::new(move |cmd| {
        let argv = strs(&cmd.argv);
        if argv[0] != "vastai" {
            return other(cmd);
        }
        let mut live = live.lock().unwrap();
        if argv[..3] == ["vastai", "show", "instances"] {
            let list = Value::List(live.iter().cloned().map(Value::Dict).collect());
            return ok(&pyjson::dumps(&list, None));
        }
        let id_matches = |item: &Dict| py_str(item.get("id").unwrap()) == argv[3];
        match argv[1] {
            "destroy" => live.retain(|item| !id_matches(item)),
            "stop" => {
                for item in live.iter_mut().filter(|i| id_matches(i)) {
                    item.insert("actual_status", Value::str("stopped"));
                }
            }
            _ => {}
        }
        ok("ok")
    })
}

pub fn no_other(cmd: &Cmd) -> Result<Output, RunError> {
    panic!("unexpected command: {:?}", cmd.argv)
}

/// The SSH readiness probe succeeds and any GUI editor CLI accepts.
pub fn ssh_ready(cmd: &Cmd) -> Result<Output, RunError> {
    match strs(&cmd.argv)[..] {
        ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=5", _, "true"] => {
            Ok(Output::status(0))
        }
        ["cursor" | "code" | "zed", ..] => Ok(Output::status(0)),
        _ => panic!("unexpected command: {:?}", cmd.argv),
    }
}

/// GUI editor CLIs on PATH (the Python tests patch `shutil.which`).
pub fn editor_stubs(t: &TestEnv) {
    for name in ["cursor", "code", "zed", "rsync"] {
        t.executable(name, "#!/bin/sh\nexit 0\n");
    }
}

fn running(item: &Dict) -> Dict {
    let mut item = item.clone();
    item.insert("actual_status", Value::str("running"));
    item
}

#[test]
fn test_picker_zero_one_many() {
    for count in [0usize, 1, 2] {
        let t = TestEnv::new();
        let runner = fake_vast(instances()[..count].to_vec(), no_other);
        // Select the last instance, as the Python test's patched picker does.
        let keys = format!("{}{RIGHT}", DOWN.repeat(count.saturating_sub(1)));
        let pickers: Vec<&str> = if count == 0 {
            vec![]
        } else {
            vec![keys.as_str()]
        };
        let ctx = t.ctx().runner(&runner).pickers(&pickers).build();
        assert_eq!(main(&ctx, &["stop"]), 0, "{}", err(&ctx));
        let calls = runner.argvs();
        if count == 0 {
            assert!(out(&ctx).contains("No instances"));
            assert_eq!(calls.len(), 1);
        } else {
            let label = instance_label(&instances()[0]).unwrap();
            assert!(label.contains("RTX 4090"));
            assert!(label.contains("running"));
            assert!(label.contains("$0.500/hr"));
            assert!(label.contains("2h 02m"));
            assert!(!label.contains("7342"));
            assert!(label.contains("ThunderKittens"));
            let id = py_str(instances()[count - 1].get("id").unwrap());
            assert_eq!(
                calls[calls.len() - 2],
                ["vastai", "stop", "instance", id.as_str()]
            );
            assert_eq!(ctx.ui.remaining(), 0);
        }
    }
}

fn instance_screen(count: usize) -> Screen<String, String> {
    let options: Vec<Opt<String>> = instances()[..count]
        .iter()
        .map(|item| {
            Opt::new(
                py_str(item.get("id").unwrap()),
                instance_label(item).unwrap(),
            )
        })
        .collect();
    Screen::new("Instances", move || options.clone(), Next::Done)
}

#[test]
fn test_instance_picker_real_keys() {
    for count in [1usize, 2] {
        let cancelled = drive(instance_screen(count), move |keys| {
            keys.wait(|s| s.labels.len() == count);
            keys.press(BACK);
            keys.press(BACK);
        });
        assert!(matches!(cancelled, Outcome::Cancelled(_)));
        let selected = drive(instance_screen(count), move |keys| {
            keys.wait(|s| s.labels.len() == count);
            keys.press(RIGHT);
        });
        assert_eq!(selected, Outcome::Done("12".to_string()));
    }
}

#[test]
fn test_explicit_id_skips_picker() {
    let t = TestEnv::new();
    let runner = fake_vast(instances(), no_other);
    // No scripted pickers: opening one fails the test.
    for argv in [["ssh", "12"], ["stop", "12"]] {
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(main(&ctx, &argv), 0, "{}", err(&ctx));
    }
    assert_eq!(
        runner.argvs(),
        [
            vec!["vastai", "ssh-url", "12"],
            vec!["vastai", "show", "instances", "--raw"],
            vec!["vastai", "stop", "instance", "12"],
            vec!["vastai", "show", "instances", "--raw"],
        ]
    );
}

#[test]
fn test_down_selection_still_confirms_and_cancel_is_safe() {
    let t = TestEnv::new();
    let runner = fake_vast(instances(), no_other);
    let ctx = t
        .ctx()
        .runner(&runner)
        .pickers(&[RIGHT])
        .input(&["no"])
        .build();
    assert_eq!(main(&ctx, &["down"]), 0);
    assert_eq!(runner.calls().len(), 1);
    assert!(!t.home.join(".ssh").exists());
    let managed = t.home.join(".ssh").join("vst_config");
    update_host(&t.env(), &instances()[0]).unwrap();
    update_host(&t.env(), &instances()[1]).unwrap();
    let ctx = t
        .ctx()
        .runner(&runner)
        .pickers(&[RIGHT])
        .input(&["yes"])
        .build();
    assert_eq!(main(&ctx, &["down"]), 0, "{}", err(&ctx));
    let calls = runner.argvs();
    assert_eq!(
        calls[calls.len() - 2],
        ["vastai", "destroy", "instance", "12", "--yes"]
    );
    assert!(!read(&managed).contains("Host vast-12\n"));
    assert!(read(&managed).contains("Host vast-34\n"));
    let ctx = t
        .ctx()
        .runner(&runner)
        .pickers(&[&format!("{BACK}{BACK}")])
        .build();
    assert_eq!(main(&ctx, &["down"]), 1);
    assert_eq!(runner.calls().len(), 6);
}

#[test]
fn test_cursor_repo_and_fallback() {
    let t = TestEnv::new();
    editor_stubs(&t);
    let runner = fake_vast(
        vec![instances()[0].clone(), running(&instances()[1])],
        ssh_ready,
    );
    let mut stdout = String::new();
    for (instance_id, path) in [("12", "/workspace/ThunderKittens"), ("34", "/workspace")] {
        let ctx = t.ctx().runner(&runner).build();
        assert_eq!(main(&ctx, &["cursor", instance_id]), 0, "{}", err(&ctx));
        stdout += &out(&ctx);
        let alias = format!("ssh-remote+vast-{instance_id}");
        assert_eq!(
            runner.argvs().last().unwrap(),
            &["cursor", "--remote", alias.as_str(), path]
        );
    }
    assert!(stdout.contains("No repository recorded"));
}

#[test]
fn test_managed_ssh_config_preserves_user_bytes() {
    let t = TestEnv::new();
    let env = t.env();
    let ssh = t.home.join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    let original =
        b"# user header\r\nHost *\r\n  User alice\r\nHost personal\r\n  HostName example.com\r\n";
    std::fs::write(ssh.join("config"), original).unwrap();
    assert_eq!(update_host(&env, &instances()[0]).unwrap(), "vast-12");
    assert_eq!(update_host(&env, &instances()[0]).unwrap(), "vast-12");
    assert_eq!(update_host(&env, &instances()[1]).unwrap(), "vast-34");
    let mut expected = b"Include ~/.ssh/vst_config\n".to_vec();
    expected.extend_from_slice(original);
    assert_eq!(std::fs::read(ssh.join("config")).unwrap(), expected);
    let managed = ssh.join("vst_config");
    assert_eq!(read(&managed).matches("Host vast-12\n").count(), 1);
    assert!(read(&managed).contains("Host vast-34\n"));
    remove_host(&env, "12").unwrap();
    assert!(!read(&managed).contains("Host vast-12\n"));
    assert!(read(&managed).contains("Host vast-34\n"));
    assert_eq!(std::fs::read(ssh.join("config")).unwrap(), expected);
    assert!(!read(&managed).contains("token"));
}

#[test]
fn test_managed_ssh_config_with_crlf_replaces_stale_alias() {
    let t = TestEnv::new();
    let env = t.env();
    let ssh = t.home.join(".ssh");
    std::fs::create_dir(&ssh).unwrap();
    let managed = ssh.join("vst_config");
    std::fs::write(
        &managed,
        "Host vast-12\r\n  HostName old.example\r\n  Port 1\r\nHost vast-34\r\n  HostName ssh7.vast.ai\r\n  Port 2222\r\n",
    )
    .unwrap();
    let entry = "Host vast-12\n  HostName ssh6.vast.ai\n  Port 12345\n  User root\n  IdentityFile ~/.ssh/id_ed25519\n  StrictHostKeyChecking accept-new\n";
    assert_eq!(update_host(&env, &instances()[0]).unwrap(), "vast-12");
    assert_eq!(
        read(&managed),
        format!("Host vast-34\n  HostName ssh7.vast.ai\n  Port 2222\n{entry}")
    );
    remove_host(&env, "34").unwrap();
    assert_eq!(read(&managed), entry);
}

#[test]
fn test_ssh_wait_timeout() {
    let t = TestEnv::new();
    editor_stubs(&t);
    let mut loading = instances()[0].clone();
    loading.insert("actual_status", Value::str("loading"));
    let runner = fake_vast(vec![loading], no_other);
    let ctx = t.ctx().runner(&runner).build();
    let error = edit_instance(&ctx, "12", Some("cursor"), None, 4.0).unwrap_err();
    assert!(error.text().contains("Timed out waiting"));
    assert!(err(&ctx).contains("Waiting for instance"));
}

#[test]
fn test_ssh_probe_failure_times_out() {
    let t = TestEnv::new();
    editor_stubs(&t);
    let runner = fake_vast(vec![instances()[0].clone()], |cmd| match cmd.program() {
        "ssh" => Ok(Output::status(255)),
        _ => panic!("launched before SSH: {:?}", cmd.argv),
    });
    let ctx = t.ctx().runner(&runner).build();
    let error = edit_instance(&ctx, "12", Some("cursor"), None, 4.0).unwrap_err();
    assert!(error.text().contains("Timed out waiting"));
}

#[test]
fn test_onstart_repo_and_untrusted_label() {
    let item = dict(
        r#"{"label": "../bad", "onstart": "git clone https://github.com/o/good /workspace/good\n"}"#,
    );
    assert_eq!(repo_name(&item).as_deref(), Some("good"));
    assert_eq!(repo_name(&dict(r#"{"label": "no-repo"}"#)), None);
}

#[test]
fn test_up_offers_editor_on_success() {
    for (editor, keys) in [
        ("code", format!("{RIGHT}{DOWN}{RIGHT}")),
        ("none", format!("{DOWN}{RIGHT}")),
    ] {
        let t = TestEnv::new();
        editor_stubs(&t);
        let instance = dict(
            r#"{"id": 12, "actual_status": "running", "label": "ThunderKittens", "ssh_host": "ssh6.vast.ai", "ssh_port": 12345}"#,
        );
        let runner = FakeRunner::new(move |cmd| match strs(&cmd.argv)[..] {
            ["vastai", "search", "offers", ..] => {
                ok(r#"[{"id": 9, "dph_total": 1, "gpu_name": "H100", "num_gpus": 1}]"#)
            }
            ["vastai", "create", "instance", ..] => ok(r#"{"success": true, "new_contract": 12}"#),
            ["vastai", "show", "instances", "--raw"] => ok(&pyjson::dumps(
                &Value::List(vec![Value::Dict(instance.clone())]),
                None,
            )),
            _ => ssh_ready(cmd),
        });
        let ctx = t
            .ctx()
            .runner(&runner)
            .interactive(true)
            .pickers(&[&keys])
            .input(&["yes"])
            .build();
        let args = UpArgs {
            repo: Some(Repo::from_arg("ThunderKittens")),
            mode: Some("container".into()),
            gpu: Some("H100".into()),
            num_gpus: 1,
            offer: Some(9),
            ..UpArgs::default()
        };
        launch(&ctx, &args, &defaults(), None).unwrap();
        let opened: Vec<Vec<String>> = runner
            .argvs()
            .into_iter()
            .filter(|a| a[0] == "code")
            .collect();
        if editor == "none" {
            assert!(opened.is_empty());
        } else {
            assert_eq!(
                opened,
                [[
                    "code",
                    "--remote",
                    "ssh-remote+vast-12",
                    "/workspace/ThunderKittens"
                ]]
            );
        }
        assert_eq!(ctx.ui.remaining(), 0);
    }
}
