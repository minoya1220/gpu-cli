//! Background autosync with the real binary: the detached process starts,
//! outlives the command that started it, is found and stopped by later
//! commands, and ends itself when its instance is gone. `ssh` and `vastai`
//! are stubs that act like one healthy (or destroyed) instance with no files.

mod common;

use std::path::Path;
use std::process::Stdio;
use std::time::{Duration, Instant};

use common::Sandbox;

const SSH: &str = r#"#!/bin/sh
for last; do :; done
[ -e "@DOWN@" ] && { echo "ssh: connect: Connection refused" >&2; exit 255; }
case "$last" in
    true) exit 0 ;;
    *"tar -xzf"*) cat >/dev/null ;;
    *" prepare "*) cat >/dev/null; printf 'rsync=ok\nexisting=0\nwatcher=poll\n' ;;
    *" manifest "*) cat >/dev/null; printf 'sig=empty\n' ;;
    *" wait "*) cat >/dev/null; sleep 1; printf 'status=idle\n' ;;
    *) echo "unexpected ssh $last" >&2; exit 97 ;;
esac
"#;

const LIVE: &str = r#"[{"id": 7, "gpu_name": "RTX 4090", "actual_status": "running", "ssh_host": "ssh6.vast.ai", "ssh_port": 2222, "label": "no-repo"}]"#;

struct Project {
    sandbox: Sandbox,
    dir: std::path::PathBuf,
    down: std::path::PathBuf,
}

impl Project {
    fn new() -> Project {
        let sandbox = Sandbox::new();
        let down = sandbox.root.join("instance-is-down");
        sandbox.stub("ssh", &SSH.replace("@DOWN@", &down.display().to_string()));
        sandbox.stub("tar", "#!/bin/sh\nexec /usr/bin/tar \"$@\"\n");
        let project = Project {
            dir: sandbox.root.join("proj"),
            down,
            sandbox,
        };
        project.instances(LIVE);
        std::fs::create_dir(&project.dir).unwrap();
        std::fs::write(project.dir.join("train.py"), "print('hi')\n").unwrap();
        project
    }

    /// What `vastai show instances` lists.
    fn instances(&self, json: &str) {
        self.sandbox.stub(
            "vastai",
            &common::FAKE_CLI.replace(
                "case \"$tool $1 $2\" in",
                &format!(
                    "case \"$tool $1 $2\" in\n    \"vastai show instances\") echo '{json}' ;;"
                ),
            ),
        );
    }

    fn gpu(&self, args: &[&str]) -> (i32, String, String) {
        let out = self
            .sandbox
            .command(args)
            .stdin(Stdio::null())
            .output()
            .unwrap();
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
            String::from_utf8_lossy(&out.stderr).into_owned(),
        )
    }

    fn state(&self) -> std::path::PathBuf {
        self.sandbox.root.join("state/gpu/sync")
    }

    fn files(&self, extension: &str) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.state())
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.extension().is_some_and(|e| e == extension))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn pid(&self) -> Option<i32> {
        let file = self.files("autosync").into_iter().next()?;
        std::fs::read_to_string(file).ok()?.trim().parse().ok()
    }
}

fn alive(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

fn wait_until(what: &str, seconds: u64, done: impl Fn() -> bool) {
    let started = Instant::now();
    while !done() {
        assert!(
            started.elapsed() < Duration::from_secs(seconds),
            "timed out waiting for {what}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[test]
fn autosync_runs_detached_is_listed_and_stopped_by_later_commands() {
    let p = Project::new();
    let dir = p.dir.to_str().unwrap();
    let (code, _, err) = p.gpu(&["link", "7", "--dir", dir, "--yes"]);
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = p.gpu(&["sync", "7", "--dir", dir, "--autosync"]);
    assert_eq!(code, 0, "{err}");
    assert!(
        out.contains("Autosync started for 7 (process"),
        "{out}{err}"
    );
    // The command that started it has exited; the syncer is still there.
    let pid = p.pid().expect("the syncer recorded its process");
    assert!(alive(pid));
    let log = p.files("log").into_iter().next().expect("a log file");
    wait_until("the syncer's first log line", 20, || {
        read(&log).contains("Autosync started:")
    });
    // It is found by a later command, which also refuses to start another.
    let (code, out, _) = p.gpu(&["sync", "7", "--dir", dir, "--autosync"]);
    assert_eq!(code, 0);
    assert!(out.contains("Autosync already runs"), "{out}");
    let (code, out, err) = p.gpu(&["sync", "--dir", dir, "--list"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("running"), "{out}");
    let (_, out, err) = p.gpu(&["sync", "--dir", dir, "--watch"]);
    assert!(
        out.contains("Autosync already syncs") || err.contains("Autosync already syncs"),
        "{out}{err}"
    );
    // Stopping it needs no connection to the instance.
    std::fs::write(&p.down, "").unwrap();
    let (code, out, err) = p.gpu(&["sync", "--dir", dir, "--no-autosync"]);
    assert_eq!(code, 0, "{err}");
    assert!(out.contains("Stopped autosync of 7"), "{out}");
    wait_until("the syncer to exit", 10, || !alive(pid));
    std::fs::remove_file(&p.down).unwrap();
    let (_, out, _) = p.gpu(&["sync", "--dir", dir, "--list"]);
    assert!(out.contains("off"), "{out}");
    // Nothing secret reached the log.
    assert!(!read(&log).contains("API_KEY"));
}

#[test]
fn autosync_ends_itself_when_its_instance_is_destroyed() {
    let p = Project::new();
    let dir = p.dir.to_str().unwrap();
    let (code, _, err) = p.gpu(&["link", "7", "--dir", dir, "--yes", "--autosync"]);
    assert_eq!(code, 0, "{err}");
    let pid = p.pid().expect("the syncer recorded its process");
    let log = p.files("log").into_iter().next().expect("a log file");
    wait_until("the syncer's first log line", 20, || {
        read(&log).contains("Autosync started:")
    });
    // The instance disappears from the provider and from the network.
    p.instances("[]");
    std::fs::write(&p.down, "").unwrap();
    wait_until("the syncer to give up on a destroyed instance", 60, || {
        !alive(pid)
    });
    let text = read(&log);
    assert!(text.contains("Sync paused"), "{text}");
    assert!(
        text.contains("Autosync stopped: the instance no longer exists."),
        "{text}"
    );
    assert!(p.files("json").is_empty(), "the link is forgotten");
}
