use super::*;
use crate::testing::TestEnv;
use std::io::Write;
use std::process::{Command, Stdio};

fn execute(env: &TestEnv, script: &str) -> std::process::Output {
    let mut child = Command::new("bash")
        .arg("-s")
        .env_clear()
        .env("HOME", &env.home)
        .env("PATH", format!("{}:/usr/bin:/bin", env.bin.display()))
        .env("MARKER", env.root.join("installed"))
        .current_dir(&env.root)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(script.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

fn checksum_tool(env: &TestEnv) {
    if !std::path::Path::new("/usr/bin/sha256sum").exists() {
        env.executable(
            "sha256sum",
            "#!/bin/sh\nexec /usr/bin/shasum -a256 \"$@\"\n",
        );
    }
}

#[test]
fn pinned_uv_checksum_failure_never_extracts_or_installs() {
    let env = TestEnv::new();
    checksum_tool(&env);
    env.executable("uname", "#!/bin/sh\necho x86_64\n");
    env.executable(
        "curl",
        "#!/bin/sh\nfor last; do :; done\nprintf corrupt >\"$last\"\n",
    );
    for tool in ["tar", "install"] {
        env.executable(tool, "#!/bin/sh\ntouch \"$MARKER\"\nexit 0\n");
    }
    let output = execute(&env, UV_INSTALL);
    assert!(!output.status.success());
    assert!(!env.root.join("installed").exists());
    assert!(String::from_utf8_lossy(&output.stdout).contains("FAILED"));
}

#[test]
fn vm_keyring_checksum_failure_never_reaches_dpkg_and_reports_failed_setup() {
    let env = TestEnv::new();
    checksum_tool(&env);
    env.executable(
        "wget",
        "#!/bin/sh\nprintf corrupt >cuda-keyring_1.1-1_all.deb\n",
    );
    env.executable("dpkg", "#!/bin/sh\ntouch \"$MARKER\"\nexit 0\n");
    let script = startup_script(&Repo::NoRepo, "vm", &config::defaults(), None)
        .unwrap()
        .replace("/workspace", &env.root.join("workspace").to_string_lossy())
        .replace("/root", &env.home.to_string_lossy())
        .replace("cd /tmp", &format!("cd {}", env.root.display()))
        .replace("source /etc/environment", "source /dev/null");
    let output = execute(&env, &script);
    assert!(!output.status.success());
    assert!(!env.root.join("installed").exists());
    assert_eq!(
        std::fs::read_to_string(env.root.join("workspace/.gpu-setup")).unwrap(),
        "failed\n"
    );
}

#[test]
fn failed_clone_is_recorded_without_claiming_setup_success() {
    let env = TestEnv::new();
    env.executable("apt", "#!/bin/sh\nexit 0\n");
    env.executable("entrypoint.sh", "#!/bin/sh\ntouch \"$MARKER\"\n");
    env.executable(
        "git",
        "#!/bin/sh\n[ \"$1\" != clone ] && exit 0\necho 'clone denied' >&2\nexit 128\n",
    );
    let script = startup_script(
        &Repo::from_arg("owner/project"),
        "container",
        &config::defaults(),
        None,
    )
    .unwrap()
    .replace("/workspace", &env.root.join("workspace").to_string_lossy())
    .replace("/root", &env.home.to_string_lossy())
    .replace("/venv", &env.root.join("venv").to_string_lossy())
    .replace("source /etc/environment", "source /dev/null");
    let output = execute(&env, &script);
    assert!(output.status.success());
    assert!(
        env.root.join("installed").exists(),
        "entrypoint must still start SSH/services"
    );
    assert_eq!(
        std::fs::read_to_string(env.root.join("workspace/.gpu-setup")).unwrap(),
        "failed\n"
    );
    assert!(!env.home.join(".works").exists());
}

#[test]
fn failed_gh_install_and_auth_still_clone_and_finish_setup() {
    let env = TestEnv::new();
    env.executable("apt", "#!/bin/sh\nexit 100\n");
    env.executable("gh", "#!/bin/sh\nexit 1\n");
    env.executable("sed", "#!/bin/sh\nexit 0\n");
    env.executable("entrypoint.sh", "#!/bin/sh\nexit 0\n");
    env.executable(
        "git",
        "#!/bin/sh\n[ \"$1\" = clone ] && mkdir -p \"$3\" && touch \"$MARKER\"\nexit 0\n",
    );
    let script = startup_script(
        &Repo::from_arg("owner/project"),
        "container",
        &config::defaults(),
        None,
    )
    .unwrap()
    .replace("/workspace", &env.root.join("workspace").to_string_lossy())
    .replace("/root", &env.home.to_string_lossy())
    .replace("/venv", &env.root.join("venv").to_string_lossy())
    .replace("source /etc/environment", "export GH_TOKEN=token; true");
    let output = execute(&env, &script);
    assert!(output.status.success());
    assert!(
        env.root.join("installed").exists(),
        "repository must be cloned"
    );
    assert_eq!(
        std::fs::read_to_string(env.root.join("workspace/.gpu-setup")).unwrap(),
        "done\n"
    );
    assert!(env.home.join(".works").exists());
}

#[test]
fn packed_script_body_is_not_readable_from_stdin() {
    let env = TestEnv::new();
    let marker = env.root.join("installed");
    let mut lines = vec!["#!/bin/bash".to_string(), "cat >/dev/null".to_string()];
    lines.extend((0..ONSTART_LIMIT / 4).map(|i| format!(": {i:x}")));
    lines.push(format!("touch {}", shell_quote(&marker.to_string_lossy())));
    let script = bounded_script(lines).unwrap();
    assert!(script.len() <= ONSTART_LIMIT);
    let script_path = env.root.join("onstart.sh");
    std::fs::write(&script_path, script).unwrap();
    let output = Command::new("bash")
        .arg(&script_path)
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(marker.exists(), "lines after a stdin reader must still run");
}
