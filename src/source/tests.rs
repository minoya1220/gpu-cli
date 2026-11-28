use super::*;
use crate::process::{Output, RunError};
use crate::pyjson::Value;
use crate::testing::{FakeRunner, TestEnv};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

const URL: &str = "https://github.com/acme/widgets";

fn sh(dir: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args(["-c", "user.name=t", "-c", "user.email=t@example.invalid"])
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// A bare repo with two commits on main (the first tagged v1) and a branch.
/// Returns (bare path, first sha, second sha).
fn bare(root: &Path) -> (PathBuf, String, String) {
    let work = root.join("work");
    fs::create_dir_all(&work).unwrap();
    sh(&work, &["init", "-q", "-b", "main"]);
    fs::write(work.join("train.py"), b"print('v1')").unwrap();
    fs::write(work.join("model.pt"), vec![0u8; 4096]).unwrap();
    fs::write(work.join(".env"), b"SECRET=1").unwrap();
    sh(&work, &["add", "-A"]);
    sh(&work, &["commit", "-q", "-m", "one"]);
    sh(&work, &["tag", "v1"]);
    let first = sh(&work, &["rev-parse", "HEAD"]);
    fs::write(work.join("train.py"), b"print('v2')").unwrap();
    fs::remove_file(work.join(".env")).unwrap();
    sh(&work, &["add", "-A"]);
    sh(&work, &["commit", "-q", "-m", "two"]);
    let second = sh(&work, &["rev-parse", "HEAD"]);
    sh(&work, &["checkout", "-q", "-b", "feature", &first]);
    fs::write(work.join("train.py"), b"print('feature')").unwrap();
    sh(&work, &["commit", "-q", "-am", "feat"]);
    let bare = root.join("widgets.git");
    sh(
        root,
        &[
            "clone",
            "-q",
            "--bare",
            work.to_str().unwrap(),
            bare.to_str().unwrap(),
        ],
    );
    (bare, first, second)
}

/// Runs the real `git`, serving `URL` from the local bare repo; records nothing
/// remote and never touches the network.
pub(crate) fn runner(bare: &Path) -> Arc<FakeRunner> {
    let bare = bare.to_string_lossy().into_owned();
    FakeRunner::new(move |cmd| {
        let argv: Vec<String> = cmd
            .argv
            .iter()
            .skip_while(|a| *a != "git")
            .map(|a| if a == URL { bare.clone() } else { a.clone() })
            .collect();
        if argv.is_empty() {
            return Err(RunError::NotFound);
        }
        let out = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
        if let Some(file) = &cmd.stdout_file {
            fs::write(file, &out.stdout).unwrap();
        }
        Ok(Output {
            code: out.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    })
}

/// This test's private temporary directory, where clones are made.
fn tmp(env: &TestEnv) -> PathBuf {
    let dir = env.root.join("tmp");
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn make_ctx(env: &TestEnv, runner: &Arc<FakeRunner>) -> crate::ctx::Ctx {
    let vars = env.env().with("TMPDIR", tmp(env).to_string_lossy());
    env.ctx().env(vars).runner(runner).build().for_command(true)
}

fn leftovers(env: &TestEnv) -> Vec<PathBuf> {
    fs::read_dir(tmp(env))
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("gpu-source-")
        })
        .collect()
}

#[test]
fn refs_resolve_to_full_shas_and_the_tree_is_the_commits() {
    let env = TestEnv::new();
    let (bare, first, second) = bare(&env.root);
    let runner = runner(&bare);
    let ctx = make_ctx(&env, &runner);
    let abbrev = &first[..8];
    for (git_ref, want) in [
        ("v1", &first),
        ("main", &second),
        (first.as_str(), &first),
        (abbrev, &first),
    ] {
        let got = fetch(&ctx, "acme/widgets", "", git_ref).unwrap();
        assert_eq!(&got.info.commit, want, "{git_ref}");
        assert_eq!(got.info.commit.len(), 40);
        assert_eq!(got.info.repo, URL);
        assert!(got.info.repo.starts_with("https://"));
    }
    let feature = fetch(&ctx, "acme/widgets", "", "feature").unwrap();
    assert_eq!(
        fs::read(feature.path().join("train.py")).unwrap(),
        b"print('feature')"
    );
    let one = fetch(&ctx, "widgets", "acme", "v1").unwrap();
    assert_eq!(
        fs::read(one.path().join("train.py")).unwrap(),
        b"print('v1')"
    );
}

#[test]
fn checkout_is_removed_on_drop() {
    let env = TestEnv::new();
    let (bare, _, _) = bare(&env.root);
    let runner = runner(&bare);
    let ctx = make_ctx(&env, &runner);
    let got = fetch(&ctx, "acme/widgets", "", "main").unwrap();
    let path = got.dir.path().to_path_buf();
    assert!(path.exists());
    drop(got);
    assert!(!path.exists());
}

#[test]
fn errors_name_the_unknown_ref_unknown_repo_and_no_access() {
    let env = TestEnv::new();
    let (bare, _, _) = bare(&env.root);
    let runner = runner(&bare);
    let ctx = make_ctx(&env, &runner);
    let before = leftovers(&env).len();
    let error = fetch(&ctx, "acme/widgets", "", "nope").err().unwrap();
    assert_eq!(error.exit_code(), 2);
    assert!(
        error.text().contains("Ref nope was not found"),
        "{}",
        error.text()
    );
    for bad in ["-x", "a b", ""] {
        let error = fetch(&ctx, "acme/widgets", "", bad).err().unwrap();
        assert!(error.text().contains("--ref"), "{}", error.text());
    }
    // A repository the server does not know.
    let missing = env.root.join("missing.git");
    let gone = crate::testing::FakeRunner::new({
        let missing = missing.to_string_lossy().into_owned();
        move |cmd| {
            let argv: Vec<String> = cmd
                .argv
                .iter()
                .skip_while(|a| *a != "git")
                .map(|a| if a == URL { missing.clone() } else { a.clone() })
                .collect();
            let out = Command::new(&argv[0]).args(&argv[1..]).output().unwrap();
            Ok(Output {
                code: out.status.code().unwrap_or(-1),
                stdout: String::new(),
                stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
            })
        }
    });
    let ctx = make_ctx(&env, &gone);
    let error = fetch(&ctx, "acme/widgets", "", "main").err().unwrap();
    assert!(error.text().contains("was not found"), "{}", error.text());
    // Refused credentials.
    let denied = FakeRunner::new(|_| {
        Ok(Output::failed(
            128,
            "fatal: could not read Username for 'https://github.com': terminal prompts disabled",
        ))
    });
    let ctx = make_ctx(&env, &denied);
    let error = fetch(&ctx, "acme/widgets", "", "main").err().unwrap();
    assert!(error.text().contains("No access"), "{}", error.text());
    assert_eq!(
        leftovers(&env).len(),
        before,
        "failed fetches leave no clone"
    );
    // Bad repository names never reach git.
    let ctx = make_ctx(&env, &FakeRunner::forbid());
    fetch(&ctx, "https://user:pw@host/x/y", "", "main")
        .err()
        .unwrap();
}

#[test]
fn run_uploads_the_commit_not_the_callers_folder_and_applies_the_rails() {
    let env = TestEnv::new();
    let (bare, first, second) = bare(&env.root);
    let cwd = env.root.join("project");
    fs::create_dir_all(cwd.join("gpu-runs")).unwrap();
    fs::write(cwd.join("train.py"), b"LOCAL EDIT").unwrap();
    fs::write(cwd.join("only_local.py"), b"x").unwrap();
    fs::write(cwd.join("gpu-runs/job.sh"), b"echo hi").unwrap();
    let git = runner(&bare);
    let make = |extra: &[&str]| {
        let mut options = vec!["--dry-run", "--source-repo", "acme/widgets"];
        options.extend_from_slice(extra);
        crate::testing::run_flags(&options, &["true"])
    };
    let vast = {
        let git = git.clone();
        FakeRunner::new(move |cmd| match cmd.program() {
            "vastai" if cmd.argv[1] == "search" => {
                crate::testing::ok(r#"[{"id":42,"gpu_name":"T4","dph_total":0.1,"num_gpus":1}]"#)
            }
            _ => {
                use crate::process::Runner;
                git.run(cmd)
            }
        })
    };
    let ctx = make_ctx(&env, &vast);
    let before = leftovers(&env).len();
    crate::run::run(
        &ctx,
        &make(&["--ref", "main", "--include", "gpu-runs/job.sh"]),
        &crate::config::defaults(),
        &cwd,
    )
    .unwrap();
    let result = ctx.result_snapshot();
    assert_eq!(
        result.get("files"),
        Some(&Value::List(vec![
            Value::str("gpu-runs/job.sh"),
            Value::str("model.pt"),
            Value::str("train.py")
        ]))
    );
    assert_eq!(result.get("source_commit"), Some(&Value::str(&second)));
    assert_eq!(result.get("source_repo"), Some(&Value::str(URL)));
    assert_eq!(result.get("source_ref"), Some(&Value::str("main")));
    assert_eq!(leftovers(&env).len(), before, "clone removed after upload");
    // The commit that still had a secret file: refused unless allowed.
    let ctx = make_ctx(&env, &vast);
    let error = crate::run::run(
        &ctx,
        &make(&["--ref", &first]),
        &crate::config::defaults(),
        &cwd,
    )
    .unwrap_err();
    assert_eq!(error.exit_code(), 2);
    assert!(error.text().contains(".env"), "{}", error.text());
    assert_eq!(leftovers(&env).len(), before, "clone removed after failure");
    // --max-upload applies to the fetched tree.
    let ctx = make_ctx(&env, &vast);
    let error = crate::run::run(
        &ctx,
        &make(&["--ref", "main", "--max-upload", "1K"]),
        &crate::config::defaults(),
        &cwd,
    )
    .unwrap_err();
    assert!(error.text().contains("--max-upload"), "{}", error.text());
    assert_eq!(leftovers(&env).len(), before);
    // An unknown ref fails before anything is rented.
    let ctx = make_ctx(&env, &vast);
    let error = crate::run::run(
        &ctx,
        &make(&["--ref", "nope"]),
        &crate::config::defaults(),
        &cwd,
    )
    .unwrap_err();
    assert!(error.text().contains("nope"));
}

#[test]
fn flags_are_required_together() {
    use clap::Parser;
    for argv in [
        ["gpu", "run", "--source-repo", "a/b", "--", "true"].as_slice(),
        ["gpu", "run", "--ref", "main", "--", "true"].as_slice(),
    ] {
        assert!(crate::cli::Cli::try_parse_from(argv).is_err(), "{argv:?}");
    }
}
