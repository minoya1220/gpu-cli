//! Job upload manifest and binary archive transport. No cloud calls here.
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::process::Cmd;

#[derive(Debug)]
pub(crate) struct Upload {
    pub root: PathBuf,
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    pub archive: tempfile::NamedTempFile,
    /// Regular files' size and modification time (ns) read before archiving,
    /// so an edit during the upload can only look newer than what was sent.
    pub stats: BTreeMap<PathBuf, (u64, i128)>,
    /// Secret-looking files left out, when the caller asked for that.
    pub skipped_secrets: Vec<PathBuf>,
}

pub(crate) fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

pub(crate) fn max_bytes(text: &str) -> Result<u64> {
    size_bytes(text, "--max-upload")
}

/// A positive byte count: plain bytes or a K/M/G size, such as `1G`.
pub(crate) fn size_bytes(text: &str, flag: &str) -> Result<u64> {
    let upper = text.to_ascii_uppercase();
    let (number, scale) = match upper.as_bytes().last() {
        Some(b'K') => (&upper[..upper.len() - 1], 1024u64),
        Some(b'M') => (&upper[..upper.len() - 1], 1024u64.pow(2)),
        Some(b'G') => (&upper[..upper.len() - 1], 1024u64.pow(3)),
        _ => (upper.as_str(), 1),
    };
    number
        .parse::<u64>()
        .ok()
        .and_then(|n| n.checked_mul(scale))
        .filter(|n| *n > 0)
        .ok_or_else(|| {
            Error::usage(format!(
                "{flag} must be positive bytes or a K/M/G size, e.g. 1G"
            ))
        })
}

pub(crate) fn secret_path(path: &Path) -> bool {
    path.components().any(|component| {
        let name = component.as_os_str().to_string_lossy().to_ascii_lowercase();
        (name.starts_with(".env")
            && ![".env.example", ".env.sample", ".env.template", ".env.dist"]
                .contains(&name.as_str()))
            || name.starts_with("id_rsa")
            || name.starts_with("id_ed25519")
            || name.ends_with(".pem")
            || name == ".netrc"
            || name == ".aws"
            || name == ".ssh"
            || (name.starts_with("credentials") && name.ends_with(".json"))
    })
}

fn excluded(path: &Path) -> bool {
    path.components()
        .any(|c| matches!(c, Component::Normal(n) if n == "gpu-runs" || n == ".git"))
}

fn walk(root: &Path, path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(root.join(path))? {
        let entry = entry?;
        let p = path.join(entry.file_name());
        if excluded(&p)
            || [".venv", "venv", "__pycache__", "node_modules", ".DS_Store"]
                .iter()
                .any(|n| entry.file_name() == *n)
        {
            continue;
        }
        if entry.file_type()?.is_dir() {
            walk(root, &p, files)?;
        } else {
            files.push(p);
        }
    }
    Ok(())
}

/// Git output is a NUL-delimited byte contract, not UTF-8 filenames.
fn git_file(ctx: &Ctx, cwd: &Path, args: &[&str]) -> Result<Option<Vec<u8>>> {
    let output = tempfile::NamedTempFile::new()?;
    let mut argv = vec![
        "git".to_string(),
        "-C".into(),
        cwd.to_string_lossy().into_owned(),
    ];
    argv.extend(args.iter().map(|s| s.to_string()));
    let result = ctx
        .run(
            &Cmd::new(&argv)
                .stdout_file(output.path())
                .timeout(Some(30.0)),
        )
        .map_err(|e| Error::msg(e.describe("git")))?;
    Ok((result.code == 0)
        .then(|| fs::read(output.path()))
        .transpose()?)
}

/// The upload is always the current directory's subtree. Inside a git repo,
/// git decides which of those files are visible (ignores apply, edits and
/// untracked files are included); elsewhere, a walk minus built-in excludes.
#[cfg(test)]
pub(crate) fn prepare(ctx: &Ctx, cwd: &Path, cap: u64) -> Result<Upload> {
    prepare_with(ctx, cwd, cap, false)
}

/// `prepare`; with `drop_secrets`, secret-looking files are left out (and
/// listed in `skipped_secrets`) instead of being the caller's problem.
pub(crate) fn prepare_with(ctx: &Ctx, cwd: &Path, cap: u64, drop_secrets: bool) -> Result<Upload> {
    prepare_including(ctx, cwd, cap, drop_secrets, &[])
}

