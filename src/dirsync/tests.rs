//! Directory sync against a fake instance: a temporary directory stands in
//! for the instance's `/workspace`, the real remote worker runs under bash
//! against it, and `ssh`, `rsync` and `vastai` are answered by the fake
//! runner. No provider is ever contacted.
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

use super::engine::{self, PullOptions};
use super::store::{self, Entry, Link};
use super::transport::{Listing, RemoteFile};
use crate::ctx::Ctx;
use crate::picker::testing::{DOWN, RIGHT};
use crate::process::{Cmd, Input, Output, RunError};
use crate::testing::*;

const ID: &str = "7";

struct Instance {
    root: PathBuf,
    /// The instance answers SSH.
    up: AtomicBool,
    destroyed: AtomicBool,
    /// `gpu expire ID show` reports a deadline.
    expiry_armed: AtomicBool,
    clock: AtomicU64,
}

impl Instance {
    fn remote(&self, path: &str) -> PathBuf {
        self.root.join(path.trim_start_matches('/'))
    }

    fn run_real(&self, program: &str, args: &[&str], stdin: Vec<u8>) -> Output {
        let mut child = std::process::Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut input = child.stdin.take().unwrap();
        let writer = std::thread::spawn(move || {
            let _ = input.write_all(&stdin);
        });
        let out = child.wait_with_output().unwrap();
        let _ = writer.join();
        Output {
            code: out.status.code().unwrap_or(1),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }

    fn answer(&self, cmd: &Cmd) -> Result<Output, RunError> {
        let stdin = match &cmd.stdin {
            Input::Text(text) => text.clone().into_bytes(),
            Input::File(path) => std::fs::read(path).unwrap(),
            _ => Vec::new(),
        };
        match cmd.program() {
            "vastai" => {
                let items = if self.destroyed.load(Ordering::SeqCst) {
                    "[]".to_string()
                } else {
                    r#"[{"id": 7, "gpu_name": "RTX 4090", "actual_status": "running",
                         "ssh_host": "ssh6.vast.ai", "ssh_port": 2222, "label": "no-repo"},
                        {"id": 8, "gpu_name": "RTX 4090", "actual_status": "running",
                         "ssh_host": "ssh7.vast.ai", "ssh_port": 2223, "label": "no-repo"}]"#
                        .to_string()
                };
                match cmd.argv[1].as_str() {
                    "show" => ok(&items),
                    "destroy" => {
                        self.destroyed.store(true, Ordering::SeqCst);
                        ok("destroyed")
                    }
                    other => panic!("unexpected vastai {other}"),
                }
            }
            "git" => {
                let args: Vec<&str> = cmd.argv[1..].iter().map(String::as_str).collect();
                Ok(self.run_real("git", &args, stdin))
            }
            "ssh" => {
                if !self.up.load(Ordering::SeqCst) {
                    return Ok(Output::failed(255, "ssh: connect: Connection refused"));
                }
                let command = cmd.argv.last().unwrap();
                // The expiry worker never runs here: it would act on this machine.
                if stdin.starts_with(b"GPU_EXPIRE_SOURCE=") {
                    assert_eq!(command, &format!("bash -s -- show {ID}"));
                    return ok(if self.expiry_armed.load(Ordering::SeqCst) {
                        r#"{"status": "armed", "deadline": 4102444800}"#
                    } else {
                        r#"{"status": "none"}"#
                    });
                }
                if command == "true" {
                    return ok("");
                }
                let mapped =
                    command.replace("/workspace", &format!("{}/workspace", self.root.display()));
                // The worker's blocking wait is answered at once: the fake
                // instance reports a change when its listing differs.
                if mapped.contains(" wait ") {
                    return Ok(self.fake_wait(&mapped, stdin));
                }
                Ok(self.run_real("bash", &["-c", &mapped], stdin))
            }
            "rsync" => {
                if !self.up.load(Ordering::SeqCst) {
                    return Ok(Output::failed(255, "ssh: connect: Connection refused"));
                }
                let list = cmd
                    .argv
                    .iter()
                    .find_map(|a| a.strip_prefix("--files-from="))
                    .unwrap();
                let source = &cmd.argv[cmd.argv.len() - 2];
                let source = source.split_once(':').unwrap().1;
                let dest = Path::new(cmd.argv.last().unwrap());
                for line in std::fs::read_to_string(list).unwrap().lines() {
                    let from = self.remote(source).join(line);
                    let to = dest.join(line);
                    std::fs::create_dir_all(to.parent().unwrap()).unwrap();
                    std::fs::copy(from, to).unwrap();
                }
                ok("")
            }
            // `gpu autosync-run`, which tests never start for real.
            _ if cmd.argv.get(1).map(String::as_str) == Some("autosync-run") => ok(""),
            other => panic!("unexpected command {other}: {:?}", cmd.argv),
        }
    }

    fn fake_wait(&self, mapped: &str, stdin: Vec<u8>) -> Output {
        let words: Vec<&str> = mapped.split_whitespace().collect();
        let at = words.iter().position(|w| *w == "wait").unwrap();
        let unquoted = |word: &str| word.trim_matches('\'').to_string();
        let dir = unquoted(words[at + 1]);
        let sig = unquoted(words.get(at + 3).copied().unwrap_or(""));
        let (dir, sig) = (dir.as_str(), sig.as_str());
        let listing = self.run_real("bash", &["-s", "--", "manifest", dir], stdin);
        if listing.stdout.starts_with(&format!("sig={sig}\n")) {
            ok("status=idle\n").unwrap()
        } else {
            Output::ok(format!("status=changed\n{}", listing.stdout))
        }
    }

    /// Change a remote file; its time always moves so the listing differs.
    fn write(&self, path: &str, text: &str) {
        let file = self.remote(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, text).unwrap();
        self.touch(&file);
    }

    fn touch(&self, file: &Path) {
        let n = self.clock.fetch_add(10, Ordering::SeqCst) + 10;
        let when = std::time::SystemTime::now() + std::time::Duration::from_secs(n);
        std::fs::File::options()
            .write(true)
            .open(file)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
}

struct Twin {
    t: TestEnv,
    local: tempfile::TempDir,
    instance: Arc<Instance>,
    runner: Arc<FakeRunner>,
    _remote: tempfile::TempDir,
}

impl Twin {
    fn new() -> Twin {
        let remote = tempfile::tempdir().unwrap();
        let instance = Arc::new(Instance {
            root: remote.path().to_path_buf(),
            up: AtomicBool::new(true),
            destroyed: AtomicBool::new(false),
            expiry_armed: AtomicBool::new(false),
            clock: AtomicU64::new(0),
        });
        let fake = instance.clone();
        let runner = FakeRunner::new(move |cmd| fake.answer(cmd));
        Twin {
            t: TestEnv::new(),
            local: tempfile::Builder::new().prefix("proj").tempdir().unwrap(),
            instance,
            runner,
            _remote: remote,
        }
    }

    fn ctx(&self) -> Ctx {
        self.t
            .ctx()
            .runner(&self.runner)
            .clock(&FakeClock::now_real())
            .build()
    }

    fn dir(&self) -> PathBuf {
        self.local.path().canonicalize().unwrap()
    }

    fn name(&self) -> String {
        self.dir()
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()
    }

    /// The instance-side directory of this project.
    fn project(&self, path: &str) -> String {
        format!("workspace/{}/{path}", self.name())
    }

    fn put(&self, path: &str, text: &str) {
        let file = self.dir().join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    fn local(&self, path: &str) -> String {
        std::fs::read_to_string(self.dir().join(path)).unwrap_or_else(|_| "<missing>".into())
    }

    fn remote_put(&self, path: &str, text: &str) {
        self.instance.write(&self.project(path), text);
    }

    /// Edit a local file; its time moves so the edit is visible.
    fn edit(&self, path: &str, text: &str) {
        self.put(path, text);
        let n = self.instance.clock.fetch_add(10, Ordering::SeqCst) + 10;
        std::fs::File::options()
            .write(true)
            .open(self.dir().join(path))
            .unwrap()
            .set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(n))
            .unwrap();
    }

    fn link(&self, ctx: &Ctx) -> Link {
        super::link(ctx, ID, &self.dir(), &[], "1G", true, 5.0)
            .unwrap()
            .0
    }

    fn gpu(&self, ctx: &Ctx, args: &[&str]) -> i32 {
        main(ctx, args)
    }

    /// `gpu sync --dir <project> ...`: the process's own directory is not the project.
    fn sync(&self, ctx: &Ctx, args: &[&str]) -> i32 {
        let dir = self.dir();
        let mut all = vec!["sync", "--dir", dir.to_str().unwrap()];
        all.extend_from_slice(args);
        main(ctx, &all)
    }

