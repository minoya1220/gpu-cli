//! Binary coverage with uv on PATH; no Modal Python environment or cloud calls.
mod common;
use common::Sandbox;
use gpu::pyjson::{self, Value};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

const UV: &str = r#"#!/bin/sh
set -eu
printf 'uv\t%s\n' "$5" >> "$FAKE_CLI_LOG"
[ "$1 $2 $3" = 'run --quiet --script' ] && [ -f "$4" ] || exit 97
case "$5" in
rates) echo '{"gpu_hour_cost_t4":"0.59","cpu_hour_cost_sandbox":"0.1419","mem_gib_hour_cost_sandbox":"0.024"}' ;;
create) cat >"$HOME/create-spec"; echo '{"id":"sb-offline"}' ;;
list) echo '{"sandboxes":[{"id":"sb-offline","tags":{"gpu-cli":"true","gpu":"T4","label":"source","run_id":"recovery"}}]}' ;;
status)
 if [ "${MODE:-}" = missing ]; then echo '{"error":{"kind":"NotFoundError","message":"gone"}}'; exit 1; fi
 if [ "${MODE:-}" = lost ] || [ -f "$HOME/destroyed" ]; then echo '{"running":false,"returncode":137}';
 else echo '{"running":true,"returncode":null}'; fi ;;
volume-delete) touch "$HOME/volume-deleted"; echo '{"finished":true}' ;;
volume-get)
 touch "$HOME/downloading"
 if [ -f "$HOME/slow-download" ]; then exec sleep 120; fi
 export COPYFILE_DISABLE=1
 exec tar --format=ustar -C "$HOME" -cf - outputs ;;
terminate)
 if [ -f "$HOME/slow-cleanup" ]; then echo $$ >"$HOME/cleanup-pid"; touch "$HOME/cleanup"; exec sleep 120; fi
 touch "$HOME/destroyed"; echo '{"finished":true}' ;;
exec)
 for remote do :; done
 case "$remote" in
 'mkdir -p '*) cat >/dev/null ;;
 'bash -s -- start '*) cat >"$HOME/payload"; echo started ;;
 *' stop '*) touch "$HOME/stopped" ;;
 'printf '*) exit 0 ;;
 *' status '*)
  touch "$HOME/following"
  if [ "${MODE:-}" = lost ]; then exit 255; fi
  if [ "${MODE:-}" = follow ] && [ ! -f "$HOME/stopped" ]; then printf 'job\t-\t0\t4\n';
  else printf 'job\t%s\t0\t4\n' "${JOB_EXIT:-0}"; fi ;;
 'dd '*) printf 'hi\r\n' ;;
 'tar -C '*)
  touch "$HOME/downloading"
  if [ -f "$HOME/slow-download" ]; then exec sleep 120; fi
  export COPYFILE_DISABLE=1
  exec tar --format=ustar -C "$HOME" -cf - outputs ;;
 *) exit 97 ;;
 esac ;;
*) exit 97 ;;
esac
"#;

fn sandbox() -> Sandbox {
    let s = Sandbox::new();
    s.stub("uv", UV);
    std::fs::create_dir(s.root.join("source")).unwrap();
    std::fs::write(s.root.join("source/job.py"), b"print('hi')").unwrap();
    std::fs::create_dir(s.home.join("outputs")).unwrap();
    std::fs::write(s.home.join("outputs/result"), b"ok\0\xff\r\n").unwrap();
    s
}
fn command(s: &Sandbox) -> std::process::Command {
    let mut c = s.command(&[
        "run",
        "--provider",
        "modal",
        "--gpu",
        "T4",
        "--expire",
        "30m",
        "--yes",
        "--json",
        "--env",
        "TOKEN",
        "--",
        "python",
        "job.py",
    ]);
    c.current_dir(s.root.join("source"))
        .env("TOKEN", "private-canary");
    c
}
fn result(out: &std::process::Output) -> Value {
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");
    pyjson::loads(&text).unwrap().get("result").unwrap().clone()
}

