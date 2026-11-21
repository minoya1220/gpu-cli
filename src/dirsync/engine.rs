//! The pull engine. The instance's copy leads; the local directory mirrors it.
//!
//! Each file's last agreed state is recorded (`store::Entry`). A pull copies
//! only files whose listing on the instance differs from that record. A local
//! file edited since the record is never overwritten: it stays as it is and
//! the instance's version is saved beside it under an obvious name. Local
//! files are never deleted.
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::store::{self, Base, Entry, Link};
use super::transport::{Listing, Manifest, Remote};
use crate::ctx::Ctx;
use crate::error::{os_error, Error, Result};
use crate::run_files::{mtime_ns, secret_path};

/// Files above this are not pulled unless the caller raises the limit.
pub(crate) const DEFAULT_MAX_FILE: u64 = 256 * 1024 * 1024;

pub(crate) struct PullOptions {
    pub max_file: u64,
    /// Ask before pulling files above `max_file` instead of skipping them.
    pub ask_large: bool,
}

#[derive(Debug, Default, PartialEq)]
pub(crate) struct Conflict {
    pub path: String,
    pub saved_as: String,
}

#[derive(Debug, Default)]
pub(crate) struct Outcome {
    pub pulled: Vec<String>,
    pub bytes: u64,
    pub conflicts: Vec<Conflict>,
    /// Files above the size limit that were left on the instance.
    pub large: Vec<(String, u64)>,
    /// Files deleted on the instance; their local copies are kept.
    pub kept_deleted: Vec<String>,
    /// Listed files rsync could not copy.
    pub missing: Vec<String>,
    /// Changed files a pull leaves on the instance: ignored by git and not
    /// included, or secret-looking.
    pub filtered: Vec<String>,
}

impl Outcome {
    /// Nothing to report: the directory was already in sync.
    pub fn quiet(&self) -> bool {
        self.pulled.is_empty()
            && self.conflicts.is_empty()
            && self.large.is_empty()
            && self.missing.is_empty()
            && self.kept_deleted.is_empty()
    }
}

pub(crate) fn human(bytes: u64) -> String {
    let units = ["B", "K", "M", "G", "T"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < units.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes}B")
    } else {
        format!("{value:.1}{}", units[unit])
    }
}

fn hash_file(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 16];
    loop {
        let n = file.read(&mut buffer).ok()?;
        if n == 0 {
            break;
        }
        hasher.update(&buffer[..n]);
    }
    Some(
        hasher
            .finalize()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
    )
}

pub(crate) fn local_stat(path: &Path) -> Option<(u64, i128)> {
    let meta = fs::symlink_metadata(path).ok()?;
    meta.is_file().then(|| (meta.len(), mtime_ns(&meta)))
}

