//! Real subprocess and signal semantics with forbidden-by-default cloud stubs.
mod common;
use common::Sandbox;
use gpu::pyjson::{self, Value};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

fn sandbox() -> Sandbox {
    let s = Sandbox::new();
    std::fs::create_dir(s.root.join("project")).unwrap();
    std::fs::create_dir(s.home.join("outputs")).unwrap();
    std::fs::write(s.home.join("outputs/checkpoint"), b"checkpoint").unwrap();
    std::fs::write(s.root.join("project/train.py"), b"print('hello')").unwrap();
    s.stub("vastai", r#"#!/bin/sh
case "$1 $2" in
 'search offers') printf '[{"id":100,"gpu_name":"RTX 4090","dph_total":0.3,"num_gpus":1,"reliability":0.99}]\n';;
 'create instance') touch "$HOME/created"; printf '{"success":true,"new_contract":123}\n';;
 'show instances') if [ -e "$HOME/destroyed" ]; then echo '[]'; else echo '[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":1234}]'; fi;;
 'destroy instance') touch "$HOME/destroyed"; echo ok;;
 *) exit 97;;
esac
"#);
    s.stub("ssh", r#"#!/bin/sh
for last do :; done
case "$last" in
 true) exit 0;;
 *' -s -- arm '*) cat >/dev/null; printf '{"status":"armed","deadline":2000000000}\n';;
 'mkdir -p '*) cat >/dev/null;;
 *' -s -- start '*) cat >"$HOME/payload"; touch "$HOME/started"; echo started;;
 *' stop '*) touch "$HOME/stopped";;
 *' status '*) touch "$HOME/following"; if [ -e "$HOME/stopped" ]; then printf 'job\t130\t0\t0\n'; else printf 'job\t-\t0\t0\n'; fi;;
 'tar -C '*) touch "$HOME/downloading"; if [ -e "$HOME/slow-download" ]; then exec sleep 120; fi; export COPYFILE_DISABLE=1; exec tar --format=ustar -C "$HOME" -cf - outputs;;
 *) echo "unexpected ssh" >&2; exit 97;;
esac
"#);
    s
}
fn wait_file(s: &Sandbox, name: &str, child: &mut Child) {
    let until = Instant::now() + Duration::from_secs(20);
    while !s.home.join(name).exists() {
        if let Some(status) = child.try_wait().unwrap() {
            panic!("child exited early: {status}");
        }
        assert!(Instant::now() < until, "waiting for {name}");
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn signal(child: &Child, sig: rustix::process::Signal) {
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        sig,
    )
    .unwrap();
}

#[test]
fn dry_run_has_a_size_plan_but_never_creates() {
    let s = sandbox();
    let out = s
        .command(&[
            "run",
            "--gpu",
            "RTX 4090",
            "--expire",
            "30m",
            "--dry-run",
            "--json",
            "--",
            "python",
            "train.py",
        ])
        .current_dir(s.root.join("project"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let object = pyjson::loads(&String::from_utf8_lossy(&out.stdout)).unwrap();
    let result = object.get("result").unwrap();
    assert_eq!(result.get("dry_run"), Some(&Value::Bool(true)));
    assert_eq!(result.get("created"), Some(&Value::Bool(false)));
    assert_eq!(result.get("uploaded_files"), Some(&Value::Int(1)));
    assert!(!s.home.join("created").exists());
    for forbidden in ["--interruptible", "--detach", "--mode"] {
        let output = s
            .command(&["run", forbidden, "--json", "--", "true"])
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2));
    }
}

#[test]
fn first_interrupt_downloads_and_destroys_before_returning_one_json_object() {
    for sig in [
        rustix::process::Signal::INT,
        rustix::process::Signal::HUP,
        rustix::process::Signal::TERM,
    ] {
        let s = sandbox();
        let mut child = s
            .command(&[
                "run", "--gpu", "RTX 4090", "--expire", "30m", "--yes", "--json", "--", "python",
                "train.py",
            ])
            .current_dir(s.root.join("project"))
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        wait_file(&s, "following", &mut child);
        signal(&child, sig);
        let output = child.wait_with_output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(130),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert_eq!(text.lines().count(), 1);
        let result = pyjson::loads(&text).unwrap();
        assert_eq!(
            result.get("result").unwrap().get("destroyed"),
            Some(&Value::Bool(true))
        );
        assert!(s.home.join("destroyed").exists());
        assert!(s.home.join("downloading").exists());
        let runs = std::fs::read_dir(s.root.join("project/gpu-runs")).unwrap();
        let path = runs.map(|d| d.unwrap().path()).next().unwrap();
        assert_eq!(
            std::fs::read(path.join("outputs/checkpoint")).unwrap_or_else(|e| panic!(
                "{e}; {text}; {}",
                String::from_utf8_lossy(&output.stderr)
            )),
            b"checkpoint"
        );
    }
}

#[test]
fn second_interrupt_aborts_download_but_still_verifies_destroy() {
    let s = sandbox();
    std::fs::write(s.home.join("slow-download"), "").unwrap();
    let mut child = s
        .command(&[
            "run", "--gpu", "RTX 4090", "--expire", "30m", "--yes", "--json", "--", "sleep", "120",
        ])
        .current_dir(s.root.join("project"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_file(&s, "following", &mut child);
    signal(&child, rustix::process::Signal::INT);
    wait_file(&s, "downloading", &mut child);
    signal(&child, rustix::process::Signal::INT);
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(130));
    assert!(s.home.join("destroyed").exists());
    assert!(String::from_utf8_lossy(&output.stderr).contains("Skipping download"));
}

#[test]
fn third_interrupt_exits_immediately_with_id_and_deadline_notice() {
    let s = sandbox();
    std::fs::write(s.home.join("slow-download"), "").unwrap();
    // Keep cleanup in progress long enough to deliver a third independent press.
    s.stub(
        "vastai",
        r#"#!/bin/sh
case "$1 $2" in
 'search offers') echo '[{"id":100,"gpu_name":"RTX 4090","dph_total":0.3,"num_gpus":1}]';;
 'create instance') echo '{"success":true,"new_contract":123}';;
 'show instances') echo '[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":1234}]';;
 'destroy instance') echo $$ >"$HOME/cleanup-pid"; touch "$HOME/cleanup"; exec sleep 120;;
 *) exit 97;;
