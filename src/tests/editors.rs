//! Port of tests/test_editors.py. Editor tests never contact Vast, GitHub,
//! or an SSH server.

use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use sha2::{Digest, Sha256};

use super::instances::{editor_stubs, fake_vast, instances, ssh_ready};
use crate::editors::last_editor;
use crate::editors::{
    after_up_screen, editor_screen, launch_editor, nvim_bootstrap, nvim_bootstrap_with,
    nvim_session, EDITORS, NVIM_VERSION,
};
use crate::history::recent_picks;
use crate::picker::testing::*;
use crate::picker::Outcome;
use crate::process::{Input, Output};
use crate::provider::{Instance, VastId};
use crate::pyfmt::shell_quote;
use crate::pyjson::Value;
use crate::testing::*;

fn accepting() -> std::sync::Arc<FakeRunner> {
    FakeRunner::new(|_| Ok(Output::status(0)))
}

fn with_stubs() -> TestEnv {
    let t = TestEnv::new();
    editor_stubs(&t);
    t
}

fn stdin_text(input: &Input) -> &str {
    match input {
        Input::Text(text) => text,
        other => panic!("no stdin text: {other:?}"),
    }
}

#[test]
fn test_gui_commands() {
    let cases = [
        (
            "cursor",
            vec![
                "cursor",
                "--remote",
                "ssh-remote+vast-12",
                "/workspace/repo",
            ],
        ),
        (
            "code",
            vec!["code", "--remote", "ssh-remote+vast-12", "/workspace/repo"],
        ),
        ("zed", vec!["zed", "ssh://vast-12/workspace/repo"]),
    ];
    for (editor, expected) in cases {
        let t = with_stubs();
        let runner = accepting();
        let ctx = t.ctx().runner(&runner).build();
        launch_editor(&ctx, editor, "vast-12", "/workspace/repo", None).unwrap();
        assert_eq!(runner.argvs()[0], expected);
    }
}

#[test]
fn test_missing_cli() {
    for editor in ["cursor", "code", "zed"] {
        let t = TestEnv::new();
        let ctx = t.ctx().build();
        let error = launch_editor(&ctx, editor, "vast-12", "/workspace", None).unwrap_err();
        let text = error.text();
        assert!(text.contains("not found on PATH"), "{text}");
        assert!(
            text.split("not found on PATH")
                .nth(1)
                .unwrap()
                .to_lowercase()
                .contains("install"),
            "{text}"
        );
    }
}

#[test]
fn test_nvim_commands() {
    for source in [
        None,
        Some("https://example.com/config.git"),
        Some("git@example.com:config.git"),
        Some("local"),
    ] {
        let t = with_stubs();
        let config = t.root.join("my config");
        std::fs::create_dir(&config).unwrap();
        let runner = accepting();
        let ctx = t.ctx().runner(&runner).build();
        let given = match source {
            Some("local") => Some(config.to_str().unwrap()),
            other => other,
        };
        launch_editor(&ctx, "nvim", "vast-12", "/workspace/repo", given).unwrap();
        let calls = runner.calls();
        assert_eq!(
            calls[0].argv,
            [
                "ssh",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                "vast-12",
                "bash -s"
            ]
        );
        let script = stdin_text(&calls[0].stdin);
        assert!(script.contains("sha256sum -c checksum"));
        assert!(script.contains(NVIM_VERSION));
        assert!(script.contains("arm64") && script.contains("x86_64"));
        match source {
            Some(url) if url != "local" => {
                assert!(script.contains(&format!("git clone -- {}", shell_quote(url))));
                assert!(script.contains("pull --ff-only"));
            }
            _ => assert!(!script.contains("git clone")),
        }
        if source == Some("local") {
            let local = std::fs::canonicalize(&config).unwrap();
            let expected = format!("{}/", local.display());
            assert_eq!(
                calls[2].argv,
                [
                    "rsync",
                    "-az",
                    "--",
                    expected.as_str(),
                    "vast-12:.config/nvim/"
                ]
            );
        }
        let last = calls.last().unwrap();
        let session = nvim_session("/workspace/repo");
        assert_eq!(last.argv, ["ssh", "-t", "vast-12", session.as_str()]);
        // Own the terminal, not a background process.
        assert!(!last.capture);
        assert_eq!(last.stdin, Input::Inherit);
    }
}