fn under(path: &str, prefix: &str) -> bool {
    path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Paths git ignores in `dir`, among `paths`. Empty outside a git repository.
fn git_ignored(ctx: &Ctx, dir: &Path, paths: &[&String]) -> BTreeSet<String> {
    if paths.is_empty() {
        return BTreeSet::new();
    }
    let mut input = String::new();
    for path in paths {
        input.push_str(path);
        input.push('\0');
    }
    let argv = vec![
        "git".to_string(),
        "-C".into(),
        dir.to_string_lossy().into_owned(),
        "check-ignore".into(),
        "-z".into(),
        "--stdin".into(),
    ];
    let cmd = crate::process::Cmd::new(&argv)
        .stdin(crate::process::Input::Text(input))
        .raw_output()
        .timeout(Some(120.0));
    match ctx.run(&cmd) {
        Ok(out) if out.code == 0 => out
            .stdout
            .split('\0')
            .filter(|p| !p.is_empty())
            .map(String::from)
            .collect(),
        _ => BTreeSet::new(),
    }
}

/// Which of the changed paths a pull may bring home: not secret-looking, and
/// not ignored by git unless the link includes them.
fn visible(ctx: &Ctx, link: &Link, changed: Vec<String>) -> Vec<String> {
    let candidates: Vec<String> = changed
        .into_iter()
        .filter(|p| store::safe_relative(p) && !secret_path(Path::new(p)))
        .collect();
    let included = |p: &String| link.include.iter().any(|inc| under(p, inc));
    let to_check: Vec<&String> = candidates.iter().filter(|p| !included(p)).collect();
    let ignored = git_ignored(ctx, &link.local, &to_check);
    candidates
        .into_iter()
        .filter(|p| included(p) || !ignored.contains(p))
        .collect()
}

/// Paths in `files` that differ from the baseline (new, or another size or time).
pub(crate) fn changed_paths(base: &Base, files: &Manifest) -> Vec<String> {
    files
        .iter()
        .filter(|(path, file)| {
            base.get(*path)
                .is_none_or(|e| e.remote_size != file.size || e.remote_mtime != file.mtime)
        })
        .map(|(path, _)| path.clone())
        .collect()
}

/// How many visible files on the instance a pull would bring.
pub(crate) fn behind(ctx: &Ctx, link: &Link, base: &Base, listing: &Listing) -> usize {
    visible(ctx, link, changed_paths(base, &listing.files)).len()
}

/// The short tag naming an instance in saved-copy file names.
fn tag(id: &str) -> String {
    id.rsplit(':')
        .next()
        .unwrap_or(id)
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
        .take(24)
        .collect()
}

/// `dir/name.gpu-TAG-STAMP.ext`, unused locally.
pub(crate) fn conflict_path(local: &Path, path: &str, tag: &str, stamp: &str) -> String {
    let (parent, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (format!("{parent}/"), name),
        None => (String::new(), path),
    };
    let candidate = |n: u32| {
        let suffix = if n == 0 {
            format!("gpu-{tag}-{stamp}")
        } else {
            format!("gpu-{tag}-{stamp}-{n}")
        };
        match Path::new(name).extension().and_then(|e| e.to_str()) {
            Some(ext) => {
                let stem = &name[..name.len() - ext.len() - 1];
                format!("{parent}{stem}.{suffix}.{ext}")
            }
            None => format!("{parent}{name}.{suffix}"),
        }
    };
    (0..)
        .map(candidate)
        .find(|c| fs::symlink_metadata(local.join(c)).is_err())
        .expect("an unused name exists")
}

/// Move `staged` to `dest` atomically, creating directories; copy when the
/// two are on different file systems.
fn place(staged: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().expect("file has a parent");
    fs::create_dir_all(parent).map_err(|e| os_error(&e, parent))?;
    if fs::rename(staged, dest).is_ok() {
        return Ok(());
    }
    let name = dest.file_name().unwrap_or_default().to_string_lossy();
    let temp = parent.join(format!(".{name}.gpu-sync-{}.tmp", std::process::id()));
    let copied = fs::copy(staged, &temp)
        .and_then(|_| {
            let mtime = fs::metadata(staged)?.modified()?;
            fs::File::options()
                .write(true)
                .open(&temp)?
                .set_modified(mtime)
        })
        .and_then(|()| fs::rename(&temp, dest));
    if let Err(error) = copied {
        let _ = fs::remove_file(&temp);
        return Err(os_error(&error, dest));
    }
    Ok(())
}

/// Like `place`, but keeps `staged` for a second try when it fails.
fn place_copy(staged: &Path, dest: &Path) -> Result<()> {
    let parent = dest.parent().expect("file has a parent");
    fs::create_dir_all(parent).map_err(|e| os_error(&e, parent))?;
    fs::copy(staged, dest).map_err(|e| os_error(&e, dest))?;
    Ok(())
}

/// Whether anything between the local root and `dest` is a symlink or a
/// regular file (so the path cannot be written without leaving the tree).
fn parents_are_directories(root: &Path, dest: &Path) -> bool {
    let mut current = PathBuf::from(root);
    let relative = dest.strip_prefix(root).unwrap_or(dest);
    let Some(parent) = relative.parent() else {
        return true;
    };
    for component in parent.components() {
        current.push(component);
        match fs::symlink_metadata(&current) {
            Ok(meta) if !meta.is_dir() => return false,
            _ => {}
        }
    }
    true
}

enum Applied {
    Pulled,
    Conflict(String),
}

struct Context<'a> {
    link: &'a Link,
    tag: String,
    stamp: String,
}