    fn git_init(&self) {
        assert!(std::process::Command::new("git")
            .args(["init", "-q"])
            .current_dir(self.dir())
            .output()
            .unwrap()
            .status
            .success());
    }

    fn state(&self) -> Vec<Link> {
        store::links(&self.t.env())
    }
}

/// Every file under `dir` except git's own, relative and sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    fn walk(root: &Path, dir: &Path, out: &mut Vec<String>) {
        let mut entries: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap())
            .collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                out.push(
                    path.strip_prefix(root)
                        .unwrap()
                        .to_string_lossy()
                        .into_owned(),
                );
            }
        }
    }
    walk(dir, dir, &mut out);
    out.retain(|n| !n.starts_with(".git/"));
    out
}

#[test]
fn link_copies_the_directory_up_once_and_never_sends_secrets() {
    let w = Twin::new();
    w.put("train.py", "print('hi')\n");
    w.put("src/model.py", "x = 1\n");
    w.put(".env", "TOKEN=secret\n");
    w.put("keys/id_ed25519", "private\n");
    w.put("node_modules/pkg/index.js", "junk\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let on_gpu = names(&w.instance.remote(&w.project("")));
    assert_eq!(on_gpu, ["src/model.py", "train.py"]);
    assert_eq!(link.remote, format!("/workspace/{}", w.name()));
    assert_eq!(w.state(), vec![link.clone()]);
    let base = store::load_base(&w.t.env(), &link);
    assert_eq!(
        base.keys().collect::<Vec<_>>(),
        ["src/model.py", "train.py"]
    );
    assert!(base["train.py"].local.is_some());
    let said = out(&ctx);
    assert!(said.contains("2 files"), "{said}");
    assert!(said.contains("secret-looking"), "{said}");
    assert!(said.contains(".env"), "{said}");
}

#[test]
fn link_asks_before_uploading_over_an_existing_directory() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    w.remote_put("old.py", "old\n");
    let ctx = w.ctx();
    let err = super::link(&ctx, ID, &w.dir(), &[], "1G", false, 5.0).unwrap_err();
    assert!(
        err.text().contains("already holds 1 file"),
        "{}",
        err.text()
    );
    assert!(w.state().is_empty());
    // With approval the old file is pulled home by the first sync.
    super::link(&ctx, ID, &w.dir(), &[], "1G", true, 5.0).unwrap();
    assert_eq!(w.sync(&ctx, &[ID]), 0, "{}", err_text(&ctx));
    assert_eq!(w.local("old.py"), "old\n");
}

fn err_text(ctx: &Ctx) -> String {
    err(ctx)
}

#[test]
fn modal_cannot_be_linked() {
    let w = Twin::new();
    let ctx = w.ctx();
    let code = w.gpu(
        &ctx,
        &["link", "modal:sb-1", "--dir", w.dir().to_str().unwrap()],
    );
    assert_eq!(code, 2);
    assert!(err(&ctx).contains("Modal sandboxes cannot be linked"));
    assert!(w.runner.calls().is_empty());
}