#[test]
fn live_success_failure_and_preemption() {
    for (mode, exit) in [("success", 0), ("failure", 7), ("lost", 18)] {
        let s = sandbox();
        let out = command(&s)
            .env("MODE", mode)
            .env("JOB_EXIT", if exit == 7 { "7" } else { "0" })
            .output()
            .unwrap();
        assert_eq!(
            out.status.code(),
            Some(exit),
            "{}\n{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        let result = result(&out);
        assert_eq!(result.get("provider"), Some(&Value::str("modal")));
        assert_eq!(result.get("id"), Some(&Value::str("modal:sb-offline")));
        assert_eq!(result.get("destroyed"), Some(&Value::Bool(true)));
        let local = s
            .root
            .join("source/gpu-runs")
            .join(result.get("run_id").unwrap().as_str().unwrap());
        let spec =
            pyjson::loads(&std::fs::read_to_string(s.home.join("create-spec")).unwrap()).unwrap();
        assert_eq!(
            spec.get("tags").unwrap().get("run_id"),
            result.get("run_id")
        );
        assert!(!pyjson::dumps(&spec, None).contains("private-canary"));
        assert_eq!(
            std::fs::read(local.join("outputs/result")).unwrap(),
            b"ok\0\xff\r\n"
        );
        assert!(s.home.join("volume-deleted").exists());
        if mode != "lost" {
            assert_eq!(std::fs::read(local.join("log.txt")).unwrap(), b"hi\r\n");
        }
        assert!(!String::from_utf8_lossy(&out.stdout).contains("private-canary"));
        assert!(!String::from_utf8_lossy(&out.stderr).contains("private-canary"));
        assert!(s.calls().iter().all(|call| call[0] == "uv"));
    }
}

#[test]
fn modal_ls_down_and_unsupported_commands_route_only_to_uv() {
    let s = sandbox();
    s.stub("vastai", "#!/bin/sh\necho '[]'\n");
    std::fs::write(s.home.join(".modal.toml"), "# auth presence hint only\n").unwrap();
    let out = s.command(&["ls", "--json", "--envelope"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        result(&out)
            .get("instances")
            .unwrap()
            .as_list()
            .unwrap()
            .len(),
        1
    );
    let out = s
        .command(&["ls", "--provider", "modal", "--json", "--envelope"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let rows = result(&out)
        .get("instances")
        .unwrap()
        .as_list()
        .unwrap()
        .to_vec();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].get("id"), Some(&Value::str("modal:sb-offline")));
    let out = s.command(&["ls", "--provider", "modal"]).output().unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(text.contains("modal:sb-offline") && text.contains("modal") && text.contains("T4"));
    let out = s
        .command(&["down", "modal:sb-offline", "--yes", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(result(&out).get("destroyed"), Some(&Value::Bool(true)));
    let out = s
        .command(&["down", "modal:sb-missing", "--yes", "--json"])
        .env("MODE", "missing")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(16));
    assert!(s.calls().iter().all(|call| call[0] == "uv"));
    for args in [
        vec!["stop", "modal:sb-offline"],
        vec!["wait", "modal:sb-offline"],
        vec!["expire", "modal:sb-offline", "30m"],
    ] {
        let out = s.command(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?} {out:?}");
    }
}

#[test]
fn unconfigured_modal_never_breaks_vast_inventory() {
    let s = sandbox();
    s.stub("vastai", "#!/bin/sh\necho '[]'\n");
    for envelope in [false, true] {
        let mut cmd = s.command(&["ls", "--json"]);
        if envelope {
            cmd.arg("--envelope");
        }
        assert!(cmd.output().unwrap().status.success());
    }
    assert!(s.calls().is_empty());
    // Config presence is only a hint: no token/runtime remains optional on an
    // unfiltered listing, but is an actionable error with --provider modal.
    std::fs::write(s.home.join(".modal.toml"), "# no credentials\n").unwrap();
    s.stub("uv", "#!/bin/sh\necho '{\"error\":{\"kind\":\"MissingToken\",\"message\":\"not configured\"}}'\nexit 1\n");
    let out = s.command(&["ls", "--json", "--envelope"]).output().unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = s
        .command(&["ls", "--provider", "modal", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(12));
}

#[test]
fn modal_exec_copy_and_ssh_public_envelopes_and_plain_bytes() {
    let s = sandbox();
    s.stub(
        "uv",
        r#"#!/bin/sh
set -eu
printf 'uv\t%s\n' "$5" >> "$FAKE_CLI_LOG"
case "$5" in
status) echo '{"running":true,"returncode":null}' ;;
exec)
 if [ -f "$HOME/timeout-exec" ]; then exec sleep 120; fi
 if [ "$7" = --copy ]; then exit 0; fi
 [ "$7" = -- ] && [ "$8" = printf ] && [ "$9" = '%s' ] || exit 97
 shift 9
 [ "$1" = 'a b;$(no)' ] || exit 97
 printf 'x\000\377\r\n'; printf 'remote stderr\r\n' >&2; exit 7 ;;
*) exit 97 ;;
esac
"#,
    );
    for json in [false, true] {
        let mut cmd = s.command(&["exec", "modal:sb-offline"]);
        if json {
            cmd.arg("--json");
        }
        let out = cmd
            .args(["--", "printf", "%s", "a b;$(no)"])
            .output()
            .unwrap();
        assert_eq!(out.status.code(), Some(7));
        if json {
            let fields = result(&out);
            assert_eq!(fields.get("exit_code"), Some(&Value::Int(7)));
            assert_eq!(fields.get("ready"), Some(&Value::Bool(true)));
            assert_eq!(fields.get("ssh_alias"), Some(&Value::Null));
            assert_eq!(fields.get("stdout"), Some(&Value::str("x\0\u{fffd}\r\n")));
            assert_eq!(fields.get("stderr"), Some(&Value::str("remote stderr\r\n")));
            let envelope = pyjson::loads(&String::from_utf8_lossy(&out.stdout)).unwrap();
            assert_eq!(
                envelope.get("error").unwrap().get("code"),
                Some(&Value::str("remote_exit"))
            );
        } else {
            assert_eq!(out.stdout, b"x\0\xff\r\n");
            assert_eq!(out.stderr, b"remote stderr\r\n");
        }
    }
    let out = s
        .command(&[
            "cp",
            "modal:sb-offline",
            "./a b",
            ":/workspace/a b",
            "--recursive",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(result(&out).get("source"), Some(&Value::str("./a b")));
    assert_eq!(result(&out).get("exit_code"), Some(&Value::Int(0)));
    let out = s
        .command(&["ssh", "modal:sb-offline", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    assert_eq!(
        result(&out).get("ssh_command"),
        Some(&Value::str("gpu ssh modal:sb-offline --exec"))
    );
    let before = s.calls().len();
    let out = s
        .command(&["ssh", "modal:sb-offline", "--exec", "--json"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert_eq!(s.calls().len(), before);
    std::fs::write(s.home.join("timeout-exec"), "").unwrap();
    let out = s
        .command(&[
            "exec",
            "modal:sb-offline",
            "--command-timeout",
            "0.1",
            "--json",
            "--",
            "true",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(20));
    assert!(s.calls().iter().all(|call| call[0] == "uv"));
}

#[test]
fn modal_orphan_inventory_recovery_and_deletion_use_public_volume_ids() {
    let s = sandbox();
    s.stub(
        "uv",
        r#"#!/bin/sh
set -eu
printf 'uv\t%s\n' "$5" >> "$FAKE_CLI_LOG"
case "$5" in
list) echo '{"sandboxes":[],"volumes":[{"name":"gpu-cli-run-orphan"}]}' ;;
volume-copy) [ "$6" = gpu-cli-run-orphan ] && [ "$7" = ./recovered ] ;;
volume-delete) [ "$6" = gpu-cli-run-orphan ]; echo '{"finished":true}' ;;
*) exit 97 ;;
esac
"#,
    );
    let out = s
        .command(&["ls", "--provider", "modal", "--json", "--envelope"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let fields = result(&out);
    let item = &fields.get("instances").unwrap().as_list().unwrap()[0];
    assert_eq!(
        item.get("id"),
        Some(&Value::str("modal:volume:gpu-cli-run-orphan"))
    );
    assert_eq!(item.get("actual_status"), Some(&Value::str("outputs-only")));
    let out = s
        .command(&[
            "cp",
            "modal:volume:gpu-cli-run-orphan",
            ":/",
            "./recovered",
            "--recursive",
            "--json",
        ])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    let out = s
        .command(&["down", "modal:volume:gpu-cli-run-orphan", "--yes", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{out:?}");
    assert_eq!(result(&out).get("destroyed"), Some(&Value::Bool(true)));
}

fn wait_file(s: &Sandbox, name: &str, child: &mut Child) {
    let end = Instant::now() + Duration::from_secs(20);
    while !s.home.join(name).exists() {
        assert!(
            child.try_wait().unwrap().is_none(),
            "child exited before {name}"
        );
        assert!(Instant::now() < end, "waiting for {name}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn signal(child: &Child) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        rustix::process::Signal::INT,
    )
    .unwrap();
}
#[test]
fn modal_interrupt_escalation_keeps_cleanup_and_third_press_recovery_notice() {
    for presses in [1, 2, 3] {
        let s = sandbox();
        if presses >= 2 {
            std::fs::write(s.home.join("slow-download"), "").unwrap();
        }
        if presses == 3 {
            std::fs::write(s.home.join("slow-cleanup"), "").unwrap();
        }
        let mut child = command(&s)
            .env("MODE", "follow")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_file(&s, "following", &mut child);
        signal(&child);
        if presses >= 2 {
            wait_file(&s, "downloading", &mut child);
            signal(&child);
        }
        if presses == 3 {
            wait_file(&s, "cleanup", &mut child);
            signal(&child);
        }
        let output = child.wait_with_output().unwrap();
        assert_eq!(output.status.code(), Some(130));
        if presses == 3 {
            let pid: i32 = std::fs::read_to_string(s.home.join("cleanup-pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            let _ = rustix::process::kill_process(
                rustix::process::Pid::from_raw(pid).unwrap(),
                rustix::process::Signal::KILL,
            );
            let text = String::from_utf8_lossy(&output.stderr);
            assert!(
                text.contains("Sandbox modal:sb-offline may still be running; hard deadline"),
                "{text}"
            );
            assert!(text.contains("gpu down modal:sb-offline --yes"), "{text}");
        } else {
            assert!(s.home.join("destroyed").exists());
            assert_eq!(result(&output).get("destroyed"), Some(&Value::Bool(true)));
        }
    }
}