fn apply(
    c: &Context,
    base: &mut Base,
    path: &str,
    remote: &super::transport::RemoteFile,
    staged: &Path,
) -> Result<Applied> {
    let dest = c.link.local.join(path);
    let staged_hash = hash_file(staged);
    let entry = |local: Option<(u64, i128)>, hash: Option<String>| Entry {
        remote_size: remote.size,
        remote_mtime: remote.mtime.clone(),
        local,
        hash,
    };
    let conflict = |base: &mut Base| -> Result<Applied> {
        let beside = conflict_path(&c.link.local, path, &c.tag, &c.stamp);
        // Beside the file, unless the path is blocked locally (a file where the
        // GPU has a directory): then in one folder for such copies.
        let saved = match place_copy(staged, &c.link.local.join(&beside)) {
            Ok(()) => beside,
            Err(_) => {
                let apart = format!("gpu-sync-conflicts/{}/{path}", c.stamp);
                place_copy(staged, &c.link.local.join(&apart))?;
                apart
            }
        };
        // The saved copy is the agreed content: the local file counts as
        // edited until it matches it, so later pulls keep protecting it.
        base.insert(path.to_string(), entry(None, staged_hash.clone()));
        Ok(Applied::Conflict(saved))
    };
    if !parents_are_directories(&c.link.local, &dest) {
        return conflict(base);
    }
    let Ok(meta) = fs::symlink_metadata(&dest) else {
        place(staged, &dest)?;
        base.insert(
            path.to_string(),
            entry(local_stat(&dest), staged_hash.clone()),
        );
        return Ok(Applied::Pulled);
    };
    if !meta.is_file() {
        return conflict(base);
    }
    let stat = (meta.len(), mtime_ns(&meta));
    let unchanged = base
        .get(path)
        .is_some_and(|e| e.local == Some(stat) || (e.hash.is_some() && e.hash == hash_file(&dest)));
    if unchanged {
        // Last look before replacing: an edit since the check above wins.
        if local_stat(&dest) == Some(stat) {
            place(staged, &dest)?;
            base.insert(
                path.to_string(),
                entry(local_stat(&dest), staged_hash.clone()),
            );
            return Ok(Applied::Pulled);
        }
        return conflict(base);
    }
    // Edited locally, but already identical to the instance's version.
    if staged_hash.is_some() && staged_hash == hash_file(&dest) {
        base.insert(path.to_string(), entry(local_stat(&dest), staged_hash));
        return Ok(Applied::Pulled);
    }
    conflict(base)
}