/// Files (or trees) under `root` named by `--include`, sent even when git
/// ignores or does not track them, and even under `gpu-runs/`. Only paths
/// inside the working directory are accepted; `.git` is never sent.
fn included(root: &Path, include: &[String]) -> Result<Vec<PathBuf>> {
    let mut files = Vec::new();
    for text in include {
        let path: PathBuf = Path::new(text)
            .components()
            .filter(|c| !matches!(c, Component::CurDir))
            .collect();
        if !relative(&path) {
            return Err(Error::usage(format!(
                "--include {text:?} must be a path inside the current directory, such as gpu-runs/job.sh"
            )));
        }
        if path.components().any(|c| c.as_os_str() == ".git") {
            return Err(Error::usage(format!(
                "--include {text:?} must not be inside .git."
            )));
        }
        include_path(root, &path, text, &mut files)?;
    }
    Ok(files)
}

fn include_path(root: &Path, path: &Path, text: &str, files: &mut Vec<PathBuf>) -> Result<()> {
    let disk = root.join(path);
    let meta = fs::symlink_metadata(&disk)
        .map_err(|_| Error::usage(format!("--include {text:?} does not exist.")))?;
    // A link (or a link in a parent) must not lead outside the working directory.
    if !disk.canonicalize()?.starts_with(root) {
        return Err(Error::usage(format!(
            "--include {text:?} resolves outside the current directory."
        )));
    }
    if meta.is_dir() {
        for entry in fs::read_dir(&disk)? {
            let name = entry?.file_name();
            if name != ".git" {
                include_path(root, &path.join(name), text, files)?;
            }
        }
    } else {
        files.push(path.to_path_buf());
    }
    Ok(())
}

/// `prepare_with`, plus `include`d paths added to the git/walk selection.
pub(crate) fn prepare_including(
    ctx: &Ctx,
    cwd: &Path,
    cap: u64,
    drop_secrets: bool,
    include: &[String],
) -> Result<Upload> {
    let root = cwd.canonicalize()?;
    let mut files = Vec::new();
    if git_file(ctx, &root, &["rev-parse", "--show-toplevel"])?.is_some() {
        // Run from the current directory, ls-files lists only that subtree,
        // with paths relative to it.
        let stage = git_file(ctx, &root, &["ls-files", "--stage", "-z"])?
            .ok_or_else(|| Error::msg("Cannot enumerate git files."))?;
        if stage.split(|b| *b == 0).any(|s| s.starts_with(b"160000 ")) {
            return Err(Error::usage(
                "gpu does not upload git submodules; upload a project without gitlinks.",
            ));
        }
        let list = git_file(
            ctx,
            &root,
            &[
                "ls-files",
                "-z",
                "--cached",
                "--others",
                "--exclude-standard",
            ],
        )?
        .ok_or_else(|| Error::msg("Cannot enumerate git-visible files."))?;
        files.extend(
            list.split(|b| *b == 0)
                .filter(|s| !s.is_empty())
                .map(|s| PathBuf::from(std::ffi::OsString::from_vec(s.to_vec()))),
        );
    } else {
        walk(&root, Path::new(""), &mut files)?;
    }
    files.retain(|p| !excluded(p) && fs::symlink_metadata(root.join(p)).is_ok());
    files.extend(included(&root, include)?);
    let mut skipped_secrets = Vec::new();
    if drop_secrets {
        let (secret, rest): (Vec<_>, Vec<_>) = files.into_iter().partition(|p| secret_path(p));
        files = rest;
        skipped_secrets = secret;
    }
    let files: Vec<_> = files
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut sizes = Vec::new();
    let mut stats = BTreeMap::new();
    let mut bytes = 0u64;
    for path in &files {
        if !relative(path) {
            return Err(Error::msg("Unsafe upload filename."));
        }
        // A tracked path below a replaced directory symlink must never read outside the root.
        let parent = root.join(path).parent().unwrap().canonicalize()?;
        if !parent.starts_with(&root) {
            return Err(Error::msg(
                "Upload directory symlink escapes the current directory.",
            ));
        }
        let meta = fs::symlink_metadata(root.join(path))?;
        if !meta.is_file() && !meta.is_symlink() {
            return Err(Error::msg(
                "Upload contains a directory, socket, or special file.",
            ));
        }
        bytes = bytes
            .checked_add(meta.len())
            .ok_or_else(|| Error::msg("Upload size overflow."))?;
        if meta.is_file() {
            stats.insert(path.clone(), (meta.len(), mtime_ns(&meta)));
        }
        sizes.push((meta.len(), path));
    }
    if bytes > cap {
        sizes.sort_by_key(|a| std::cmp::Reverse(a.0));
        return Err(Error::usage(format!("Upload is {bytes} bytes, above --max-upload {cap}. Largest: {}. Ignore them in .gitignore or raise \
            --max-upload.", sizes.iter().take(5).map(|(s,p)| format!("{} ({s})", p.display())).collect::<Vec<_>>().join(", "))));
    }
    let mut archive = tempfile::NamedTempFile::new()?;
    {
        let gzip =
            flate2::write::GzEncoder::new(archive.as_file_mut(), flate2::Compression::fast());
        let mut tar = tar::Builder::new(gzip);
        tar.follow_symlinks(false);
        for path in &files {
            tar.append_path_with_name(root.join(path), path)?;
        }
        tar.into_inner()?.finish()?;
    }
    // Re-count the generated archive: a file may have grown between preflight
    // stat and append. The paid operation only sees this immutable tempfile.
    let actual = tar::Archive::new(flate2::read::GzDecoder::new(fs::File::open(
        archive.path(),
    )?))
    .entries()?
    .try_fold(0u64, |sum, e| -> Result<u64> {
        Ok(sum.saturating_add(e?.size()))
    })?;
    if actual > cap {
        return Err(Error::usage(
            "Upload grew above --max-upload while archiving; retry after files stop changing.",
        ));
    }
    bytes = actual;
    archive.as_file_mut().seek(SeekFrom::Start(0))?;
    Ok(Upload {
        root,
        files,
        bytes,
        archive,
        stats,
        skipped_secrets,
    })
}

