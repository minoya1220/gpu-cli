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
