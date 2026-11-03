//! Port of tests/test_repos.py: optional and arbitrary repositories, with no
//! network, credentials, or paid instances.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use super::cli::{bash_n, dry_run_request};
use super::picker::Gate;
use super::repo_cache::{flow, gated};
use crate::config::defaults;
use crate::error::Result;
use crate::launch::prompt_for_picks;
use crate::offers::Stats;
use crate::picker::testing::*;
use crate::picker::{Job, JobState, Outcome};
use crate::process::Output;
use crate::pyjson::Dict;
use crate::repos::{cache_path, load_cached_repos, resolve_repo, save_cached_repos, Repo};
use crate::startup::startup_script;
use crate::testing::*;
use crate::wizard::{Picks, Search};

fn tesla() -> Picks {
    Picks::new(None, None, Some("Tesla T4"))
}

fn own_repo(t: &TestEnv) -> crate::wizard::LaunchWizard {
    flow(t, Job::new(|| Ok(vec!["own-repo".to_string()])))
}

#[test]
fn test_resolve_repo() {
    let cases = [
        (
            "gpu-kernels",
            "https://github.com/minoya1220/gpu-kernels",
            "gpu-kernels",
        ),
        (
            "sgl-project/sglang",
            "https://github.com/sgl-project/sglang",
            "sglang",
        ),
        (
            "https://github.com/sgl-project/sglang.git",
            "https://github.com/sgl-project/sglang.git",
            "sglang",
        ),
        (
            "https://git.example.org:8443/team/sub/repo.git/",
            "https://git.example.org:8443/team/sub/repo.git/",
            "repo",
        ),
        (
            "git@git.example.org:team/sub/repo.git",
            "git@git.example.org:team/sub/repo.git",
            "repo",
        ),
        (
            "owner/.dotfiles",
            "https://github.com/owner/.dotfiles",
            ".dotfiles",
        ),
        ("", "", "no-repo"),
    ];
    for (repo, url, name) in cases {
        assert_eq!(
            resolve_repo(repo, "minoya1220").unwrap(),
            (url.to_string(), name.to_string()),
            "{repo}"
        );
    }
}

#[test]
fn test_unsafe_or_credential_bearing_repos_are_rejected() {
    let cases = [
        "--upload-pack=bad",
        "../escape",
        "owner/..",
        "owner/repo;id",
        "$(id)",
        "file:///tmp/repo",
        "http://host/repo",
        "ssh://git@host/repo",
        "git@host:../../repo",
        "https://host/repo;id",
        "https://host/$(id)",
        "https://host/repo'",
        "https://host/repo\n",
        "https://user:secret@host/repo",
        "https://host/repo?token=secret",
        "https://host/repo#branch",
        "https://host/.git",
        "git@host:repo`id`",
        "https://host/../repo",
    ];
    for repo in cases {
        assert!(resolve_repo(repo, "minoya1220").is_err(), "{repo:?}");
    }
}