#[test]
fn test_safe_config_and_directory_quoting() {
    let t = with_stubs();
    let runner = accepting();
    let ctx = t.ctx().runner(&runner).build();
    let url = "https://example.com/it's;$(touch BAD).git";
    launch_editor(
        &ctx,
        "nvim",
        "vast-12",
        "/workspace/a'; touch BAD; '",
        Some(url),
    )
    .unwrap();
    let calls = runner.calls();
    assert!(stdin_text(&calls[0].stdin).contains(&shell_quote(url)));
    assert!(calls
        .last()
        .unwrap()
        .argv
        .last()
        .unwrap()
        .contains(&shell_quote("/workspace/a'; touch BAD; '")));
}

#[test]
fn test_history_only_after_success() {
    for success in [true, false] {
        let t = with_stubs();
        let runner = fake_vast(vec![instances()[0].clone()], move |cmd| {
            match cmd.program() {
                "code" => Ok(Output::status(if success { 0 } else { 1 })),
                _ => ssh_ready(cmd),
            }
        });
        let ctx = t.ctx().runner(&runner).build();
        if success {
            VastId("12".into()).edit(&ctx, Some("code"), None).unwrap();
            assert_eq!(last_editor(&t.env()).as_deref(), Some("code"));
            VastId("12".into()).edit(&ctx, None, None).unwrap();
            assert_eq!(recent_picks(&t.env(), "editor"), ["code", "code"]);
        } else {
            assert!(VastId("12".into()).edit(&ctx, Some("code"), None).is_err());
            assert_eq!(last_editor(&t.env()), None);
        }
    }
}

#[test]
fn test_nvim_config_type_only_matters_for_nvim() {
    let t = with_stubs();
    let runner = fake_vast(vec![instances()[0].clone()], |_| Ok(Output::status(0)));
    let ctx = t.ctx().runner(&runner).build();
    let config = Value::Int(5);
    VastId("12".into())
        .edit(&ctx, Some("code"), Some(&config))
        .unwrap();
    let error = VastId("12".into())
        .edit(&ctx, Some("nvim"), Some(&config))
        .unwrap_err();
    assert_eq!(
        error.text(),
        "Config 'editor.nvim_config' must be a git URL or directory path string."
    );
    assert!(!runner
        .calls()
        .iter()
        .any(|c| c.argv.iter().any(|a| a == "bash -s")));
    VastId("12".into())
        .edit(&ctx, Some("nvim"), Some(&Value::Int(0)))
        .unwrap();
}

#[test]
fn test_no_history_noninteractive() {
    let t = TestEnv::new();
    let ctx = t.ctx().interactive(false).build();
    let error = VastId("12".into()).edit(&ctx, None, None).unwrap_err();
    assert!(error.text().contains("gpu edit 12 --editor code"));
}

#[test]
fn test_after_up_real_keys() {
    let cases = [
        (Some("zed"), 0, "zed"),
        (Some("code"), 1, "cursor"),
        (Some("code"), 2, "none"),
        (None, 0, "cursor"),
        (None, 1, "none"),
    ];
    for (last, choice, expected) in cases {
        let result = drive(after_up_screen(last.map(String::from)), move |keys| {
            keys.wait(on("Open workspace"));
            let mut labels = Vec::new();
            if last.is_some() {
                labels.push("Open editor".to_string());
            }
            labels.extend(["Select editor...".to_string(), "Don't open".to_string()]);
            assert_eq!(keys.labels(), labels);
            for _ in 0..choice {
                keys.press(DOWN);
            }
            keys.press(RIGHT);
            if (last.is_some() && choice == 1) || (choice == 0 && last.is_none()) {
                keys.wait(on("Editor"));
                keys.press(RIGHT);
            }
        });
        assert_eq!(
            result,
            Outcome::Done(expected.to_string()),
            "{last:?} {choice}"
        );
    }
}

#[test]
fn test_editor_real_keys() {
    for (index, (expected, _)) in EDITORS.iter().enumerate() {
        let result = drive(editor_screen(), move |keys| {
            keys.wait(on("Editor"));
            for _ in 0..index {
                keys.press(DOWN);
            }
            keys.press(RIGHT);
        });
        assert_eq!(result, Outcome::Done(expected.to_string()));
    }
}