/// Pull what changed on the instance into the local directory and update
/// `base`. The caller saves `base` afterwards.
pub(crate) fn pull(
    ctx: &Ctx,
    remote: &Remote,
    link: &Link,
    base: &mut Base,
    listing: &Listing,
    options: &PullOptions,
) -> Result<Outcome> {
    let mut outcome = Outcome::default();
    let changed = changed_paths(base, &listing.files);
    let wanted = visible(ctx, link, changed.clone());
    let shown: BTreeSet<&String> = wanted.iter().collect();
    outcome.filtered = changed.into_iter().filter(|p| !shown.contains(p)).collect();
    let mut to_pull = Vec::new();
    let mut large = Vec::new();
    for path in wanted {
        let size = listing.files[&path].size;
        if size > options.max_file {
            large.push((path, size));
        } else {
            to_pull.push(path);
        }
    }
    if !large.is_empty() {
        let total: u64 = large.iter().map(|(_, size)| size).sum();
        let ask = options.ask_large
            && ctx.interactive()
            && crate::prompt::confirm(
                ctx,
                Some(&format!(
                    "{} files above {} ({} in all) changed on the instance. Pull them too?",
                    large.len(),
                    human(options.max_file),
                    human(total)
                )),
            )?;
        if ask {
            to_pull.extend(large.drain(..).map(|(path, _)| path));
        }
    }
    outcome.large = large;
    // Files deleted on the instance stay here; forget them so that
    // they are reported once.
    outcome.kept_deleted = base
        .keys()
        .filter(|path| !listing.files.contains_key(*path))
        .cloned()
        .collect();
    for path in &outcome.kept_deleted {
        base.remove(path);
    }
    if to_pull.is_empty() {
        return Ok(outcome);
    }
    let state = ensure_state_dir(ctx)?;
    sweep_staging(&state);
    let staging = tempfile::Builder::new()
        .prefix("pull-")
        .tempdir_in(state)
        .map_err(|e| Error::msg(format!("Cannot create a staging directory: {e}")))?;
    let list = staging.path().join("files.list");
    let area = staging.path().join("data");
    fs::create_dir(&area).map_err(|e| os_error(&e, &area))?;
    fs::write(&list, to_pull.join("\n") + "\n").map_err(|e| os_error(&e, &list))?;
    remote.pull(&link.remote, &list, &area)?;
    let context = Context {
        link,
        tag: tag(&link.id),
        stamp: ctx.clock.now().strftime("%Y%m%dT%H%M%SZ").to_string(),
    };
    for path in to_pull {
        let staged = area.join(&path);
        let Ok(meta) = fs::symlink_metadata(&staged) else {
            outcome.missing.push(path);
            continue;
        };
        if !meta.is_file() {
            outcome.missing.push(path);
            continue;
        }
        let file = &listing.files[&path];
        match apply(&context, base, &path, file, &staged)? {
            Applied::Pulled => {
                outcome.bytes += meta.len();
                outcome.pulled.push(path);
            }
            Applied::Conflict(saved_as) => outcome.conflicts.push(Conflict { path, saved_as }),
        }
    }
    Ok(outcome)
}

/// Staging folders of pulls that were interrupted (Ctrl-C skips destructors).
pub(crate) fn sweep_staging(state: &Path) {
    let Ok(entries) = fs::read_dir(state) else {
        return;
    };
    let cutoff = std::time::SystemTime::now() - std::time::Duration::from_secs(6 * 3600);
    for entry in entries.flatten() {
        let stale = entry.file_name().to_string_lossy().starts_with("pull-")
            && entry
                .metadata()
                .and_then(|m| m.modified())
                .is_ok_and(|t| t < cutoff);
        if stale {
            let _ = fs::remove_dir_all(entry.path());
        }
    }
}

fn ensure_state_dir(ctx: &Ctx) -> Result<PathBuf> {
    let dir = store::dir(&ctx.env);
    fs::create_dir_all(&dir).map_err(|e| os_error(&e, &dir))?;
    Ok(dir)
}

/// The baseline right after the initial copy: every uploaded file as listed
/// on the instance, paired with the local stat taken before archiving.
pub(crate) fn initial_base(
    listing: &Listing,
    stats: &std::collections::BTreeMap<PathBuf, (u64, i128)>,
) -> Base {
    let mut base = Base::new();
    for (path, local) in stats {
        let Some(path) = path.to_str() else { continue };
        let Some(file) = listing.files.get(path) else {
            continue;
        };
        base.insert(
            path.to_string(),
            Entry {
                remote_size: file.size,
                remote_mtime: file.mtime.clone(),
                local: Some(*local),
                hash: None,
            },
        );
    }
    base
}