#[test]
fn sync_pulls_gpu_changes_and_reports_nothing_when_in_sync() {
    let w = Twin::new();
    w.put("train.py", "v1\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    assert_eq!(w.sync(&ctx, &[ID]), 0, "{}", err(&ctx));
    assert!(out(&ctx).contains("in sync"));
    w.remote_put("train.py", "v2 edited on the GPU\n");
    w.remote_put("results/log.txt", "epoch 1\n");
    assert_eq!(w.sync(&ctx, &["--list"]), 0, "{}", err(&ctx));
    assert!(out(&ctx).contains("behind by 2 files"));
    assert_eq!(w.sync(&ctx, &[]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("Pulled 2 files"), "{said}");
    assert_eq!(w.local("train.py"), "v2 edited on the GPU\n");
    assert_eq!(w.local("results/log.txt"), "epoch 1\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(out(&ctx).contains("in sync with"));
}

#[test]
fn a_local_edit_is_never_overwritten_and_the_gpu_version_is_kept_beside_it() {
    let w = Twin::new();
    w.put("notes.md", "base\n");
    w.put("src/app.py", "base app\n");
    w.put("quiet.py", "quiet\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    w.edit("notes.md", "my local edit\n");
    w.edit("quiet.py", "local only change\n");
    w.remote_put("notes.md", "gpu edit\n");
    w.remote_put("src/app.py", "gpu app\n");
    assert_eq!(w.sync(&ctx, &[]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("CONFLICT notes.md"), "{said}");
    assert_eq!(w.local("notes.md"), "my local edit\n");
    assert_eq!(w.local("quiet.py"), "local only change\n");
    // The untouched file simply updates.
    assert_eq!(w.local("src/app.py"), "gpu app\n");
    let saved: Vec<String> = names(&w.dir())
        .into_iter()
        .filter(|n| n.starts_with("notes.gpu-7-"))
        .collect();
    assert_eq!(saved.len(), 1, "{:?}", names(&w.dir()));
    assert!(saved[0].ends_with(".md"));
    assert_eq!(w.local(&saved[0]), "gpu edit\n");
    assert!(said.contains(&saved[0]));
    // Another pull neither reports it again nor touches either version.
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(!out(&ctx).contains("CONFLICT"));
    assert_eq!(w.local("notes.md"), "my local edit\n");
    // A later GPU edit to the still-edited file conflicts again: more copies, never an overwrite.
    w.remote_put("notes.md", "gpu edit again\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(out(&ctx).contains("CONFLICT notes.md"));
    assert_eq!(w.local("notes.md"), "my local edit\n");
    assert_eq!(
        names(&w.dir())
            .iter()
            .filter(|n| n.starts_with("notes.gpu-7-"))
            .count(),
        2
    );
}

#[test]
fn touching_a_pulled_file_or_adopting_the_gpu_version_is_not_an_edit() {
    let w = Twin::new();
    w.put("a.txt", "v1\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    w.remote_put("a.txt", "v2\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert_eq!(w.local("a.txt"), "v2\n");
    // Only the time changed: the content is what was synced, so the next pull replaces it.
    w.edit("a.txt", "v2\n");
    w.remote_put("a.txt", "v3\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(!out(&ctx).contains("CONFLICT"));
    assert_eq!(w.local("a.txt"), "v3\n");
    // After a conflict, taking the GPU version settles it for good.
    w.edit("a.txt", "mine\n");
    w.remote_put("a.txt", "theirs\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(out(&ctx).contains("CONFLICT a.txt"));
    let saved = names(&w.dir())
        .into_iter()
        .find(|n| n.starts_with("a.gpu-"))
        .unwrap();
    std::fs::copy(w.dir().join(&saved), w.dir().join("a.txt")).unwrap();
    std::fs::remove_file(w.dir().join(&saved)).unwrap();
    w.remote_put("a.txt", "theirs 2\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(!out(&ctx).contains("CONFLICT"));
    assert_eq!(w.local("a.txt"), "theirs 2\n");
}

#[test]
fn a_local_edit_that_matches_the_gpu_version_is_not_a_conflict() {
    let w = Twin::new();
    w.put("a.txt", "one\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    w.edit("a.txt", "same result\n");
    w.remote_put("a.txt", "same result\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(!out(&ctx).contains("CONFLICT"));
    assert_eq!(names(&w.dir()), ["a.txt"]);
}

#[test]
fn files_deleted_on_the_gpu_stay_local_and_are_reported_once() {
    let w = Twin::new();
    w.put("keep.py", "k\n");
    w.put("gone.py", "g\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    std::fs::remove_file(w.instance.remote(&w.project("gone.py"))).unwrap();
    assert_eq!(w.sync(&ctx, &[]), 0);
    let said = out(&ctx);
    assert!(said.contains("1 file deleted on the GPU"), "{said}");
    assert_eq!(w.local("gone.py"), "g\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert!(!out(&ctx).contains("deleted on the GPU"));
}

#[test]
fn gitignored_and_secret_files_on_the_gpu_are_not_pulled_until_included() {
    let w = Twin::new();
    w.put(".gitignore", "outputs/\n*.log\n");
    w.put("train.py", "t\n");
    w.git_init();
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    w.remote_put("outputs/model.bin", "weights\n");
    w.remote_put("run.log", "log\n");
    w.remote_put("notes.txt", "n\n");
    w.remote_put(".env", "TOKEN=1\n");
    w.remote_put("sub/credentials.json", "{}\n");
    assert_eq!(w.sync(&ctx, &[]), 0, "{}", err(&ctx));
    assert_eq!(names(&w.dir()), [".gitignore", "notes.txt", "train.py"]);
    // --include opts one ignored folder in, and the link remembers it.
    assert_eq!(w.sync(&ctx, &["--include", "outputs/"]), 0, "{}", err(&ctx));
    assert_eq!(w.local("outputs/model.bin"), "weights\n");
    assert!(!w.dir().join("run.log").exists());
    assert_eq!(w.state()[0].include, ["outputs"]);
    w.remote_put("outputs/more.bin", "more\n");
    assert_eq!(w.sync(&ctx, &[]), 0);
    assert_eq!(w.local("outputs/more.bin"), "more\n");
    assert!(!w.dir().join(".env").exists());
}

#[test]
fn very_large_files_are_skipped_with_a_warning_until_the_limit_is_raised() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    w.remote_put("big.bin", &"x".repeat(5000));
    w.remote_put("small.txt", "s\n");
    assert_eq!(w.sync(&ctx, &["--max-file", "1K"]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(
        said.contains("Not pulled: 1 file above the size limit"),
        "{said}"
    );
    assert!(said.contains("--max-file"), "{said}");
    assert!(!w.dir().join("big.bin").exists());
    assert_eq!(w.local("small.txt"), "s\n");
    assert_eq!(w.sync(&ctx, &["--max-file", "1M"]), 0);
    assert_eq!(w.local("big.bin").len(), 5000);
    assert_eq!(w.sync(&ctx, &["--max-file", "nope"]), 2);
}

#[test]
fn links_to_instances_that_no_longer_exist_are_pruned() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    assert_eq!(w.state().len(), 1);
    w.instance.destroyed.store(true, Ordering::SeqCst);
    assert_eq!(w.sync(&ctx, &["--list"]), 2);
    assert!(err(&ctx).contains("No instance is linked to this directory"));
    assert!(w.state().is_empty());
}

#[test]
fn a_provider_that_cannot_list_never_prunes() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    let failing = FakeRunner::new(|cmd| {
        assert_eq!(cmd.program(), "vastai");
        Ok(Output::failed(1, "vast is down"))
    });
    let down = w.t.ctx().runner(&failing).build();
    let _ = w.gpu(&down, &["sync", "--list"]);
    assert_eq!(w.state().len(), 1);
}

#[test]
fn several_linked_instances_need_an_id_without_a_terminal() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let first = w.link(&ctx);
    let mut second = first.clone();
    second.id = "8".into();
    store::save_link(&w.t.env(), &second).unwrap();
    store::save_base(&w.t.env(), &second, &store::load_base(&w.t.env(), &first)).unwrap();
    assert_eq!(w.sync(&ctx, &["--list"]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("7") && said.contains("8"), "{said}");
    assert_eq!(w.sync(&ctx, &[]), 2);
    assert!(err(&ctx).contains("Several linked instances"));
    assert_eq!(w.sync(&ctx, &["8"]), 0, "{}", err(&ctx));
}

#[test]
fn unlink_forgets_the_link_and_leaves_files() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    assert_eq!(
        w.gpu(&ctx, &["unlink", ID, "--dir", w.dir().to_str().unwrap()]),
        0
    );
    assert!(w.state().is_empty());
    assert_eq!(w.local("a.py"), "a\n");
    assert_eq!(w.gpu(&ctx, &["unlink", ID]), 2);
}

#[test]
fn down_pulls_the_latest_changes_before_destroying() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    w.remote_put("a.py", "final result\n");
    w.remote_put("new.txt", "n\n");
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 0, "{}", err(&ctx));
    assert_eq!(w.local("a.py"), "final result\n");
    assert_eq!(w.local("new.txt"), "n\n");
    assert!(w.instance.destroyed.load(Ordering::SeqCst));
    // The destroyed instance's link is gone.
    assert!(w.state().is_empty());
}

#[test]
fn down_refuses_to_destroy_when_the_final_pull_fails() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    w.instance.up.store(false, Ordering::SeqCst);
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 2);
    let said = err(&ctx);
    assert!(said.contains("not all in"), "{said}");
    assert!(said.contains("--discard-unsynced"), "{said}");
    assert!(!w.instance.destroyed.load(Ordering::SeqCst));
    assert_eq!(w.state().len(), 1);
    // Explicit say-so destroys anyway, with a warning.
    assert_eq!(
        w.gpu(&ctx, &["down", ID, "--yes", "--discard-unsynced"]),
        0,
        "{}",
        err(&ctx)
    );
    assert!(w.instance.destroyed.load(Ordering::SeqCst));
    assert!(err(&ctx).contains("Warning"));
}

#[test]
fn down_treats_files_it_could_not_pull_as_unsynced() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    w.remote_put("huge.bin", "");
    let huge = std::fs::File::options()
        .write(true)
        .open(w.instance.remote(&w.project("huge.bin")))
        .unwrap();
    // Sparse: it costs no disk, and it is above the pull limit.
    huge.set_len(300 * 1024 * 1024).unwrap();
    w.remote_put("small.py", "s\n");
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 2);
    assert!(err(&ctx).contains("1 file could not be pulled"));
    assert!(!w.instance.destroyed.load(Ordering::SeqCst));
    // What could be pulled was.
    assert_eq!(w.local("small.py"), "s\n");
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes", "--discard-unsynced"]), 0);
    assert!(w.instance.destroyed.load(Ordering::SeqCst));
}

#[test]
fn down_refuses_while_ignored_or_secret_gpu_changes_were_not_pulled() {
    let w = Twin::new();
    w.put(".gitignore", "outputs/\n");
    w.put("train.py", "t\n");
    w.git_init();
    let ctx = w.ctx();
    w.link(&ctx);
    w.remote_put("outputs/model.pt", "weights\n");
    w.remote_put(".env", "TOKEN=1\n");
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 2);
    let said = err(&ctx);
    assert!(
        said.contains(
            "1 file changed on the GPU but not pulled because git ignores it (outputs/model.pt)"
        ),
        "{said}"
    );
    assert!(said.contains("because it looks like a secret"), "{said}");
    assert!(
        said.contains(&format!("gpu sync {ID} --include PATH")),
        "{said}"
    );
    assert!(said.contains("--discard-unsynced"), "{said}");
    assert!(!w.instance.destroyed.load(Ordering::SeqCst));
    // Including the folder brings the checkpoint home; the secret stays, so
    // only an explicit say-so destroys.
    assert_eq!(
        w.sync(&ctx, &[ID, "--include", "outputs"]),
        0,
        "{}",
        err(&ctx)
    );
    assert_eq!(w.local("outputs/model.pt"), "weights\n");
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 2);
    let said = err(&ctx);
    assert!(
        said.contains("never pulled because it looks like a secret"),
        "{said}"
    );
    assert!(said.contains("(.env)"), "{said}");
    assert!(!said.contains("model.pt"), "{said}");
    // --include cannot bring a secret home, so it is not offered.
    assert!(!said.contains("--include"), "{said}");
    assert!(
        said.contains(&format!(
            "They are lost when the instance is destroyed. Next: gpu down {ID} --yes --discard-unsynced"
        )),
        "{said}"
    );
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes", "--discard-unsynced"]), 0);
    assert!(w.instance.destroyed.load(Ordering::SeqCst));
}

#[test]
fn down_lists_only_the_first_unpulled_paths() {
    let problem = super::unpulled_problem(
        0,
        &(1..=7).map(|n| format!("out/{n}.pt")).collect::<Vec<_>>(),
        &[],
    )
    .unwrap();
    assert!(
        problem.ends_with("(out/1.pt, out/2.pt, out/3.pt, out/4.pt, out/5.pt and 2 more)"),
        "{problem}"
    );
    assert_eq!(super::unpulled_problem(0, &[], &[]), None);
}

#[test]
fn down_of_an_unlinked_instance_makes_no_sync_calls() {
    let w = Twin::new();
    let ctx = w.ctx();
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 0, "{}", err(&ctx));
    assert!(w.runner.argvs().iter().all(|a| a[0] == "vastai"));
}

#[test]
fn watch_pulls_each_batch_of_gpu_saves_and_stops_when_asked() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    let remote = super::Remote::new(&ctx, "vast-7");
    let options = PullOptions {
        max_file: engine::DEFAULT_MAX_FILE,
        ask_large: false,
    };
    // First wait reports nothing; then the GPU saves twice before the next one.
    let first = remote.manifest(&link.remote).unwrap();
    w.remote_put("a.py", "saved on the gpu\n");
    w.remote_put("b.py", "b\n");
    super::watch(&ctx, &remote, &link, &options, Some(first), Some(2)).unwrap();
    assert_eq!(w.local("a.py"), "saved on the gpu\n");
    assert_eq!(w.local("b.py"), "b\n");
    let said = out(&ctx);
    assert!(said.contains("Watching"), "{said}");
    assert!(said.contains("Pulled 2 files"), "{said}");
}

#[test]
fn watch_survives_a_dropped_connection_and_gives_up_eventually() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let remote = super::Remote::new(&ctx, "vast-7");
    let options = PullOptions {
        max_file: engine::DEFAULT_MAX_FILE,
        ask_large: false,
    };
    w.instance.up.store(false, Ordering::SeqCst);
    let error = super::watch(&ctx, &remote, &link, &options, None, None).unwrap_err();
    assert!(
        error.text().contains("Cannot read the instance's files"),
        "{}",
        error.text()
    );
    assert!(err(&ctx).contains("Retrying in 10s"));
}

#[test]
fn two_syncs_of_one_link_do_not_run_at_once() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let held = store::lock(&w.t.env(), &link).unwrap().unwrap();
    assert!(store::lock(&w.t.env(), &link).unwrap().is_none());
    let remote = super::Remote::new(&ctx, "vast-7");
    let options = PullOptions {
        max_file: engine::DEFAULT_MAX_FILE,
        ask_large: false,
    };
    let error = super::pull_once(&ctx, &remote, &link, &options, None).unwrap_err();
    assert!(error.text().contains("Another gpu sync is already running"));
    drop(held);
    assert!(super::pull_once(&ctx, &remote, &link, &options, None).is_ok());
}