#[test]
fn test_generated_script_new_choices() {
    let repos = [
        "",
        "sgl-project/sglang",
        "https://example.org/team/other.git",
        "git@github.com:sgl-project/sglang.git",
    ];
    for mode in ["container", "vm"] {
        for repo in repos {
            let script = startup_script(&Repo::from_arg(repo), mode, &defaults()).unwrap();
            assert!(script.contains("git config --global user.name"));
            assert!(script.contains(r#"if [ -n "${GH_TOKEN:-}" ]; then gh auth setup-git"#));
            assert!(script.contains("mkdir -p /workspace"));
            let last = if mode == "container" {
                "entrypoint.sh"
            } else {
                "touch /root/.works"
            };
            assert_eq!(script.lines().last(), Some(last));
            if repo.is_empty() {
                assert!(!script.contains("git clone") && !script.contains("uv sync"));
                assert!(
                    !script.contains("pyproject.toml") && !script.contains("/workspace/no-repo")
                );
            } else {
                let (url, name) = resolve_repo(repo, "minoya1220").unwrap();
                assert!(script.contains(&format!("git clone {url} /workspace/{name}")));
                assert!(script.contains(&format!(
                    "if [ -f /workspace/{name}/pyproject.toml ]; then uv sync --directory /workspace/{name}; fi"
                )));
            }
            let result = bash_n(&script);
            assert!(
                result.status.success(),
                "{}",
                String::from_utf8_lossy(&result.stderr)
            );
        }
    }
}

#[test]
fn test_cli_dry_run_skips_github_and_prompts() {
    let cases: [&[&str]; 5] = [
        &["--no-repo"],
        &["--repo", "sgl-project/sglang"],
        &["--repo", "https://example.org/group/repo.git"],
        &["--repo", "git@github.com:sgl-project/sglang.git"],
        &["--repo", "gpu-kernels"],
    ];
    for flags in cases {
        let t = TestEnv::new();
        let runner = FakeRunner::new(|cmd| {
            assert_eq!(
                cmd.argv[..3],
                ["vastai", "search", "offers"],
                "only read-only search allowed"
            );
            ok(
                r#"[{"id": 42, "gpu_name": "Tesla T4", "num_gpus": 1, "dph_total": 0.1, "reliability": 0.99, "verification": "verified"}]"#,
            )
        });
        let ctx = t.ctx().runner(&runner).build();
        let mut argv = vec!["up"];
        argv.extend_from_slice(flags);
        argv.extend([
            "--mode",
            "container",
            "--gpu",
            "Tesla T4",
            "--yes",
            "--dry-run",
        ]);
        assert_eq!(main(&ctx, &argv), 0, "{flags:?}: {}", err(&ctx));
        let request = dry_run_request(&out(&ctx));
        let repo = if flags[0] == "--no-repo" {
            ""
        } else {
            flags[1]
        };
        let label = &request[request.iter().position(|a| a == "--label").unwrap() + 1];
        assert_eq!(*label, resolve_repo(repo, "minoya1220").unwrap().1);
        let script = &request[request.iter().position(|a| a == "--onstart-cmd").unwrap() + 1];
        assert_eq!(
            *script,
            startup_script(&Repo::from_arg(repo), "container", &defaults()).unwrap()
        );
        assert_eq!(runner.calls().len(), 1);
        assert!(!t.history_path().exists());
    }
}

#[test]
fn test_flag_conflict_and_invalid_repo_fail_before_network() {
    let t = TestEnv::new();
    let ctx = t.ctx().build();
    assert_eq!(
        main(
            &ctx,
            &["up", "--no-repo", "--repo", "owner/repo", "--dry-run"]
        ),
        2
    );
    assert_eq!(
        main(&ctx, &["up", "--repo", "https://host/repo;id", "--dry-run"]),
        1
    );
    assert!(err(&ctx).contains("Use a bare repo name"));
}

#[test]
fn test_no_repo_usable_while_github_is_blocked() {
    let t = TestEnv::new();
    save_cached_repos(&t.env(), "minoya1220", &["own-repo".to_string()]).unwrap();
    let gate = Gate::default();
    let job = gated(&gate, Ok(vec!["own-repo"]));
    let observer = job.observer();
    let release = gate.clone();
    let result = super::wizard::run(&flow(&t, job), tesla(), move |keys| {
        keys.wait_for("the sync to run", || observer.state() == JobState::Running);
        keys.press("0");
        assert!(keys.press_until(ENTER, on("Mode")) < std::time::Duration::from_millis(500));
        keys.press_until("2", on("Checking repositories"));
        release.set();
    });
    gate.set();
    let Outcome::Done(picks) = result else {
        panic!("{result:?}")
    };
    assert_eq!(picks.repo, Some(Repo::NoRepo));
}

#[test]
fn test_url_entered_inline_and_back_preserves_choices() {
    for repo in [
        "https://github.com/sgl-project/sglang.git",
        "git@github.com:sgl-project/sglang.git",
        "sgl-project/sglang",
    ] {
        let t = TestEnv::new();
        let result = super::wizard::run(&own_repo(&t), tesla(), move |keys| {
            keys.press_until("9", on("Clone from URL"));
            keys.press(ENTER); // empty is not an implicit no-repo choice
            assert_eq!(keys.title(), "Clone from URL");
            keys.type_text("https://host/repo;id");
            keys.press(ENTER);
            assert_eq!(keys.title(), "Clone from URL");
            keys.press(BACK); // clear invalid input
            assert_eq!(keys.filter(), "");
            keys.press_until(BACK, on("Repository"));
            keys.press_until("9", on("Clone from URL"));
            keys.type_text(&format!("{repo}x"));
            keys.press(BACKSPACE);
            keys.press_until(ENTER, on("Mode"));
            keys.press_until(BACK, on("Clone from URL"));
            assert_eq!(keys.filter(), repo);
            keys.press_until(ENTER, on("Mode"));
            keys.press("1");
        });
        let expected = Picks::new(
            Some(Repo::from_arg(repo)),
            Some("container"),
            Some("Tesla T4"),
        );
        assert_eq!(result, Outcome::Done(expected));
    }
}

#[test]
fn test_github_failure_does_not_hide_static_choices() {
    let t = TestEnv::new();
    let job: Job<Vec<String>> = Job::new(|| Err("GitHub unavailable".into()));
    let observer = job.observer();
    let result = super::wizard::run(&flow(&t, job), Picks::default(), move |keys| {
        keys.wait_for("the sync to fail", || observer.state() == JobState::Failed);
        keys.wait(|s| s.labels.contains(&"No repository".to_string()));
        assert!(keys.labels().contains(&"Clone from URL...".to_string()));
        keys.type_text("Clone from");
        keys.press_until(ENTER, on("Clone from URL"));
        keys.press_until(BACK, on("Repository"));
        keys.press(BACK); // clear repository filter
        keys.press(BACK);
        keys.press(BACK); // quit: twice on the first screen
    });
    assert!(matches!(result, Outcome::Cancelled(_)));
}

#[test]
fn test_cache_contains_only_repo_names_and_no_recent_rows() {
    let t = TestEnv::new();
    save_cached_repos(
        &t.env(),
        "minoya1220",
        &[
            "own-repo".to_string(),
            "https://user:token@host/repo".to_string(),
        ],
    )
    .unwrap();
    assert_eq!(
        load_cached_repos(&t.env(), "minoya1220"),
        Some(vec!["own-repo".to_string()])
    );
    assert!(!read(&cache_path(&t.env(), "minoya1220")).contains("token"));
    let result = super::wizard::run(
        &own_repo(&t),
        Picks::new(None, Some("vm"), Some("B200")),
        |keys| {
            assert_eq!(
                keys.labels(),
                ["own-repo", "Clone from URL...", "No repository"]
            );
            keys.press("0");
        },
    );
    let Outcome::Done(picks) = result else {
        panic!("{result:?}")
    };
    assert_eq!(picks.repo, Some(Repo::NoRepo));
}

#[test]
fn test_paste_url_in_same_application() {
    let t = TestEnv::new();
    let repo = "https://github.com/sgl-project/sglang.git";
    let result = super::wizard::run(
        &own_repo(&t),
        Picks::new(None, Some("container"), Some("B200")),
        move |keys| {
            keys.type_text("Clone from");
            keys.press_until(ENTER, on("Clone from URL"));
            keys.press(&format!("\x1b[200~{repo}\x1b[201~"));
            assert_eq!(keys.filter(), repo);
            keys.press(ENTER);
        },
    );
    let Outcome::Done(picks) = result else {
        panic!("{result:?}")
    };
    assert_eq!(picks.repo, Some(Repo::from_arg(repo)));
}

struct OneOffer;

impl Search for OneOffer {
    fn offers(&self, _mode: &str, gpu: Option<&str>) -> Result<Vec<Dict>> {
        Ok(vec![dict(&format!(
            r#"{{"id": 42, "gpu_name": "{}"}}"#,
            gpu.unwrap_or_default()
        ))])
    }

    fn stats(&self, _mode: &str) -> Result<Stats> {
        Ok(Stats::new())
    }
}

#[test]
fn test_line_picker_new_choices_without_github() {
    for (answers, repo) in [
        (&["1"][..], Repo::NoRepo),
        (
            &["2", "sgl-project/sglang"],
            Repo::from_arg("sgl-project/sglang"),
        ),
    ] {
        let t = TestEnv::new();
        let runner = FakeRunner::new(|cmd| match cmd.program() {
            "gh" => Ok(Output::failed(1, "offline")),
            _ => panic!("unexpected command: {:?}", cmd.argv),
        });
        let ctx = t.ctx().runner(&runner).input(answers).build();
        let (picks, _) = prompt_for_picks(
            &ctx,
            Picks::new(None, Some("vm"), Some("B200")),
            &OneOffer,
            "minoya1220",
        )
        .unwrap();
        assert_eq!(picks.repo, Some(repo));
        assert!(err(&ctx).contains("Could not list repositories: offline"));
    }
}

#[test]
fn test_optional_repository_load_can_retry() {
    let t = TestEnv::new();
    let attempts = Arc::new(AtomicUsize::new(0));
    let counter = attempts.clone();
    let job = Job::new(move || {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            Err("offline".to_string())
        } else {
            Ok(vec!["own-repo".to_string()])
        }
    });
    let observer = job.observer();
    let result = super::wizard::run(
        &flow(&t, job),
        Picks::new(None, Some("vm"), Some("B200")),
        move |keys| {
            keys.wait_for("the load to fail", || observer.state() == JobState::Failed);
            keys.press("\x12"); // Ctrl-R
            keys.wait(|s| s.labels.contains(&"own-repo".to_string()));
            keys.type_text("own-repo");
            keys.press(ENTER);
        },
    );
    let Outcome::Done(picks) = result else {
        panic!("{result:?}")
    };
    assert_eq!(picks.repo, Some(Repo::from_arg("own-repo")));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
