//! `gpu job` across separate processes: every command is a fresh `gpu` binary
//! sharing only the sandbox HOME and state directories, with stub `vastai` and
//! `ssh` that keep the remote job's state in files. No cloud call is possible.
mod common;
use common::Sandbox;
use gpu::pyjson::{self, Dict, Value};
use std::process::Output;

fn sandbox() -> Sandbox {
    let s = Sandbox::new();
    std::fs::create_dir(s.root.join("project")).unwrap();
    std::fs::create_dir(s.home.join("outputs")).unwrap();
    std::fs::write(s.home.join("outputs/checkpoint"), b"checkpoint").unwrap();
    std::fs::write(s.root.join("project/train.py"), b"print('hello')").unwrap();
    s.stub(
        "vastai",
        r#"#!/bin/sh
case "$1 $2" in
 'search offers') printf '[{"id":100,"gpu_name":"RTX 4090","dph_total":0.3,"num_gpus":1,"reliability":0.99}]\n';;
 'create instance') touch "$HOME/created"; printf '{"success":true,"new_contract":123}\n';;
 'show instances') if [ -e "$HOME/destroyed" ] || [ -e "$HOME/vanished" ]; then echo '[]'; else echo '[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":1234}]'; fi;;
 'destroy instance') touch "$HOME/destroyed"; echo ok;;
 *) exit 97;;
esac
"#,
    );
    // The remote supervisor's state lives in $HOME/remote-*: `status` is the
    // line the real one prints, `log` the job's output.
    s.stub(
        "ssh",
        r#"#!/bin/sh
for last do :; done
case "$last" in
 true) exit 0;;
 *' -s -- arm '*) cat >/dev/null; echo arm >>"$HOME/arms"; printf '{"status":"armed","deadline":2000000000}\n';;
 'mkdir -p '*) cat >/dev/null;;
 *' -s -- start '*) cat >"$HOME/payload"; touch "$HOME/started"; echo started;;
 *' stop '*) touch "$HOME/stopped"; printf 'job\t143\t0\t6\n' >"$HOME/remote-status";;
 *' status '*) cat "$HOME/remote-status";;
 'dd if='*) skip=${last#*skip=}; skip=${skip%% *}; count=${last#*count=}; count=${count%% *}
            tail -c +$((skip + 1)) "$HOME/remote-log" | head -c "$count";;
 'tar -C '*) touch "$HOME/downloading"; export COPYFILE_DISABLE=1; exec tar --format=ustar -C "$HOME" -cf - outputs;;
 *) echo "unexpected ssh" >&2; exit 97;;
esac
"#,
    );
    std::fs::write(s.home.join("remote-status"), "setup\t-\t0\t0\n").unwrap();
    std::fs::write(s.home.join("remote-log"), "step 1\nstep 2\n").unwrap();
    s
}

fn gpu(s: &Sandbox, args: &[&str]) -> Output {
    s.command(args)
        .current_dir(s.root.join("project"))
        .output()
        .unwrap()
}

fn json(out: &Output) -> Dict {
    let text = String::from_utf8_lossy(&out.stdout);
    assert_eq!(text.lines().count(), 1, "{text}");
    pyjson::loads(&text).unwrap().as_dict().unwrap().clone()
}

fn result(out: &Output) -> Dict {
    json(out).get("result").unwrap().as_dict().unwrap().clone()
}

fn start(s: &Sandbox, extra: &[&str]) -> String {
    let mut args = vec![
        "job", "start", "--gpu", "RTX 4090", "--expire", "30m", "--yes", "--json",
    ];
    args.extend_from_slice(extra);
    args.extend_from_slice(&["--", "python", "train.py"]);
    let out = gpu(s, &args);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let envelope = json(&out);
    assert_eq!(
        envelope.get("command").and_then(Value::as_str),
        Some("job start")
    );
    result(&out)
        .get("job_id")
        .and_then(Value::as_str)
        .unwrap()
        .to_string()
}