/// Modification time in nanoseconds since the epoch.
pub(crate) fn mtime_ns(meta: &fs::Metadata) -> i128 {
    use std::os::unix::fs::MetadataExt;
    i128::from(meta.mtime()) * 1_000_000_000 + i128::from(meta.mtime_nsec())
}

/// tar's unpack_in rejects traversal and symlink-parent escapes. Restrict the
/// archive to the declared output subtree, and never materialize special files.
/// Only symlinks whose relative target stays inside the outputs are recreated;
/// other links and special entries are skipped with a warning.
pub(crate) fn unpack(ctx: &Ctx, path: &Path, dest: &Path, outputs: &Path) -> Result<u64> {
    let mut archive = tar::Archive::new(fs::File::open(path)?);
    let mut bytes = 0u64;
    for entry in archive.entries()? {
        let mut entry = entry?;
        let name = entry.path()?.into_owned();
        if !relative(&name) || !name.starts_with(outputs) {
            return Err(Error::msg(format!(
                "Results archive contains an unsafe path: {}.",
                crate::pyfmt::repr_str(&name.to_string_lossy())
            )));
        }
        let kind = entry.header().entry_type();
        let kept = kind.is_file()
            || kind.is_dir()
            || (kind.is_symlink()
                && entry
                    .link_name()?
                    .is_some_and(|target| link_inside(dest, &name, &target, outputs)));
        if !kept {
            ctx.eprintln(&format!(
                "WARNING: skipping {} (only regular files, directories and symlinks inside the outputs are downloaded).",
                name.display()
            ))?;
            continue;
        }
        bytes = bytes.saturating_add(entry.size());
        if !entry.unpack_in(dest)? {
            return Err(Error::msg("Unsafe results archive."));
        }
    }
    Ok(bytes)
}

/// Resolve against the extracted tree: earlier links are followed on disk,
/// `..` must leave an existing real directory (the link's parents are created
/// first), and existing paths are never replaced by a link, so later entries
/// cannot change what was checked.
fn link_inside(dest: &Path, name: &Path, target: &Path, outputs: &Path) -> bool {
    let parent = name.parent().unwrap_or(Path::new(""));
    if target.as_os_str().is_empty()
        || fs::symlink_metadata(dest.join(name)).is_ok()
        || fs::create_dir_all(dest.join(parent)).is_err()
    {
        return false;
    }
    resolve(dest, PathBuf::new(), parent, 0)
        .and_then(|parent| resolve(dest, parent, target, 0))
        .is_some_and(|path| path.starts_with(outputs))
}

fn resolve(dest: &Path, mut path: PathBuf, target: &Path, depth: usize) -> Option<PathBuf> {
    if depth > 32 {
        return None;
    }
    for part in target.components() {
        match part {
            Component::Normal(p) => {
                path.push(p);
                let disk = dest.join(&path);
                if fs::symlink_metadata(&disk).is_ok_and(|m| m.is_symlink()) {
                    let link = fs::read_link(&disk).ok()?;
                    path.pop();
                    path = resolve(dest, path, &link, depth + 1)?;
                }
            }
            Component::CurDir => {}
            Component::ParentDir => {
                let disk = dest.join(&path);
                if path.as_os_str().is_empty()
                    || !fs::symlink_metadata(&disk).is_ok_and(|m| m.is_dir())
                    || !path.pop()
                {
                    return None;
                }
            }
            Component::RootDir | Component::Prefix(_) => return None,
        }
    }
    Some(path)
}

pub(crate) fn tail(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)?;
    let size = file.metadata()?.len();
    file.seek(SeekFrom::Start(size.saturating_sub(4096)))?;
    let mut text = Vec::new();
    file.read_to_end(&mut text)?;
    Ok(String::from_utf8_lossy(&text).into_owned())
}

#[cfg(test)]
mod tests;