#[test]
fn up_dir_conflicts_with_a_repository_and_dry_run_lists_the_upload() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    w.put(".env", "T=1\n");
    let ctx = w.ctx();
    let dir = w.dir();
    let code = w.gpu(&ctx, &["up", "--dir", dir.to_str().unwrap(), "--repo", "x"]);
    assert_eq!(code, 2);
    assert!(w.runner.calls().is_empty());
    super::dry_run_report(&ctx, &dir, "1G").unwrap();
    let said = out(&ctx);
    assert!(said.contains("DRY RUN: would copy 1 file"), "{said}");
    assert!(said.contains(".env"), "{said}");
    assert!(w.runner.argvs().iter().all(|a| a[0] == "git"));
    assert_eq!(
        w.gpu(
            &ctx,
            &["up", "--dir", "/definitely/missing/dir", "--dry-run"]
        ),
        2
    );
}

#[test]
fn expiry_on_a_linked_instance_warns_about_the_missing_final_pull() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    err(&ctx);
    super::expiry_notice(&ctx, ID).unwrap();
    let said = err(&ctx);
    assert!(
        said.contains("deletes the GPU copy without pulling"),
        "{said}"
    );
    super::expiry_notice(&ctx, "99").unwrap();
    assert_eq!(err(&ctx), "");
}

#[test]
fn link_warns_when_the_instance_already_has_expiry_armed() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let dir = w.dir();
    let args = ["link", ID, "--dir", dir.to_str().unwrap(), "--yes"];
    assert_eq!(w.gpu(&ctx, &args), 0, "{}", err(&ctx));
    assert!(!err(&ctx).contains("Auto-destroy"));
    w.instance.expiry_armed.store(true, Ordering::SeqCst);
    assert_eq!(w.gpu(&ctx, &args), 0, "{}", err(&ctx));
    let said = err(&ctx);
    assert!(
        said.contains("deletes the GPU copy without pulling"),
        "{said}"
    );
}

#[test]
fn edit_opens_the_linked_directory() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    assert_eq!(super::linked_directory(&w.t.env(), ID), Some(link.remote));
    assert_eq!(super::linked_directory(&w.t.env(), "8"), None);
}

// Engine and store details that need no instance.

#[test]
fn saved_copies_get_obvious_unused_names() {
    let dir = tempfile::tempdir().unwrap();
    let name = |path| engine::conflict_path(dir.path(), path, "7", "T");
    assert_eq!(name("a.py"), "a.gpu-7-T.py");
    assert_eq!(name("src/lib/a.tar.gz"), "src/lib/a.tar.gpu-7-T.gz");
    assert_eq!(name("Makefile"), "Makefile.gpu-7-T");
    assert_eq!(name(".bashrc"), ".bashrc.gpu-7-T");
    std::fs::write(dir.path().join("a.gpu-7-T.py"), "taken").unwrap();
    assert_eq!(name("a.py"), "a.gpu-7-T-1.py");
}

#[test]
fn baseline_rows_round_trip_including_awkward_paths() {
    let t = TestEnv::new();
    let link = Link {
        id: "gcp:p:z:n".into(),
        local: PathBuf::from("/tmp/x"),
        remote: "/workspace/x".into(),
        include: vec!["outputs".into()],
        linked_at: "now".into(),
    };
    store::save_link(&t.env(), &link).unwrap();
    assert_eq!(store::links(&t.env()), vec![link.clone()]);
    assert_eq!(link.provider(), crate::provider::Provider::Gcp);
    let mut base = store::Base::new();
    base.insert(
        "a b/c d.txt".into(),
        Entry {
            remote_size: 3,
            remote_mtime: "1700000000.5".into(),
            local: Some((3, 1_700_000_000_500_000_000)),
            hash: Some("ab".into()),
        },
    );
    base.insert(
        "kept".into(),
        Entry {
            remote_size: 0,
            remote_mtime: "9".into(),
            local: None,
            hash: None,
        },
    );
    store::save_base(&t.env(), &link, &base).unwrap();
    assert_eq!(store::load_base(&t.env(), &link), base);
    store::remove_link(&t.env(), &link);
    assert!(store::links(&t.env()).is_empty());
    assert!(store::load_base(&t.env(), &link).is_empty());
}

#[test]
fn changes_are_files_whose_listing_differs_from_the_baseline() {
    let mut base = store::Base::new();
    let entry = |size, mtime: &str| Entry {
        remote_size: size,
        remote_mtime: mtime.into(),
        local: None,
        hash: None,
    };
    base.insert("same".into(), entry(1, "10"));
    base.insert("bigger".into(), entry(1, "10"));
    base.insert("newer".into(), entry(1, "10"));
    let file = |size, mtime: &str| RemoteFile {
        size,
        mtime: mtime.into(),
    };
    let mut listing = Listing::default();
    listing.files.insert("same".into(), file(1, "10"));
    listing.files.insert("bigger".into(), file(2, "10"));
    listing.files.insert("newer".into(), file(1, "11"));
    listing.files.insert("brand-new".into(), file(1, "1"));
    assert_eq!(
        engine::changed_paths(&base, &listing.files),
        ["bigger", "brand-new", "newer"]
    );
}

#[test]
fn sizes_read_for_people() {
    assert_eq!(engine::human(12), "12B");
    assert_eq!(engine::human(2048), "2.0K");
    assert_eq!(engine::human(3 * 1024 * 1024 * 1024), "3.0G");
}

#[test]
fn the_remote_directory_is_a_safe_name_under_workspace() {
    assert_eq!(
        super::remote_dir(Path::new("/home/me/my proj")),
        "/workspace/my_proj"
    );
    assert_eq!(
        super::remote_dir(Path::new("/home/me/.hidden")),
        "/workspace/hidden"
    );
    assert_eq!(super::remote_dir(Path::new("/")), "/workspace/project");
}

#[test]
fn includes_must_stay_inside_the_directory() {
    assert_eq!(
        super::normalize_includes(&["./outputs/".into(), "a/b".into()]).unwrap(),
        ["outputs", "a/b"]
    );
    for bad in ["/etc", "../x", "a/../../b", ""] {
        assert!(super::normalize_includes(&[bad.into()]).is_err(), "{bad}");
    }
}

#[test]
fn a_local_file_where_the_gpu_made_a_directory_is_kept_and_the_gpu_version_saved_apart() {
    let w = Twin::new();
    w.put("data", "a plain file\n");
    let ctx = w.ctx();
    w.link(&ctx);
    out(&ctx);
    std::fs::remove_file(w.instance.remote(&w.project("data"))).unwrap();
    w.remote_put("data/inner.txt", "inside\n");
    assert_eq!(w.sync(&ctx, &[]), 0, "{}", err(&ctx));
    assert_eq!(w.local("data"), "a plain file\n");
    let said = out(&ctx);
    assert!(said.contains("CONFLICT data/inner.txt"), "{said}");
    let apart = names(&w.dir())
        .into_iter()
        .find(|n| n.starts_with("gpu-sync-conflicts/"))
        .unwrap();
    assert!(apart.ends_with("/data/inner.txt"), "{apart}");
    assert_eq!(w.local(&apart), "inside\n");
}

#[test]
fn interrupted_pulls_leave_no_staging_folders_behind_for_long() {
    let t = TestEnv::new();
    let state = store::dir(&t.env());
    std::fs::create_dir_all(state.join("pull-old")).unwrap();
    std::fs::create_dir_all(state.join("pull-new")).unwrap();
    let old = std::time::SystemTime::now() - std::time::Duration::from_secs(7 * 3600);
    std::fs::File::open(state.join("pull-old"))
        .unwrap()
        .set_modified(old)
        .unwrap();
    engine::sweep_staging(&state);
    assert!(!state.join("pull-old").exists());
    assert!(state.join("pull-new").exists());
}