#[test]
fn test_edit_flag_config_override_and_alias() {
    let t = with_stubs();
    std::fs::create_dir_all(t.config.join("gpu")).unwrap();
    std::fs::write(
        t.config.join("gpu").join("config.json"),
        r#"{"editor": {"nvim_config": "https://example.com/stored.git"}}"#,
    )
    .unwrap();
    let runner = fake_vast(vec![instances()[0].clone()], |cmd| {
        match strs(&cmd.argv)[..] {
            ["ssh", "-o", "BatchMode=yes", "-o", "ConnectTimeout=10", "vast-12", "bash -s"]
            | ["ssh", "-t", ..] => Ok(Output::status(0)),
            _ => ssh_ready(cmd),
        }
    });
    let bootstrap = |runner: &FakeRunner| -> String {
        let calls = runner.calls();
        let call = calls
            .iter()
            .rev()
            .find(|c| c.argv.last().map(String::as_str) == Some("bash -s"))
            .unwrap();
        stdin_text(&call.stdin).to_string()
    };
    let ctx = t.ctx().runner(&runner).build();
    let argv = [
        "edit",
        "12",
        "--editor",
        "nvim",
        "--nvim-config",
        "https://example.com/override.git",
    ];
    assert_eq!(main(&ctx, &argv), 0, "{}", err(&ctx));
    assert!(bootstrap(&runner).contains("git clone -- https://example.com/override.git"));
    let ctx = t.ctx().runner(&runner).build();
    assert_eq!(
        main(&ctx, &["edit", "12", "--editor", "nvim"]),
        0,
        "{}",
        err(&ctx)
    );
    assert!(bootstrap(&runner).contains("git clone -- https://example.com/stored.git"));
}

