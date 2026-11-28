//! `--source-repo`/`--ref`: fetch a commit on the launching side so the job
//! uploads exactly that tree. Credentials stay with the local git; nothing
//! here reaches the rented machine.
use std::path::Path;

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;

/// Where a job's code came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SourceInfo {
    /// The validated clone URL (never carries credentials).
    pub repo: String,
    /// The branch, tag or commit as the caller wrote it.
    pub git_ref: String,
    /// The full commit SHA it resolved to.
    pub commit: String,
}

/// A checked-out commit in a temporary directory, removed on drop.
pub(crate) struct Checkout {
    pub dir: tempfile::TempDir,
    pub info: SourceInfo,
}

const CLONE_TIMEOUT: f64 = 900.0;

fn git(ctx: &Ctx, dir: &Path, args: &[&str], timeout: f64) -> Result<crate::process::Output> {
    // Never prompt: the terminal is not for git, and a remote server has none.
    let mut argv: Vec<String> = ["env", "GIT_TERMINAL_PROMPT=0"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if ctx.env.get("GIT_SSH_COMMAND").is_none() {
        argv.push("GIT_SSH_COMMAND=ssh -o BatchMode=yes".into());
    }
    argv.extend([
        "git".into(),
        "-c".into(),
        "core.hooksPath=/dev/null".into(),
        "-C".into(),
        dir.to_string_lossy().into_owned(),
    ]);
    argv.extend(args.iter().map(|s| s.to_string()));
    ctx.run(&Cmd::new(&argv).timeout(Some(timeout)))
        .map_err(|e| Error::msg(e.describe("git")))
}

fn clone_error(ctx: &Ctx, repo: &str, stderr: &str) -> Error {
    let lower = stderr.to_ascii_lowercase();
    let detail = ctx.safe_diagnostic(stderr.trim());
    let detail = detail.lines().last().unwrap_or("").trim().to_string();
    if [
        "authentication failed",
        "could not read username",
        "permission denied",
        "terminal prompts disabled",
        "publickey",
        "access denied",
        "403",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Error::usage(format!(
            "No access to repository {repo}: local git credentials were refused. Sign in with git or gh, or ask for access. ({detail})"
        ));
    }
    if [
        "not found",
        "does not exist",
        "not appear to be a git repository",
    ]
    .iter()
    .any(|m| lower.contains(m))
    {
        return Error::usage(format!(
            "Repository {repo} was not found, or your git credentials cannot see it. Check --source-repo. ({detail})"
        ));
    }
    Error::usage(format!("Cannot fetch repository {repo}. ({detail})"))
}

/// Clone `repo` into a temporary directory and check out `git_ref`, resolved to
/// a full commit SHA. Remote branches and tags are fetched by the clone.
pub(crate) fn fetch(ctx: &Ctx, repo: &str, owner: &str, git_ref: &str) -> Result<Checkout> {
    let (url, _) = crate::repos::resolve_repo(repo, owner)?;
    if url.is_empty() {
        return Err(Error::usage("--source-repo must name a repository."));
    }
    if git_ref.is_empty()
        || git_ref.starts_with('-')
        || git_ref.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(Error::usage(format!(
            "--ref {git_ref:?} must be a commit, branch or tag name."
        )));
    }
    let base = match ctx.env.get("TMPDIR") {
        Some(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => std::env::temp_dir(),
    };
    let dir = tempfile::Builder::new()
        .prefix("gpu-source-")
        .tempdir_in(base)?;
    let target = dir.path().join("src");
    let target_arg = target.to_string_lossy().into_owned();
    let cloned = git(
        ctx,
        dir.path(),
        &["clone", "--quiet", "--no-checkout", "--", &url, &target_arg],
        CLONE_TIMEOUT,
    )?;
    if cloned.code != 0 {
        return Err(clone_error(ctx, &url, &cloned.stderr));
    }
    let mut commit = None;
    for candidate in [
        format!("{git_ref}^{{commit}}"),
        format!("origin/{git_ref}^{{commit}}"),
    ] {
        let out = git(
            ctx,
            &target,
            &["rev-parse", "--verify", "--quiet", &candidate],
            60.0,
        )?;
        let sha = out.stdout.trim();
        if out.code == 0 && sha.len() >= 40 && sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            commit = Some(sha.to_string());
            break;
        }
    }
    let commit = commit.ok_or_else(|| {
        Error::usage(format!(
            "Ref {git_ref} was not found in {url}. Push the branch or commit first, then name a branch, tag or commit that exists there."
        ))
    })?;
    let out = git(
        ctx,
        &target,
        &["checkout", "--quiet", "--detach", &commit],
        300.0,
    )?;
    if out.code != 0 {
        return Err(Error::usage(format!(
            "Cannot check out {commit} from {url}. ({})",
            ctx.safe_diagnostic(out.stderr.trim())
        )));
    }
    Ok(Checkout {
        dir,
        info: SourceInfo {
            repo: url,
            git_ref: git_ref.to_string(),
            commit,
        },
    })
}

impl Checkout {
    pub(crate) fn path(&self) -> std::path::PathBuf {
        self.dir.path().join("src")
    }
}

#[cfg(test)]
mod tests;