#[test]
fn a_terminal_picks_among_linked_instances_and_confirms_large_pulls() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let first = w.link(&w.ctx());
    let mut second = first.clone();
    second.id = "8".into();
    store::save_link(&w.t.env(), &second).unwrap();
    store::save_base(&w.t.env(), &second, &store::load_base(&w.t.env(), &first)).unwrap();
    w.remote_put("big.bin", &"x".repeat(5000));
    let ctx =
        w.t.ctx()
            .runner(&w.runner)
            .interactive(true)
            .pickers(&[&format!("{DOWN}{RIGHT}"), "no\r"])
            .build();
    assert_eq!(w.sync(&ctx, &["--max-file", "1K"]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("Not pulled: 1 file"), "{said}");
    assert!(!w.dir().join("big.bin").exists());
    let ctx =
        w.t.ctx()
            .runner(&w.runner)
            .interactive(true)
            .pickers(&[RIGHT, "yes\r"])
            .build();
    assert_eq!(w.sync(&ctx, &["--max-file", "1K"]), 0, "{}", err(&ctx));
    assert_eq!(w.local("big.bin").len(), 5000);
}

// Background autosync: the same pull loop, detached.

use super::autosync::Background;
use super::watch::{Loop, Stop};
use crate::pyjson::Dict;
use std::sync::Mutex;

fn options() -> PullOptions {
    PullOptions {
        max_file: engine::DEFAULT_MAX_FILE,
        ask_large: false,
    }
}

/// An autosync loop for `link`, as `gpu autosync-run` builds it.
fn autosync_loop<'a>(
    ctx: &'a Ctx,
    cfg: &'a Dict,
    link: &Link,
    options: &'a PullOptions,
    passes: Option<u32>,
) -> Loop<'a> {
    Loop {
        ctx,
        link: link.clone(),
        options,
        alias: None,
        retries: None,
        background: Some(Background::new(ctx, cfg)),
        waits: None,
        passes,
    }
}

/// Stands in for background syncers: a `gpu autosync-run` spawn takes the
/// link's lock, and `kill` releases it, so nothing real is started or signalled.
struct Syncers {
    held: Mutex<Vec<store::AutosyncLock>>,
    killed: Mutex<Vec<String>>,
}

impl Twin {
    fn runner_with(&self, syncers: &Arc<Syncers>, links: Vec<Link>) -> Arc<FakeRunner> {
        let fake = self.instance.clone();
        let syncers = syncers.clone();
        let env = self.t.env();
        FakeRunner::new(move |cmd| match cmd.program() {
            "kill" => {
                syncers.killed.lock().unwrap().push(cmd.argv.join(" "));
                syncers.held.lock().unwrap().clear();
                links.iter().for_each(|l| release_settles(&env, l));
                ok("")
            }
            _ if cmd.argv.get(1).map(String::as_str) == Some("autosync-run") => {
                let dir = Path::new(&cmd.argv[4]);
                let link = links.iter().find(|l| l.local == dir).expect("a linked dir");
                if let Some(claim) = store::autosync_claim(&env, link).unwrap() {
                    syncers.held.lock().unwrap().push(claim);
                }
                ok("")
            }
            _ => fake.answer(cmd),
        })
    }

    fn pretend_autosync_runs(&self, syncers: &Arc<Syncers>, link: &Link) {
        let claim = store::autosync_claim(&self.t.env(), link).unwrap().unwrap();
        syncers.held.lock().unwrap().push(claim);
    }
}

/// A lock is released when the last copy of its file descriptor closes, and
/// another test's `fork` may briefly hold a copy before it execs. The fake
/// clock passes no real time, so wait (really) until `link`'s lock is free.
fn release_settles(env: &crate::ctx::Env, link: &Link) {
    let started = std::time::Instant::now();
    while store::autosync_pid(env, link).is_some() && started.elapsed().as_secs() < 5 {
        std::thread::sleep(std::time::Duration::from_millis(5));
    }
}

fn syncers() -> Arc<Syncers> {
    Arc::new(Syncers {
        held: Mutex::new(Vec::new()),
        killed: Mutex::new(Vec::new()),
    })
}

#[test]
fn the_background_loop_pulls_gpu_saves_and_logs_with_times() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.remote_put("a.py", "saved on the gpu\n");
    let (cfg, options) = (Dict::new(), options());
    let stop = autosync_loop(&ctx, &cfg, &link, &options, Some(2))
        .run(None)
        .unwrap();
    assert_eq!(stop, Stop::Bound);
    assert_eq!(w.local("a.py"), "saved on the gpu\n");
    let log = out(&ctx);
    assert!(log.starts_with("20"), "{log}");
    assert!(log.contains("Z Pulled 1 file"), "{log}");
    let status = store::load_status(&w.t.env(), &link).unwrap();
    assert_eq!(status.state, "syncing");
    assert_eq!(status.last_pull_files, 1);
    assert!(!status.last_pull_at.is_empty());
    assert!(status.errors.is_empty());
}

#[test]
fn a_local_edit_is_still_never_overwritten_by_the_background_loop() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.edit("a.py", "my edit\n");
    w.remote_put("a.py", "gpu edit\n");
    let (cfg, options) = (Dict::new(), options());
    autosync_loop(&ctx, &cfg, &link, &options, Some(1))
        .run(None)
        .unwrap();
    assert_eq!(w.local("a.py"), "my edit\n");
    let log = out(&ctx);
    assert!(log.contains("CONFLICT a.py"), "{log}");
    let saved = names(&w.dir())
        .into_iter()
        .find(|n| n.starts_with("a.gpu-"))
        .expect("the GPU version is saved beside it");
    assert_eq!(w.local(&saved), "gpu edit\n");
}

#[test]
fn a_dropped_connection_is_retried_with_backoff_and_logged_once() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.remote_put("a.py", "while it was away\n");
    // The instance stops answering, then comes back after a few attempts.
    let fake = w.instance.clone();
    let probes = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "ssh" && probes.fetch_add(1, Ordering::SeqCst) == 0 {
            fake.up.store(false, Ordering::SeqCst);
        }
        if cmd.program() == "ssh" && probes.load(Ordering::SeqCst) > 40 {
            fake.up.store(true, Ordering::SeqCst);
        }
        fake.answer(cmd)
    });
    let clock = FakeClock::now_real();
    let ctx = w.t.ctx().runner(&runner).clock(&clock).build();
    let (cfg, options) = (Dict::new(), options());
    let mut syncing = autosync_loop(&ctx, &cfg, &link, &options, Some(4));
    syncing.run(None).unwrap();
    let log = out(&ctx);
    assert_eq!(log.matches("Sync paused").count(), 1, "{log}");
    assert!(log.contains("Retrying in 10s"), "{log}");
    assert!(log.contains("Connection restored after"), "{log}");
    assert_eq!(w.local("a.py"), "while it was away\n");
    let status = store::load_status(&w.t.env(), &link).unwrap();
    assert_eq!(status.state, "syncing");
    assert_eq!(status.errors.len(), 1, "{status:?}");
    assert!(clock.elapsed() > 100.0, "waited {}s", clock.elapsed());
}

#[test]
fn pauses_double_up_to_five_minutes() {
    let seconds = |n| super::watch::pause_after(n, true);
    assert_eq!(
        (1..=8).map(seconds).collect::<Vec<_>>(),
        [10.0, 20.0, 40.0, 80.0, 160.0, 300.0, 300.0, 300.0]
    );
    // A terminal's --watch keeps its steady pause.
    assert_eq!(super::watch::pause_after(5, false), 10.0);
}

#[test]
fn the_background_loop_stops_by_itself_when_the_instance_is_destroyed() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.instance.up.store(false, Ordering::SeqCst);
    w.instance.destroyed.store(true, Ordering::SeqCst);
    let (cfg, options) = (Dict::new(), options());
    let stop = autosync_loop(&ctx, &cfg, &link, &options, None)
        .run(None)
        .unwrap();
    assert_eq!(stop, Stop::InstanceGone);
    assert_eq!(w.state(), Vec::<Link>::new());
    assert!(store::load_status(&w.t.env(), &link).is_none());
    assert!(out(&ctx).contains("Autosync stopped: the instance no longer exists."));
}

#[test]
fn the_background_loop_keeps_trying_while_the_provider_cannot_say() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    w.instance.up.store(false, Ordering::SeqCst);
    let fake = w.instance.clone();
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "vastai" && cmd.argv[1] == "show" {
            return Ok(Output::failed(1, "rate limited"));
        }
        fake.answer(cmd)
    });
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    let (cfg, options) = (Dict::new(), options());
    let stop = autosync_loop(&ctx, &cfg, &link, &options, Some(3))
        .run(None)
        .unwrap();
    assert_eq!(stop, Stop::Bound);
    assert_eq!(w.state(), vec![link]);
}

