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
        super::link(ctx, ID, &self.dir(), &[], "1G", true, 5.0).unwrap()
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
    let init = |args: &[&str]| {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(w.dir())
            .output()
            .unwrap()
            .status
            .success());
    };
    init(&["init", "-q"]);
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
    super::dry_run(&ctx, &dir, "1G").unwrap();
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
