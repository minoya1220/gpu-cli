//! Job upload manifest and binary archive transport. No cloud calls here.
use std::collections::BTreeSet;
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
    pub relative: PathBuf,
    pub files: Vec<PathBuf>,
    pub bytes: u64,
    pub archive: tempfile::NamedTempFile,
}

pub(crate) fn relative(path: &Path) -> bool {
    !path.as_os_str().is_empty() && path.components().all(|c| matches!(c, Component::Normal(_)))
}

pub(crate) fn max_bytes(text: &str) -> Result<u64> {
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
        .ok_or_else(|| Error::usage("--max-upload must be positive bytes or a K/M/G size, e.g. 1G"))
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

pub(crate) fn prepare(ctx: &Ctx, cwd: &Path, cap: u64) -> Result<Upload> {
    let cwd = cwd.canonicalize()?;
    let mut files = Vec::new();
    let root = match git_file(ctx, &cwd, &["rev-parse", "--show-toplevel"])? {
        Some(mut bytes) => {
            if bytes.last() == Some(&b'\n') {
                bytes.pop();
            }
            let root = PathBuf::from(std::ffi::OsString::from_vec(bytes));
            let stage = git_file(ctx, &root, &["ls-files", "--stage", "-z"])?
                .ok_or_else(|| Error::msg("Cannot enumerate git files."))?;
            if stage.split(|b| *b == 0).any(|s| s.starts_with(b"160000 ")) {
                return Err(Error::usage(
                    "gpu run does not upload git submodules; upload a project without gitlinks.",
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
            root
        }
        None => {
            walk(&cwd, Path::new(""), &mut files)?;
            cwd.clone()
        }
    };
    files.retain(|p| !excluded(p) && fs::symlink_metadata(root.join(p)).is_ok());
    let files: Vec<_> = files
        .into_iter()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let mut sizes = Vec::new();
    let mut bytes = 0u64;
    for path in &files {
        if !relative(path) {
            return Err(Error::msg("Unsafe upload filename."));
        }
        // A tracked path below a replaced directory symlink must never read outside the root.
        let parent = root.join(path).parent().unwrap().canonicalize()?;
        if !parent.starts_with(&root) {
            return Err(Error::msg(
                "Upload directory symlink escapes the project root.",
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
    let relative = cwd
        .strip_prefix(&root)
        .map_err(|_| Error::msg("Working directory is outside git root."))?
        .to_path_buf();
    Ok(Upload {
        root,
        relative,
        files,
        bytes,
        archive,
    })
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