#[test]
fn the_background_loop_stops_by_itself_when_the_link_is_removed() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    let (fake, env, gone) = (w.instance.clone(), w.t.env(), link.clone());
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "ssh" && cmd.argv.last().unwrap().contains(" wait ") {
            store::remove_link(&env, &gone);
        }
        fake.answer(cmd)
    });
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    let (cfg, options) = (Dict::new(), options());
    let stop = autosync_loop(&ctx, &cfg, &link, &options, None)
        .run(None)
        .unwrap();
    assert_eq!(stop, Stop::LinkRemoved);
    assert!(store::load_status(&w.t.env(), &link).is_none());
}

#[test]
fn the_background_loop_follows_a_changed_include_instead_of_stopping() {
    let w = Twin::new();
    w.put(".gitignore", "outputs/\n");
    w.put("train.py", "t\n");
    w.git_init();
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.remote_put("outputs/model.bin", "weights\n");
    let (fake, env, linked) = (w.instance.clone(), w.t.env(), link.clone());
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "ssh" && cmd.argv.last().unwrap().contains(" wait ") {
            let mut changed = linked.clone();
            changed.include = vec!["outputs".into()];
            changed.linked_at = "later".into();
            store::save_link(&env, &changed).unwrap();
        }
        fake.answer(cmd)
    });
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    let (cfg, options) = (Dict::new(), options());
    let stop = autosync_loop(&ctx, &cfg, &link, &options, Some(4))
        .run(None)
        .unwrap();
    assert_eq!(stop, Stop::Bound);
    assert_eq!(w.local("outputs/model.bin"), "weights\n");
    let status = store::load_status(&w.t.env(), &link).unwrap();
    assert_eq!(status.state, "syncing");
}

#[test]
fn the_detached_syncer_claims_its_link_stops_on_unlink_and_never_doubles_up() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    let dir = link.local.to_string_lossy().into_owned();
    let flags = crate::cli::AutosyncRunFlags {
        id: ID.into(),
        dir: dir.clone(),
        max_file: "256M".into(),
    };
    let cfg = Dict::new();
    // One already runs: this one says so and leaves.
    let held = store::autosync_claim(&w.t.env(), &link).unwrap().unwrap();
    let before = w.runner.argvs().len();
    super::autosync_run(&ctx, &cfg, &flags).unwrap();
    assert!(out(&ctx).contains("already runs"));
    assert_eq!(w.runner.argvs().len(), before);
    drop(held);
    release_settles(&w.t.env(), &link);
    // A link that does not exist is refused.
    let missing = crate::cli::AutosyncRunFlags {
        id: "99".into(),
        ..flags.clone()
    };
    assert!(super::autosync_run(&ctx, &cfg, &missing).is_err());
    // Removing the link while it runs ends it, and the lock is released.
    let (fake, env, gone) = (w.instance.clone(), w.t.env(), link.clone());
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "ssh" && cmd.argv.last().unwrap().contains(" wait ") {
            store::remove_link(&env, &gone);
        }
        fake.answer(cmd)
    });
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    super::autosync_run(&ctx, &cfg, &flags).unwrap();
    let log = out(&ctx);
    assert!(log.contains("Autosync started:"), "{log}");
    assert!(
        log.contains("Autosync stopped: the link was removed."),
        "{log}"
    );
    release_settles(&w.t.env(), &link);
    assert!(store::autosync_pid(&w.t.env(), &link).is_none());
}

#[test]
fn autosync_pulls_once_more_shortly_before_the_auto_destroy_deadline() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.instance.expiry_armed.store(true, Ordering::SeqCst);
    // The fake instance reports a deadline at 4102444800 (2100-01-01).
    let deadline = jiff::Timestamp::from_second(4_102_444_800).unwrap();
    let (cfg, options) = (Dict::new(), options());
    // Hours before it: nothing special happens.
    let early = FakeClock::at(deadline - jiff::SignedDuration::from_secs(4 * 3600));
    let ctx_early = w.t.ctx().runner(&w.runner).clock(&early).build();
    autosync_loop(&ctx_early, &cfg, &link, &options, Some(2))
        .run(None)
        .unwrap();
    assert!(!out(&ctx_early).contains("auto-destroy deadline"));
    // Two minutes before it: one more pull, announced once.
    w.remote_put("late.py", "last minute\n");
    let late = FakeClock::at(deadline - jiff::SignedDuration::from_secs(120));
    let ctx_late = w.t.ctx().runner(&w.runner).clock(&late).build();
    autosync_loop(&ctx_late, &cfg, &link, &options, Some(3))
        .run(None)
        .unwrap();
    assert_eq!(w.local("late.py"), "last minute\n");
    let log = out(&ctx_late);
    assert_eq!(
        log.matches("Pulled before the auto-destroy deadline")
            .count(),
        1,
        "{log}"
    );
    let status = store::load_status(&w.t.env(), &link).unwrap();
    assert!(!status.deadline.is_empty());
}

#[test]
fn the_pull_before_the_deadline_warns_about_what_it_could_not_bring() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    w.instance.expiry_armed.store(true, Ordering::SeqCst);
    w.remote_put("token.pem", "private\n");
    let deadline = jiff::Timestamp::from_second(4_102_444_800).unwrap();
    let late = FakeClock::at(deadline - jiff::SignedDuration::from_secs(60));
    let ctx = w.t.ctx().runner(&w.runner).clock(&late).build();
    let (cfg, options) = (Dict::new(), options());
    autosync_loop(&ctx, &cfg, &link, &options, Some(1))
        .run(None)
        .unwrap();
    let log = out(&ctx);
    assert!(
        log.contains("Pulled before the auto-destroy deadline"),
        "{log}"
    );
    assert!(
        log.contains("Warning: the latest changes on 7 are not all in")
            && log.contains("token.pem"),
        "{log}"
    );
}

#[test]
fn sync_autosync_starts_one_detached_syncer_logging_to_the_state_directory() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let setup = w.ctx();
    let link = w.link(&setup);
    let syncers = syncers();
    let runner = w.runner_with(&syncers, vec![link.clone()]);
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    assert_eq!(w.sync(&ctx, &["--autosync"]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("Autosync started for 7 (process"), "{said}");
    let log = store::log_path(&w.t.env(), &link);
    assert!(said.contains(&log.display().to_string()), "{said}");
    assert!(log.starts_with(store::dir(&w.t.env())));
    let spawned: Vec<_> = runner
        .argvs()
        .into_iter()
        .filter(|a| a.get(1).map(String::as_str) == Some("autosync-run"))
        .collect();
    assert_eq!(spawned.len(), 1);
    assert_eq!(
        spawned[0][2..],
        [
            "7",
            "--dir",
            link.local.to_str().unwrap(),
            "--max-file",
            "256M"
        ]
    );
    // Asking again while it runs starts nothing.
    assert_eq!(w.sync(&ctx, &["--autosync"]), 0, "{}", err(&ctx));
    assert!(out(&ctx).contains("Autosync already runs for"));
    let spawned_again = runner
        .argvs()
        .iter()
        .filter(|a| a.get(1).map(String::as_str) == Some("autosync-run"))
        .count();
    assert_eq!(spawned_again, 1);
    // And gpu sync shows it.
    assert_eq!(w.sync(&ctx, &["--list"]), 0, "{}", err(&ctx));
    let table = out(&ctx);
    assert!(table.contains("Autosync"), "{table}");
    assert!(table.contains("running"), "{table}");
}

#[test]
fn sync_list_shows_whether_autosync_runs_and_its_recent_errors() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    assert_eq!(w.sync(&ctx, &["--list"]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.lines().next().unwrap().ends_with("Autosync"), "{said}");
    assert!(said.lines().nth(1).unwrap().ends_with("off"), "{said}");
    // A syncer that is failing.
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    let status = store::Status {
        state: "reconnecting".into(),
        errors: vec![(
            "2026-01-01T10:00:00Z".into(),
            "SSH could not connect".into(),
        )],
        ..store::Status::default()
    };
    store::save_status(&w.t.env(), &link, &status).unwrap();
    assert_eq!(w.sync(&ctx, &["--list"]), 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("running, reconnecting"), "{said}");
    assert!(said.contains("Recent autosync errors for 7"), "{said}");
    assert!(
        said.contains("2026-01-01T10:00:00Z  SSH could not connect"),
        "{said}"
    );
    // The same for machines.
    let dir = w.dir();
    let ctx = w.ctx();
    assert_eq!(
        w.gpu(
            &ctx,
            &["--json", "sync", "--dir", dir.to_str().unwrap(), "--list"]
        ),
        0,
        "{}",
        err(&ctx)
    );
    let reply = json(&out(&ctx));
    let item = &reply
        .get("result")
        .unwrap()
        .get("instances")
        .unwrap()
        .as_list()
        .unwrap()[0];
    let auto = item.get("autosync").unwrap();
    assert_eq!(auto.get("running"), Some(&crate::pyjson::Value::Bool(true)));
    assert_eq!(
        auto.get("errors").unwrap().as_list().unwrap()[0].get("text"),
        Some(&crate::pyjson::Value::str("SSH could not connect"))
    );
}