fn executable(path: &Path, body: &str) {
    std::fs::write(path, body).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

fn run_shell(
    shell: &str,
    args: &[&str],
    stdin: &str,
    home: &Path,
    path: &Path,
) -> std::process::Output {
    use std::io::Write;
    let mut child = Command::new(shell)
        .args(args)
        .env_clear()
        .env("HOME", home)
        .env("PATH", path)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(stdin.as_bytes())
        .unwrap();
    child.wait_with_output().unwrap()
}

/// A tool from the test machine's own PATH.
fn host_tool(name: &str) -> Option<PathBuf> {
    std::env::var("PATH")
        .ok()?
        .split(':')
        .map(|d| Path::new(d).join(name))
        .find(|p| p.is_file())
}

#[test]
fn test_nvim_session_shell() {
    for tmux in [true, false] {
        // Execute the generated shell with only fake editor binaries on PATH.
        let t = TestEnv::new();
        let bin = t.root.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        executable(&bin.join("nvim"), "#!/bin/sh\necho direct\n");
        if tmux {
            executable(&bin.join("tmux"), "#!/bin/sh\nprintf \"%s\\n\" \"$*\"\n");
        }
        let result = run_shell(
            "/bin/sh",
            &["-c", &nvim_session(t.root.to_str().unwrap())],
            "",
            &t.root,
            &bin,
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let expected = if tmux {
            "new-session -A -s gpu-nvim nvim"
        } else {
            "direct"
        };
        assert_eq!(String::from_utf8_lossy(&result.stdout).trim(), expected);
    }
}

#[test]
fn test_existing_nvim_skips_install() {
    let t = TestEnv::new();
    let bin = t.root.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    executable(&bin.join("nvim"), "#!/bin/sh\nexit 0\n");
    let result = run_shell("/bin/bash", &["-s"], &nvim_bootstrap(None), &t.root, &bin);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}

#[test]
fn test_missing_nvim_installs_only_verified_archive() {
    for (cpu, arch, valid) in [
        ("x86_64", "x86_64", true),
        ("aarch64", "arm64", true),
        ("x86_64", "x86_64", false),
    ] {
        // Real extraction and copy with a tiny local release fixture; no network or remote host.
        let t = TestEnv::new();
        let fixture = t.root.join("fixture");
        let release_bin = fixture.join(format!("nvim-linux-{arch}/bin"));
        std::fs::create_dir_all(&release_bin).unwrap();
        executable(&release_bin.join("nvim"), "#!/bin/sh\nexit 0\n");
        let archive = t.root.join("release.tar.gz");
        let status = Command::new("tar")
            .args([
                "-czf",
                archive.to_str().unwrap(),
                "-C",
                fixture.to_str().unwrap(),
                &format!("nvim-linux-{arch}"),
            ])
            .status()
            .unwrap();
        assert!(status.success());
        let digest = if valid {
            Sha256::digest(std::fs::read(&archive).unwrap())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect()
        } else {
            "0".repeat(64)
        };
        let script = if arch == "x86_64" {
            nvim_bootstrap_with(None, &digest, "")
        } else {
            nvim_bootstrap_with(None, "", &digest)
        };
        let tools = t.root.join("tools");
        std::fs::create_dir(&tools).unwrap();
        for tool in ["mktemp", "rm", "tar", "mkdir", "cp", "gzip"] {
            if let Some(real) = host_tool(tool) {
                symlink(real, tools.join(tool)).unwrap();
            }
        }
        executable(&tools.join("uname"), &format!("#!/bin/sh\necho {cpu}\n"));
        executable(
            &tools.join("curl"),
            &format!(
                "#!/bin/sh\ncp {} \"$4\"\n",
                shell_quote(archive.to_str().unwrap())
            ),
        );
        let hasher = match host_tool("sha256sum") {
            Some(path) => shell_quote(path.to_str().unwrap()),
            None => format!(
                "{} -a 256",
                shell_quote(
                    host_tool("shasum")
                        .expect("sha256sum or shasum")
                        .to_str()
                        .unwrap()
                )
            ),
        };
        executable(
            &tools.join("sha256sum"),
            &format!("#!/bin/sh\nread digest name < checksum\nset -- $({hasher} \"$name\")\n[ \"$1\" = \"$digest\" ]\n"),
        );
        let result = run_shell("/bin/bash", &["-s"], &script, &t.root, &tools);
        assert_eq!(
            result.status.success(),
            valid,
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(t.root.join(".local/bin/nvim").exists(), valid);
    }
}

#[test]
fn test_git_config_shell_is_quoted_and_updates() {
    for existing in [true, false] {
        let t = TestEnv::new();
        let tools = t.root.join(".local/bin");
        std::fs::create_dir_all(&tools).unwrap();
        executable(&tools.join("nvim"), "#!/bin/sh\nexit 0\n");
        executable(
            &tools.join("git"),
            "#!/bin/sh\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\"; done >> \"$HOME/git-calls\"\necho '---' >> \"$HOME/git-calls\"\n",
        );
        symlink(host_tool("mkdir").unwrap(), tools.join("mkdir")).unwrap();
        if existing {
            std::fs::create_dir_all(t.root.join(".config/nvim/.git")).unwrap();
        }
        let source = "https://example.com/it's;$(touch BAD).git";
        let result = run_shell(
            "/bin/bash",
            &["-s"],
            &nvim_bootstrap(Some(source)),
            &t.root,
            &tools,
        );
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let log = read(&t.root.join("git-calls"));
        let calls: Vec<Vec<&str>> = log
            .split("---\n")
            .filter(|c| !c.is_empty())
            .map(|c| c.lines().collect())
            .collect();
        let config = t.root.join(".config/nvim");
        let config = config.to_str().unwrap();
        let expected: Vec<Vec<&str>> = if existing {
            vec![
                vec!["-C", config, "remote", "set-url", "origin", source],
                vec!["-C", config, "pull", "--ff-only"],
            ]
        } else {
            vec![vec!["clone", "--", source, config]]
        };
        assert_eq!(calls, expected);
        assert!(!t.root.join("BAD").exists());
    }
}

#[test]
fn test_invalid_local_config_does_not_run_ssh() {
    let t = with_stubs();
    let ctx = t.ctx().build();
    let missing = t.root.join("missing");
    let error = launch_editor(
        &ctx,
        "nvim",
        "vast-12",
        "/workspace",
        Some(missing.to_str().unwrap()),
    )
    .unwrap_err();
    assert!(error.text().contains("existing local directory"));
}