fn finish(s: &Sandbox, code: i32) {
    std::fs::write(
        s.home.join("remote-status"),
        format!("job\t{code}\t0\t14\n"),
    )
    .unwrap();
}

fn state(s: &Sandbox, id: &str) -> Dict {
    let out = gpu(s, &["job", "status", id, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    result(&out)
}

#[test]
fn start_returns_while_the_job_runs_and_status_follows_its_states() {
    let s = sandbox();
    let id = start(&s, &["--owner", "alice"]);
    // Rented, uploaded and launched detached; nothing waited, nothing destroyed.
    assert!(s.home.join("created").exists());
    assert!(s.home.join("started").exists());
    assert!(!s.home.join("destroyed").exists());
    assert!(!s.home.join("downloading").exists());
    let payload = std::fs::read_to_string(s.home.join("payload")).unwrap();
    assert!(payload.contains("DETACHED=1"), "{payload}");
    assert!(id.ends_with("-123"), "{id}");
    let record = s.root.join(format!("state/gpu/jobs/{id}.json"));
    assert!(record.exists());

    let status = state(&s, &id);
    assert_eq!(status.get("state"), Some(&Value::str("booting")));
    assert_eq!(status.get("exit_code"), Some(&Value::Null));
    assert_eq!(status.get("owner"), Some(&Value::str("alice")));
    assert_eq!(status.get("id"), Some(&Value::str("123")));

    std::fs::write(s.home.join("remote-status"), "job\t-\t0\t7\n").unwrap();
    assert_eq!(state(&s, &id).get("state"), Some(&Value::str("running")));

    finish(&s, 0);
    let done = state(&s, &id);
    assert_eq!(done.get("state"), Some(&Value::str("succeeded")));
    assert_eq!(done.get("exit_code"), Some(&Value::Int(0)));
    assert!(matches!(done.get("cost_usd"), Some(Value::Float(_))));
    // The exit code is kept in the record, not asked of the instance again.
    assert!(std::fs::read_to_string(&record)
        .unwrap()
        .contains("\"exit_code\": 0"));

    finish(&s, 3);
    assert_eq!(state(&s, &id).get("state"), Some(&Value::str("failed")));
    // The instance is addressable by its Vast ID as well.
    assert_eq!(
        state(&s, "123").get("job_id").and_then(Value::as_str),
        Some(id.as_str())
    );
}

#[test]
fn logs_print_the_log_so_far_and_resume_from_an_offset() {
    let s = sandbox();
    let id = start(&s, &[]);
    std::fs::write(s.home.join("remote-status"), "job\t-\t0\t14\n").unwrap();
    let out = gpu(&s, &["job", "logs", &id]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "step 1\nstep 2\n");

    let out = gpu(&s, &["job", "logs", &id, "--json", "--offset", "7"]);
    let r = result(&out);
    assert_eq!(r.get("log"), Some(&Value::str("step 2\n")));
    assert_eq!(r.get("next_offset"), Some(&Value::Int(14)));

    // --follow ends when the job does and prints what it wrote.
    finish(&s, 0);
    let out = gpu(&s, &["job", "logs", &id, "--follow"]);
    assert!(out.status.success());
    assert_eq!(String::from_utf8_lossy(&out.stdout), "step 1\nstep 2\n");
}

#[test]
fn outputs_downloads_then_destroys_and_is_safe_to_repeat() {
    let s = sandbox();
    let id = start(&s, &[]);

    // A running job's outputs are not collected, and nothing is destroyed.
    std::fs::write(s.home.join("remote-status"), "job\t-\t0\t14\n").unwrap();
    let out = gpu(&s, &["job", "outputs", &id, "--json"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(!s.home.join("destroyed").exists());
    assert!(!s.home.join("downloading").exists());

    finish(&s, 0);
    let out = gpu(&s, &["job", "outputs", &id, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = result(&out);
    assert_eq!(r.get("destroyed"), Some(&Value::Bool(true)));
    assert_eq!(r.get("exit_code"), Some(&Value::Int(0)));
    assert!(s.home.join("downloading").exists());
    assert!(s.home.join("destroyed").exists());
    let local = s.root.join("project/gpu-runs").join(&id);
    assert_eq!(
        std::fs::read(local.join("outputs/checkpoint")).unwrap(),
        b"checkpoint"
    );
    assert_eq!(
        std::fs::read_to_string(local.join("log.txt")).unwrap(),
        "step 1\nstep 2\n"
    );
    assert!(local.join("result.json").exists());

    // Later calls answer from the record: the instance is gone.
    let calls = s.calls().len();
    let status = state(&s, &id);
    assert_eq!(status.get("state"), Some(&Value::str("succeeded")));
    assert_eq!(status.get("destroyed"), Some(&Value::Bool(true)));
    assert_eq!(status.get("outputs_collected"), Some(&Value::Bool(true)));
    let again = gpu(&s, &["job", "outputs", &id, "--json"]);
    assert!(again.status.success());
    let logs = gpu(&s, &["job", "logs", &id]);
    assert_eq!(String::from_utf8_lossy(&logs.stdout), "step 1\nstep 2\n");
    assert_eq!(s.calls().len(), calls, "no provider call after collecting");
}

#[test]
fn a_failed_job_still_hands_over_its_outputs_and_exit_code() {
    let s = sandbox();
    let id = start(&s, &[]);
    finish(&s, 3);
    let out = gpu(&s, &["job", "outputs", &id, "--json"]);
    assert!(out.status.success());
    let r = result(&out);
    assert_eq!(r.get("state"), Some(&Value::str("failed")));
    assert_eq!(r.get("exit_code"), Some(&Value::Int(3)));
    assert!(s.home.join("destroyed").exists());
}

#[test]
fn linger_keeps_the_instance_after_outputs_with_a_shorter_expiry() {
    let s = sandbox();
    let id = start(&s, &["--linger", "10m"]);
    finish(&s, 0);
    let arms = |s: &Sandbox| {
        std::fs::read_to_string(s.home.join("arms"))
            .unwrap_or_default()
            .lines()
            .count()
    };
    let before = arms(&s);
    let out = gpu(&s, &["job", "outputs", &id, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let r = result(&out);
    assert_eq!(r.get("destroyed"), Some(&Value::Bool(false)));
    assert!(r.get("retained_until").is_some());
    assert!(!s.home.join("destroyed").exists());
    assert!(arms(&s) > before, "retention is armed on the instance");
}

#[test]
fn cancel_stops_the_job_and_destroys_the_instance() {
    let s = sandbox();
    let id = start(&s, &[]);
    std::fs::write(s.home.join("remote-status"), "job\t-\t0\t7\n").unwrap();
    let out = gpu(&s, &["job", "cancel", &id, "--json"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(result(&out).get("state"), Some(&Value::str("cancelled")));
    assert!(s.home.join("stopped").exists());
    assert!(s.home.join("destroyed").exists());
    let status = state(&s, &id);
    assert_eq!(status.get("state"), Some(&Value::str("cancelled")));
    assert_eq!(status.get("destroyed"), Some(&Value::Bool(true)));
    // Cancelling again, or asking for outputs, is an answer, not a second destroy.
    assert!(gpu(&s, &["job", "cancel", &id]).status.success());
    assert_eq!(gpu(&s, &["job", "outputs", &id]).status.code(), Some(1));
}

#[test]
fn a_vanished_instance_is_reported_lost_with_its_exit_code_kept() {
    let s = sandbox();
    let id = start(&s, &[]);
    finish(&s, 0);
    assert_eq!(state(&s, &id).get("state"), Some(&Value::str("succeeded")));
    std::fs::write(s.home.join("vanished"), "").unwrap();
    let status = state(&s, &id);
    assert_eq!(status.get("state"), Some(&Value::str("lost")));
    assert_eq!(status.get("exit_code"), Some(&Value::Int(0)));
    assert_eq!(status.get("destroyed"), Some(&Value::Bool(true)));
    let out = gpu(&s, &["job", "outputs", &id]);
    assert_eq!(out.status.code(), Some(18));
}

#[test]
fn ls_lists_jobs_and_filters_by_owner() {
    let s = sandbox();
    let first = start(&s, &["--owner", "alice"]);
    let out = gpu(&s, &["job", "ls", "--json"]);
    let r = result(&out);
    let jobs = r.get("jobs").unwrap().as_list().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(
        jobs[0].as_dict().unwrap().get("job_id"),
        Some(&Value::str(&first))
    );
    let none = gpu(&s, &["job", "ls", "--owner", "bob", "--json"]);
    assert!(result(&none)
        .get("jobs")
        .unwrap()
        .as_list()
        .unwrap()
        .is_empty());
    let alice = gpu(&s, &["job", "ls", "--owner", "alice"]);
    let text = String::from_utf8_lossy(&alice.stdout);
    assert!(text.contains(&first) && text.contains("booting"), "{text}");
}

#[test]
fn start_keeps_every_run_rail() {
    let s = sandbox();
    // Required expiry, owner syntax and secret refusal come from the shared code.
    let out = gpu(
        &s,
        &["job", "start", "--gpu", "RTX 4090", "--yes", "--", "true"],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("gpu job start requires --expire"));
    let out = gpu(
        &s,
        &[
            "job",
            "start",
            "--gpu",
            "RTX 4090",
            "--expire",
            "30m",
            "--yes",
            "--owner",
            "Bad Owner",
            "--",
            "true",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    std::fs::write(s.root.join("project/.env"), "TOKEN=x").unwrap();
    let out = gpu(
        &s,
        &[
            "job",
            "start",
            "--gpu",
            "RTX 4090",
            "--expire",
            "30m",
            "--yes",
            "--include",
            ".env",
            "--",
            "true",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("secret-looking"));
    let out = gpu(
        &s,
        &[
            "job",
            "start",
            "--gpu",
            "RTX 4090",
            "--expire",
            "30m",
            "--yes",
            "--max-upload",
            "1",
            "--",
            "true",
        ],
    );
    assert!(!out.status.success());
    assert!(!s.home.join("created").exists());
    // Modal keeps its blocking path; the job commands are Vast only.
    let out = gpu(
        &s,
        &[
            "job",
            "start",
            "--provider",
            "modal",
            "--gpu",
            "T4",
            "--expire",
            "30m",
            "--yes",
            "--",
            "true",
        ],
    );
    assert_eq!(out.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&out.stderr).contains("Vast only"));
    std::fs::remove_file(s.root.join("project/.env")).unwrap();
    // A dry run plans without renting or saving a record.
    let out = gpu(
        &s,
        &[
            "job",
            "start",
            "--gpu",
            "RTX 4090",
            "--expire",
            "30m",
            "--dry-run",
            "--json",
            "--",
            "true",
        ],
    );
    assert!(
        out.status.success(),
        "{} {}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!s.home.join("created").exists());
    assert!(!s.root.join("state/gpu/jobs").exists());
}

#[test]
fn unknown_and_damaged_job_ids_are_clear_errors() {
    let s = sandbox();
    let out = gpu(&s, &["job", "status", "nope", "--json"]);
    assert_eq!(out.status.code(), Some(16));
    let out = gpu(&s, &["job", "status", "../../etc/passwd"]);
    assert_eq!(out.status.code(), Some(16));
    std::fs::create_dir_all(s.root.join("state/gpu/jobs")).unwrap();
    std::fs::write(s.root.join("state/gpu/jobs/broken.json"), "{").unwrap();
    let out = gpu(&s, &["job", "status", "broken"]);
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("damaged"));
    let out = gpu(&s, &["job", "ls", "--json"]);
    assert!(out.status.success());
    assert_eq!(
        result(&out)
            .get("damaged_records")
            .unwrap()
            .as_list()
            .unwrap()
            .len(),
        1
    );
}