#[test]
fn a_crashed_syncer_does_not_look_like_it_runs() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    // A status record with no process behind it.
    store::save_status(
        &w.t.env(),
        &link,
        &store::Status {
            state: "syncing".into(),
            ..store::Status::default()
        },
    )
    .unwrap();
    out(&ctx);
    assert_eq!(w.sync(&ctx, &["--list"]), 0, "{}", err(&ctx));
    assert!(out(&ctx).lines().nth(1).unwrap().ends_with("off"));
}

#[test]
fn sync_no_autosync_stops_the_syncer_even_when_the_instance_cannot_be_reached() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    store::save_status(
        &w.t.env(),
        &link,
        &store::Status {
            state: "reconnecting".into(),
            errors: vec![("t".into(), "e".into())],
            ..store::Status::default()
        },
    )
    .unwrap();
    w.instance.up.store(false, Ordering::SeqCst);
    let runner = w.runner_with(&syncers, vec![link.clone()]);
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    assert_eq!(w.sync(&ctx, &["--no-autosync"]), 0, "{}", err(&ctx));
    assert!(out(&ctx).contains("Stopped autosync of 7"));
    assert_eq!(syncers.killed.lock().unwrap().len(), 1);
    assert!(syncers.killed.lock().unwrap()[0].starts_with("kill -TERM "));
    assert!(store::autosync_pid(&w.t.env(), &link).is_none());
    let status = store::load_status(&w.t.env(), &link).unwrap();
    assert_eq!(status.state, "stopped: stopped by you");
    assert!(status.errors.is_empty());
    // Nothing to stop the second time.
    assert_eq!(w.sync(&ctx, &["--no-autosync"]), 0, "{}", err(&ctx));
    assert!(out(&ctx).contains("Autosync is not running"));
    assert_eq!(syncers.killed.lock().unwrap().len(), 1);
}

#[test]
fn a_syncer_that_ignores_the_stop_is_killed() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    // `kill -TERM` changes nothing; only `kill -KILL` releases it.
    let held = syncers.clone();
    let (env, stuck) = (w.t.env(), link.clone());
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "kill" && cmd.argv[1] == "-KILL" {
            held.held.lock().unwrap().clear();
            release_settles(&env, &stuck);
        }
        ok("")
    });
    let clock = FakeClock::now_real();
    let ctx = w.t.ctx().runner(&runner).clock(&clock).build();
    assert!(super::autosync::stop(&ctx, &link).unwrap());
    let kills: Vec<_> = runner.argvs().iter().map(|a| a[1].clone()).collect();
    assert_eq!(kills, ["-TERM", "-KILL"]);
    assert!(clock.elapsed() >= 40.0);
}

#[test]
fn down_pulls_then_destroys_then_stops_the_syncer() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    w.remote_put("a.py", "last words\n");
    let runner = w.runner_with(&syncers, vec![link.clone()]);
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    assert_eq!(w.gpu(&ctx, &["down", ID, "--yes"]), 0, "{}", err(&ctx));
    assert_eq!(w.local("a.py"), "last words\n");
    let order: Vec<String> = runner
        .argvs()
        .iter()
        .filter_map(|a| match a[0].as_str() {
            "rsync" => Some("pull".to_string()),
            "kill" => Some("stop".into()),
            "vastai" if a[1] == "destroy" => Some("destroy".into()),
            _ => None,
        })
        .collect();
    assert_eq!(order, ["pull", "destroy", "stop"]);
    assert_eq!(w.state(), Vec::<Link>::new());
}

#[test]
fn down_does_not_wait_forever_for_a_syncer_in_the_middle_of_a_pull() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    // Another process is pulling right now.
    let _busy = store::lock(&w.t.env(), &link).unwrap().unwrap();
    let clock = FakeClock::now_real();
    let ctx = w.t.ctx().runner(&w.runner).clock(&clock).build();
    let remote = super::Remote::new(&ctx, "vast-7");
    let error = super::pull_once(&ctx, &remote, &link, &options(), None).unwrap_err();
    assert!(error.text().contains("Another gpu sync is already running"));
    assert!(clock.elapsed() >= 120.0);
}

#[test]
fn a_manual_pull_waits_for_the_syncer_to_finish_its_pull() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    w.remote_put("a.py", "newer\n");
    let busy = Arc::new(Mutex::new(Some(
        store::lock(&w.t.env(), &link).unwrap().unwrap(),
    )));
    // The other pull finishes after a few waits.
    let release = busy.clone();
    let clock = FakeClock::now_real();
    let waited = clock.clone();
    let fake = w.instance.clone();
    let runner = FakeRunner::new(move |cmd| fake.answer(cmd));
    let ctx = w.t.ctx().runner(&runner).clock(&clock).build();
    std::thread::spawn(move || {
        while waited.elapsed() < 1.0 {
            std::thread::yield_now();
        }
        release.lock().unwrap().take();
    });
    let remote = super::Remote::new(&ctx, "vast-7");
    super::pull_once(&ctx, &remote, &link, &options(), None).unwrap();
    assert_eq!(w.local("a.py"), "newer\n");
}

#[test]
fn watch_in_a_terminal_is_refused_while_autosync_runs() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    out(&ctx);
    assert_eq!(w.sync(&ctx, &["--watch"]), 2);
    let said = err(&ctx);
    assert!(said.contains("Autosync already syncs"), "{said}");
    assert!(
        said.contains("would only repeat its work. Next: gpu sync 7 --no-autosync"),
        "{said}"
    );
}

#[test]
fn unlink_stops_the_syncer_first() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    let runner = w.runner_with(&syncers, vec![link.clone()]);
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    assert_eq!(
        w.gpu(&ctx, &["unlink", ID, "--dir", link.local.to_str().unwrap()]),
        0,
        "{}",
        err(&ctx)
    );
    assert_eq!(syncers.killed.lock().unwrap().len(), 1);
    assert_eq!(w.state(), Vec::<Link>::new());
}

#[test]
fn expiry_notice_says_what_autosync_does_about_the_deadline() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    err(&ctx);
    super::expiry_notice(&ctx, ID).unwrap();
    let said = err(&ctx);
    assert!(
        said.contains("deletes the GPU copy without pulling"),
        "{said}"
    );
    assert!(said.contains("gpu sync --autosync"), "{said}");
    let syncers = syncers();
    w.pretend_autosync_runs(&syncers, &link);
    super::expiry_notice(&ctx, ID).unwrap();
    let said = err(&ctx);
    assert!(said.contains("autosync runs"), "{said}");
    assert!(
        said.contains("pulls once more a few minutes before the deadline"),
        "{said}"
    );
    assert!(
        !said.contains("deletes the GPU copy without pulling"),
        "{said}"
    );
}

#[test]
fn link_autosync_links_then_starts_the_syncer() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let probe = w.ctx();
    let link = Link {
        id: ID.into(),
        local: w.dir(),
        remote: String::new(),
        include: Vec::new(),
        linked_at: String::new(),
    };
    drop(probe);
    let syncers = syncers();
    let runner = w.runner_with(&syncers, vec![link]);
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    let dir = w.dir();
    let code = w.gpu(
        &ctx,
        &[
            "link",
            ID,
            "--dir",
            dir.to_str().unwrap(),
            "--yes",
            "--autosync",
        ],
    );
    assert_eq!(code, 0, "{}", err(&ctx));
    let said = out(&ctx);
    assert!(said.contains("Linked "), "{said}");
    assert!(said.contains("Autosync started for 7"), "{said}");
    // Without the flag no syncer starts.
    let w2 = Twin::new();
    w2.put("a.py", "a\n");
    let ctx = w2.ctx();
    let dir = w2.dir();
    assert_eq!(
        w2.gpu(&ctx, &["link", ID, "--dir", dir.to_str().unwrap(), "--yes"]),
        0,
        "{}",
        err(&ctx)
    );
    assert!(w2
        .runner
        .argvs()
        .iter()
        .all(|a| a.get(1).map(String::as_str) != Some("autosync-run")));
}

fn up_flags(dir: &Path, extra: &[&str]) -> crate::cli::UpFlags {
    use clap::Parser;
    let mut argv = vec!["gpu", "up", "--dir", dir.to_str().unwrap()];
    argv.extend_from_slice(extra);
    match crate::cli::Cli::try_parse_from(argv).unwrap().command {
        Some(crate::cli::Command::Up(flags)) => flags,
        _ => unreachable!(),
    }
}