esac
"#,
    );
    let mut child = s
        .command(&[
            "run", "--gpu", "RTX 4090", "--expire", "30m", "--yes", "--", "sleep", "120",
        ])
        .current_dir(s.root.join("project"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_file(&s, "following", &mut child);
    signal(&child, rustix::process::Signal::INT);
    wait_file(&s, "downloading", &mut child);
    signal(&child, rustix::process::Signal::INT);
    wait_file(&s, "cleanup", &mut child);
    signal(&child, rustix::process::Signal::INT);
    // The cleanup subprocess is an intentionally interrupted fake. Kill it so
    // no sleeping test helper remains after the parent CLI's immediate exit.
    let output = child.wait_with_output().unwrap();
    let pid: i32 = std::fs::read_to_string(s.home.join("cleanup-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    );
    assert_eq!(output.status.code(), Some(130));
    let text = String::from_utf8_lossy(&output.stderr);
    assert!(
        text.contains("Instance 123 may still be running; it self-destroys at"),
        "{text}"
    );
}

#[test]
fn interrupt_before_create_cancels_at_once_without_cleanup() {
    let s = sandbox();
    s.stub(
        "vastai",
        r#"#!/bin/sh
case "$1 $2" in
 'search offers') echo $$ >"$HOME/search-pid"; touch "$HOME/searching"; exec sleep 120;;
 *) touch "$HOME/unexpected"; exit 97;;
esac
"#,
    );
    let mut child = s
        .command(&[
            "run", "--gpu", "RTX 4090", "--expire", "30m", "--yes", "--", "python", "train.py",
        ])
        .current_dir(s.root.join("project"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    wait_file(&s, "searching", &mut child);
    let started = Instant::now();
    signal(&child, rustix::process::Signal::INT);
    let output = child.wait_with_output().unwrap();
    let pid: i32 = std::fs::read_to_string(s.home.join("search-pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let _ = rustix::process::kill_process(
        rustix::process::Pid::from_raw(pid).unwrap(),
        rustix::process::Signal::KILL,
    );
    assert_eq!(output.status.code(), Some(130));
    assert!(started.elapsed() < Duration::from_secs(10));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("Stopping job"), "{stderr}");
    assert!(!s.home.join("unexpected").exists());
}

#[test]
fn terminal_interrupts_to_the_foreground_group_do_not_kill_cleanup() {
    use std::os::unix::process::CommandExt;
    let s = sandbox();
    s.stub(
        "vastai",
        r#"#!/bin/sh
case "$1 $2" in
 'search offers') printf '[{"id":100,"gpu_name":"RTX 4090","dph_total":0.3,"num_gpus":1,"reliability":0.99}]\n';;
 'create instance') printf '{"success":true,"new_contract":123}\n';;
 'show instances') if [ -e "$HOME/destroyed" ]; then echo '[]'; else echo '[{"id":123,"actual_status":"running","ssh_host":"fake","ssh_port":1234}]'; fi;;
 'destroy instance') touch "$HOME/cleanup"; while [ ! -e "$HOME/proceed" ]; do sleep 0.05; done; touch "$HOME/destroyed"; echo ok;;
 *) exit 97;;
esac
"#,
    );
    // Its own group stands in for the terminal's foreground process group.
    let mut child = s
        .command(&[
            "run", "--gpu", "RTX 4090", "--expire", "30m", "--yes", "--json", "--", "python",
            "train.py",
        ])
        .current_dir(s.root.join("project"))
        .process_group(0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let group = rustix::process::Pid::from_raw(child.id() as i32).unwrap();
    wait_file(&s, "following", &mut child);
    rustix::process::kill_process_group(group, rustix::process::Signal::INT).unwrap();
    wait_file(&s, "cleanup", &mut child);
    rustix::process::kill_process_group(group, rustix::process::Signal::INT).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    std::fs::write(s.home.join("proceed"), "").unwrap();
    let output = child.wait_with_output().unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(output.status.code(), Some(130), "{stderr}");
    let result = pyjson::loads(&String::from_utf8(output.stdout).unwrap()).unwrap();
    assert_eq!(
        result.get("result").unwrap().get("destroyed"),
        Some(&Value::Bool(true))
    );
    assert!(s.home.join("destroyed").exists());
}