#[test]
fn up_dir_starts_autosync_from_the_flag_or_the_pickers_answer() {
    for (extra, chosen, starts) in [
        (&["--autosync"][..], false, true),
        (&[][..], true, true),
        (&[][..], false, false),
    ] {
        let w = Twin::new();
        w.put("a.py", "a\n");
        let dir = w.dir();
        let link = Link {
            id: ID.into(),
            local: dir.clone(),
            remote: String::new(),
            include: Vec::new(),
            linked_at: String::new(),
        };
        let syncers = syncers();
        let runner = w.runner_with(&syncers, vec![link]);
        let ctx =
            w.t.ctx()
                .runner(&runner)
                .clock(&FakeClock::now_real())
                .build();
        ctx.set_autosync_choice(Some(chosen));
        let flags = up_flags(&dir, extra);
        super::after_up(&ctx, &Dict::new(), &flags, ID, &dir).unwrap();
        let started = runner
            .argvs()
            .iter()
            .any(|a| a.get(1).map(String::as_str) == Some("autosync-run"));
        assert_eq!(started, starts, "{extra:?} chosen={chosen}");
    }
}

#[test]
fn a_failed_autosync_start_does_not_fail_the_launch() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let dir = w.dir();
    let fake = w.instance.clone();
    let runner = FakeRunner::new(move |cmd| {
        if cmd.argv.get(1).map(String::as_str) == Some("autosync-run") {
            return Err(RunError::NotFound);
        }
        fake.answer(cmd)
    });
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    let flags = up_flags(&dir, &["--autosync"]);
    super::after_up(&ctx, &Dict::new(), &flags, ID, &dir).unwrap();
    let said = err(&ctx);
    assert!(said.contains("Could not start autosync"), "{said}");
    assert!(said.contains("gpu sync 7 --autosync"), "{said}");
    assert_eq!(w.state().len(), 1);
}

#[test]
fn the_picker_asks_whether_to_autosync_and_remembers_the_answer() {
    let w = Twin::new();
    let dir = w.dir();
    for (keys, expected) in [("\r", true), (&format!("{DOWN}\r") as &str, false)] {
        let ctx = w.t.ctx().interactive(true).pickers(&[keys]).build();
        super::ask_autosync(&ctx, &dir).unwrap();
        assert_eq!(ctx.autosync_chosen(), expected, "{keys:?}");
        assert_eq!(ctx.ui.remaining(), 0);
    }
    // No terminal, no question.
    let ctx = w.t.ctx().build();
    super::ask_autosync(&ctx, &dir).unwrap();
    assert!(!ctx.autosync_chosen());
}

#[test]
fn up_dir_asks_about_autosync_unless_told_not_to() {
    let dir = Path::new("/project");
    assert!(super::asks_autosync(&up_flags(dir, &[])));
    for extra in ["--autosync", "--yes", "--dry-run"] {
        assert!(!super::asks_autosync(&up_flags(dir, &[extra])), "{extra}");
    }
    // Cancelling the question launches nothing.
    let w = Twin::new();
    w.put("a.py", "a\n");
    let dir = w.dir();
    let ctx =
        w.t.ctx()
            .runner(&w.runner)
            .interactive(true)
            .pickers(&[crate::picker::testing::CTRL_C])
            .build();
    let args = [
        "up",
        "--dir",
        dir.to_str().unwrap(),
        "--gpu",
        "RTX 4090",
        "--mode",
        "container",
    ];
    assert_eq!(w.gpu(&ctx, &args), 130, "{}", err(&ctx));
    assert_eq!(ctx.ui.remaining(), 0);
    assert!(w.runner.argvs().iter().all(|a| a[0] != "vastai"));
}

#[test]
fn autosync_flags_are_checked() {
    let w = Twin::new();
    let ctx = w.ctx();
    for args in [
        &["up", "--autosync", "--no-repo"][..],
        &["sync", "--watch", "--autosync"],
        &["sync", "--autosync", "--no-autosync"],
        &["sync", "--list", "--autosync"],
    ] {
        assert_eq!(w.gpu(&ctx, args), 2, "{args:?}");
    }
}

#[test]
fn log_lines_never_carry_secrets() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    out(&ctx);
    let fake = w.instance.clone();
    let runner = FakeRunner::new(move |cmd| {
        if cmd.program() == "ssh" && cmd.argv.last().unwrap().contains(" manifest ") {
            return Ok(Output::failed(
                255,
                "CONTAINER_API_KEY=hunter2hunter2 refused",
            ));
        }
        fake.answer(cmd)
    });
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    let (cfg, options) = (Dict::new(), options());
    autosync_loop(&ctx, &cfg, &link, &options, Some(1))
        .run(None)
        .unwrap();
    let log = out(&ctx) + &err(&ctx);
    assert!(log.contains("Sync paused"), "{log}");
    assert!(!log.contains("hunter2"), "{log}");
    let status = std::fs::read_to_string(
        store::dir(&w.t.env()).join(format!("{}.status", store_key(&w, &link))),
    )
    .unwrap_or_default();
    assert!(!status.contains("hunter2"), "{status}");
}

fn store_key(w: &Twin, link: &Link) -> String {
    // The status file sits beside the link record, under the same key.
    let state = store::dir(&w.t.env());
    let record = std::fs::read_dir(state)
        .unwrap()
        .flatten()
        .map(|e| e.path())
        .find(|p| p.extension().is_some_and(|e| e == "json"))
        .unwrap();
    let _ = link;
    record.file_stem().unwrap().to_string_lossy().into_owned()
}

#[test]
fn recent_errors_are_deduplicated_and_capped() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let ctx = w.ctx();
    let link = w.link(&ctx);
    let cfg = Dict::new();
    let mut background = Background::new(&ctx, &cfg);
    for n in 0..6 {
        let text = format!("error {}", n / 2);
        background.failed(&ctx, &link, &text, n + 1, 10.0).unwrap();
    }
    let status = store::load_status(&w.t.env(), &link).unwrap();
    assert_eq!(
        status
            .errors
            .iter()
            .map(|(_, t)| t.as_str())
            .collect::<Vec<_>>(),
        ["error 0", "error 1", "error 2"]
    );
    assert_eq!(status.state, "reconnecting");
}

#[test]
fn a_real_detached_process_keeps_running_and_logs_to_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let log = dir.path().join("sync.log");
    let cmd = Cmd::new(&["sh", "-c", "echo out; echo err >&2"]);
    let pid = {
        use crate::process::Runner;
        crate::process::RealRunner
            .spawn_detached(&cmd, &log)
            .unwrap()
    };
    assert!(pid > 0);
    let started = std::time::Instant::now();
    while std::fs::read_to_string(&log)
        .unwrap_or_default()
        .lines()
        .count()
        < 2
        && started.elapsed().as_secs() < 10
    {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let text = std::fs::read_to_string(&log).unwrap();
    assert!(text.contains("out") && text.contains("err"), "{text}");
}

/// A folder chosen in the picker ("From local directory...") takes the same
/// copy-up and link path as `gpu up --dir PATH`.
#[test]
fn a_directory_picked_in_the_launch_picker_is_copied_and_linked_like_up_dir() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let dir = w.dir();
    let link = Link {
        id: ID.into(),
        local: dir.clone(),
        remote: String::new(),
        include: Vec::new(),
        linked_at: String::new(),
    };
    let syncers = syncers();
    let runner = w.runner_with(&syncers, vec![link]);
    let ctx =
        w.t.ctx()
            .runner(&runner)
            .clock(&FakeClock::now_real())
            .build();
    // The picker records the folder; the flags carry no --dir.
    ctx.set_picked_dir(dir.clone());
    ctx.set_autosync_choice(Some(true));
    let mut flags = up_flags(&dir, &[]);
    flags.dir = None;
    super::finish_up(&ctx, &Dict::new(), &flags, Some(ID.into()), false).unwrap();
    assert_eq!(w.state().len(), 1, "the folder was copied up");
    let started = runner
        .argvs()
        .iter()
        .any(|a| a.get(1).map(String::as_str) == Some("autosync-run"));
    assert!(started, "the picker's autosync answer is honoured");
}

#[test]
fn a_picked_directory_dry_run_reports_the_upload_and_nothing_without_a_pick() {
    let w = Twin::new();
    w.put("a.py", "a\n");
    let dir = w.dir();
    let ctx = w.ctx();
    let mut flags = up_flags(&dir, &[]);
    flags.dir = None;
    super::finish_up(&ctx, &Dict::new(), &flags, None, true).unwrap();
    assert_eq!(out(&ctx), "");
    ctx.set_picked_dir(dir);
    super::finish_up(&ctx, &Dict::new(), &flags, None, true).unwrap();
    assert!(out(&ctx).contains("DRY RUN: would copy 1 file"));
}
