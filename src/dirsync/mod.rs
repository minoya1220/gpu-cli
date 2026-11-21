//! Attach a GPU to a local-only codebase: `gpu up --dir`, `gpu link`,
//! `gpu sync`. The local directory is copied up once; afterwards the
//! instance's copy leads and `gpu sync` mirrors its changes back.
//! rsync over SSH moves the files; `engine` decides what moves and what is
//! kept as a conflict, `transport` is everything that crosses SSH.
mod engine;
mod store;
mod transport;

#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use crate::cli::{LinkFlags, SyncFlags};
use crate::ctx::Ctx;
use crate::error::{Error, Result};
use crate::provider::Provider;
use crate::pyjson::{Dict, Value};
use crate::run_files;
use crate::table::plural;
use engine::{human, Outcome, PullOptions};
use store::Link;
use transport::Remote;

/// Seconds one `wait` call may block on the instance.
const WATCH_WAIT: u32 = 30;
/// A full listing even when nothing was reported, every this many idle waits.
const BACKSTOP_EVERY: u32 = 4;
/// Consecutive failures after which `--watch` gives up.
const WATCH_RETRIES: u32 = 12;
const ONE_TIME_WAIT: f64 = 600.0;

/// The canonical local directory for `--dir`, or the current directory.
pub(crate) fn resolve_dir(path: Option<&str>) -> Result<PathBuf> {
    let dir = match path {
        Some(path) => PathBuf::from(path),
        None => std::env::current_dir()?,
    };
    let canonical = dir
        .canonicalize()
        .map_err(|_| Error::usage(format!("{} is not a directory I can read", dir.display())))?;
    if !canonical.is_dir() {
        return Err(Error::usage(format!(
            "{} is not a directory",
            dir.display()
        )));
    }
    Ok(canonical)
}

/// The twin's path on the instance: `/workspace/NAME`.
fn remote_dir(local: &Path) -> String {
    let name: String = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let name = name.trim_start_matches('.');
    format!(
        "/workspace/{}",
        if name.is_empty() { "project" } else { name }
    )
}

fn normalize_includes(paths: &[String]) -> Result<Vec<String>> {
    paths
        .iter()
        .map(|path| {
            let path = path
                .strip_prefix("./")
                .unwrap_or(path)
                .trim_end_matches('/');
            if store::safe_relative(path) {
                Ok(path.to_string())
            } else {
                Err(Error::usage(format!(
                    "--include {path:?} must be a path inside the directory, such as outputs"
                )))
            }
        })
        .collect()
}

fn running(item: &Dict) -> bool {
    item.get("actual_status")
        .and_then(Value::as_str)
        .is_some_and(|s| {
            matches!(
                s.to_ascii_lowercase().as_str(),
                "running" | "ready" | "active"
            )
        })
}

/// The instance ID of a listing row (Vast's are numbers).
fn id_of(item: &Dict) -> Option<String> {
    item.get("id").map(crate::pyfmt::py_str)
}

fn refuse_modal(id: &str) -> Result<()> {
    if Provider::from_key(id.split(':').next().unwrap_or("")) == Some(Provider::Modal) {
        return Err(Error::usage(
            "Modal sandboxes cannot be linked to a local directory: there is no SSH to sync over. Use gpu cp, or gpu run's outputs.",
        ));
    }
    Ok(())
}

/// Local checks before anything is rented: the directory exists and its
/// upload fits. Also what a dry run prints.
pub(crate) fn preflight(ctx: &Ctx, dir: &Path, max_upload: &str) -> Result<run_files::Upload> {
    let cap = run_files::size_bytes(max_upload, "--max-upload")?;
    run_files::prepare_with(ctx, dir, cap, true)
}

fn describe_upload(upload: &run_files::Upload) -> String {
    let mut text = format!(
        "{} ({})",
        plural(upload.files.len(), "file"),
        human(upload.bytes)
    );
    if !upload.skipped_secrets.is_empty() {
        text.push_str(&format!(
            "; {} secret-looking {} never sent: {}",
            upload.skipped_secrets.len(),
            crate::table::noun(upload.skipped_secrets.len(), "file"),
            upload
                .skipped_secrets
                .iter()
                .take(5)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    text
}

/// What `gpu up --dir --dry-run` reports instead of renting.
pub(crate) fn dry_run(ctx: &Ctx, dir: &Path, max_upload: &str) -> Result<()> {
    let upload = preflight(ctx, dir, max_upload)?;
    ctx.result("dir", Value::str(dir.to_string_lossy()));
    ctx.result("uploaded_files", Value::Int(upload.files.len() as i64));
    ctx.println(&format!(
        "DRY RUN: would copy {} from {} to {} once, then mirror the GPU copy back (gpu sync).",
        describe_upload(&upload),
        dir.display(),
        remote_dir(dir)
    ))
}

/// Copy `dir` up to the instance once and record the link.
pub(crate) fn link(
    ctx: &Ctx,
    id: &str,
    dir: &Path,
    include: &[String],
    max_upload: &str,
    yes: bool,
    timeout: f64,
) -> Result<(Link, String)> {
    refuse_modal(id)?;
    let include = normalize_includes(include)?;
    let upload = preflight(ctx, dir, max_upload)?;
    let alias = crate::provider::parse(id)?.wait_ssh(ctx, timeout)?;
    let remote = Remote::new(ctx, alias);
    let link = Link {
        id: id.to_string(),
        local: dir.to_path_buf(),
        remote: remote_dir(dir),
        include,
        linked_at: ctx.clock.now().to_string(),
    };
    let prepared = remote.prepare(&link.remote)?;
    if prepared.existing > 0 {
        let question = format!(
            "{} already holds {} on the instance. Uploading overwrites same-named files, and the rest will be pulled into {}.",
            link.remote,
            plural(prepared.existing as usize, "file"),
            dir.display()
        );
        if !yes {
            if !ctx.interactive() {
                return Err(Error::usage(question).next(format!("gpu link {id} --yes")));
            }
            if !crate::prompt::confirm(ctx, Some(&format!("{question} Continue?")))? {
                return Err(Error::Cancelled(130));
            }
        }
    }
    ctx.progress(&format!(
        "Copying {} to {}:{} ...",
        describe_upload(&upload),
        id,
        link.remote
    ))?;
    remote.upload(&link.remote, upload.archive.path())?;
    let listing = remote.manifest(&link.remote)?;
    let base = engine::initial_base(&listing, &upload.stats);
    store::save_base(&ctx.env, &link, &base)?;
    store::save_link(&ctx.env, &link)?;
    ctx.result("linked", Value::Bool(true));
    ctx.result("local_dir", Value::str(dir.to_string_lossy()));
    ctx.result("remote_dir", Value::str(&link.remote));
    ctx.result("uploaded_files", Value::Int(upload.files.len() as i64));
    ctx.result("uploaded_bytes", Value::Int(upload.bytes as i64));
    ctx.println(&format!(
        "Linked {} with {id}:{}. Copied {}.\nEdit on the instance, then pull its changes here: gpu sync  (or gpu sync --watch)",
        dir.display(),
        link.remote,
        describe_upload(&upload)
    ))?;
    Ok((link, remote.alias))
}

/// `gpu link`.
pub(crate) fn link_command(ctx: &Ctx, cfg: &Dict, flags: &LinkFlags) -> Result<()> {
    let dir = resolve_dir(flags.dir.as_deref())?;
    let Some(id) = crate::provider::selected_id(ctx, cfg, flags.id.as_deref(), "link")? else {
        return Ok(());
    };
    let (_, alias) = link(
        ctx,
        &id,
        &dir,
        &flags.include,
        &flags.max_upload,
        flags.yes,
        flags.timeout,
    )?;
    if crate::provider::parse(&id)?
        .expiry_armed(ctx, cfg, &alias)
        .unwrap_or(false)
    {
        expiry_notice(ctx, &id)?;
    }
    Ok(())
}

/// After `gpu up --dir` created `id`: wait for SSH, copy the directory up.
pub(crate) fn after_up(
    ctx: &Ctx,
    cfg: &Dict,
    flags: &crate::cli::UpFlags,
    id: &str,
    dir: &Path,
) -> Result<()> {
    link(
        ctx,
        id,
        dir,
        &flags.include,
        &flags.max_upload,
        true,
        flags.timeout.max(ONE_TIME_WAIT),
    )
    .map(drop)
    .map_err(|error| {
        let message = format!(
            "Instance {id} is running but {} could not be copied to it: {}",
            dir.display(),
            error.text()
        );
        error
            .with_text(message)
            .next(format!("gpu link {id} --dir {}", dir.display()))
    })?;
    if flags.expire.as_deref().is_some_and(|e| !e.is_empty()) {
        expiry_notice(ctx, id)?;
    }
    // The editor offer waited for the directory to exist on the instance.
    crate::editors::offer_after_launch(ctx, cfg, &*crate::provider::parse(id)?)
}

/// The directory on the instance that a linked local directory mirrors.
pub(crate) fn linked_directory(env: &crate::ctx::Env, id: &str) -> Option<String> {
    store::links(env)
        .into_iter()
        .find(|link| link.id == id)
        .map(|link| link.remote)
}

/// Expiry destroys without a final pull: say so before it is armed.
pub(crate) fn expiry_notice(ctx: &Ctx, id: &str) -> Result<()> {
    let links: Vec<_> = store::links(&ctx.env)
        .into_iter()
        .filter(|l| l.id == id)
        .collect();
    if links.is_empty() {
        return Ok(());
    }
    ctx.eprintln(&format!(
        "Note: {id} is linked to {}. Auto-destroy deletes the GPU copy without pulling it first: \
         changes not yet synced are lost. Keep `gpu sync --watch` running, or run `gpu sync` before the deadline.",
        links[0].local.display()
    ))
}

/// Forget links whose instance a provider no longer lists.
pub(crate) fn prune(ctx: &Ctx, cfg: &Dict) -> Option<crate::provider::Live> {
    let live = match crate::provider::live(ctx, cfg) {
        Ok(live) => live,
        Err(error) => {
            let _ = ctx.eprintln(&format!(
                "Could not list instances ({}); links are kept.",
                error.summary()
            ));
            return None;
        }
    };
    for link in store::links(&ctx.env) {
        let gone = live.answered.contains(&link.provider())
            && !live
                .items
                .iter()
                .any(|item| id_of(item).as_deref() == Some(link.id.as_str()));
        if gone {
            store::remove_link(&ctx.env, &link);
        }
    }
    Some(live)
}

fn pull_options(max_file: &str, ask_large: bool) -> Result<PullOptions> {
    Ok(PullOptions {
        max_file: run_files::size_bytes(max_file, "--max-file")?,
        ask_large,
    })
}

fn say_outcome(ctx: &Ctx, link: &Link, outcome: &Outcome) -> Result<()> {
    if !outcome.pulled.is_empty() {
        ctx.println(&format!(
            "Pulled {} ({}) from {} into {}",
            plural(outcome.pulled.len(), "file"),
            human(outcome.bytes),
            link.id,
            link.local.display()
        ))?;
    }
    for conflict in &outcome.conflicts {
        ctx.println(&format!(
            "CONFLICT {}: edited here and on the GPU. Kept yours; the GPU version is saved as {}",
            conflict.path, conflict.saved_as
        ))?;
    }
    if !outcome.large.is_empty() {
        let total: u64 = outcome.large.iter().map(|(_, size)| size).sum();
        let (path, size) = outcome.large.iter().max_by_key(|(_, size)| *size).unwrap();
        ctx.println(&format!(
            "Not pulled: {} above the size limit ({} in all; largest {path}, {}). Pull them with: gpu sync --max-file {}M",
            plural(outcome.large.len(), "file"),
            human(total),
            human(*size),
            size / (1024 * 1024) + 1,
        ))?;
    }
    if !outcome.missing.is_empty() {
        ctx.println(&format!(
            "Could not copy {}: {}",
            plural(outcome.missing.len(), "file"),
            outcome
                .missing
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
    }
    if !outcome.kept_deleted.is_empty() {
        ctx.println(&format!(
            "{} deleted on the GPU; your local {} kept: {}",
            plural(outcome.kept_deleted.len(), "file"),
            if outcome.kept_deleted.len() == 1 {
                "copy is"
            } else {
                "copies are"
            },
            outcome
                .kept_deleted
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ))?;
    }
    Ok(())
}

fn record_outcome(ctx: &Ctx, outcome: &Outcome) {
    let list = |items: &[String]| Value::List(items.iter().map(Value::str).collect());
    ctx.result("pulled", list(&outcome.pulled));
    ctx.result("pulled_bytes", Value::Int(outcome.bytes as i64));
    ctx.result(
        "conflicts",
        Value::List(
            outcome
                .conflicts
                .iter()
                .map(|c| {
                    let mut item = Dict::new();
                    item.insert("path", Value::str(&c.path));
                    item.insert("saved_as", Value::str(&c.saved_as));
                    Value::Dict(item)
                })
                .collect(),
        ),
    );
    ctx.result(
        "not_pulled_large",
        list(
            &outcome
                .large
                .iter()
                .map(|(p, _)| p.clone())
                .collect::<Vec<_>>(),
        ),
    );
    ctx.result("kept_after_gpu_delete", list(&outcome.kept_deleted));
}

/// One pull for `link`: lock, list, copy, update the baseline. `listing`
/// saves a second listing when the caller already has one.
fn pull_once(
    ctx: &Ctx,
    remote: &Remote,
    link: &Link,
    options: &PullOptions,
    listing: Option<transport::Listing>,
) -> Result<Outcome> {
    let Some(_lock) = store::lock(&ctx.env, link)? else {
        return Err(Error::msg(format!(
            "Another gpu sync is already running for {} and {}",
            link.local.display(),
            link.id
        )));
    };
    let listing = match listing {
        Some(listing) => listing,
        None => remote.manifest(&link.remote)?,
    };
    let mut base = store::load_base(&ctx.env, link);
    let outcome = engine::pull(ctx, remote, link, &mut base, &listing, options)?;
    // The baseline always follows what was applied, even when a later file failed.
    store::save_base(&ctx.env, link, &base)?;
    Ok(outcome)
}

/// The links `gpu sync` may act on for these arguments.
fn candidates(ctx: &Ctx, id: Option<&str>, dir: Option<&str>) -> Result<Vec<Link>> {
    let links = store::links(&ctx.env);
    let here = match (id, dir) {
        (Some(_), None) => None,
        _ => Some(resolve_dir(dir)?),
    };
    Ok(links
        .into_iter()
        .filter(|l| id.is_none_or(|id| l.id == id))
        .filter(|l| here.as_ref().is_none_or(|d| d.starts_with(&l.local)))
        .collect())
}

struct Row {
    link: Link,
    state: String,
    alias: Option<String>,
    listing: Option<transport::Listing>,
}

fn connect(
    ctx: &Ctx,
    link: &Link,
    running: bool,
) -> (Option<String>, Option<transport::Listing>, String) {
    if !running {
        return (None, None, "not running".into());
    }
    let attempt = || -> Result<(String, transport::Listing)> {
        let alias = crate::provider::parse(&link.id)?.wait_ssh(ctx, 60.0)?;
        let listing = Remote::new(ctx, &alias).manifest(&link.remote)?;
        Ok((alias, listing))
    };
    match attempt() {
        Ok((alias, listing)) => {
            let base = store::load_base(&ctx.env, link);
            let behind = engine::behind(ctx, link, &base, &listing);
            let state = if behind == 0 {
                "in sync".into()
            } else {
                format!("behind by {}", plural(behind, "file"))
            };
            (Some(alias), Some(listing), state)
        }
        Err(_) => (None, None, "unreachable".into()),
    }
}

fn print_rows(ctx: &Ctx, rows: &[Row]) -> Result<()> {
    let mut cells = vec![["#", "Instance", "Directory", "On the GPU", "State"].map(String::from)];
    for (n, row) in rows.iter().enumerate() {
        cells.push([
            (n + 1).to_string(),
            row.link.id.clone(),
            row.link.local.display().to_string(),
            row.link.remote.clone(),
            row.state.clone(),
        ]);
    }
    use crate::table::Column::{Left, Plain, Right};
    for line in crate::table::aligned_rows(&cells, [Right, Left, Left, Left, Plain]) {
        ctx.println(line.trim_end())?;
    }
    Ok(())
}

fn record_rows(ctx: &Ctx, rows: &[Row]) {
    ctx.result(
        "instances",
        Value::List(
            rows.iter()
                .map(|row| {
                    let mut item = Dict::new();
                    item.insert("id", Value::str(&row.link.id));
                    item.insert("local_dir", Value::str(row.link.local.to_string_lossy()));
                    item.insert("remote_dir", Value::str(&row.link.remote));
                    item.insert("state", Value::str(&row.state));
                    Value::Dict(item)
                })
                .collect(),
        ),
    );
}

/// `gpu sync`.
pub(crate) fn sync_command(ctx: &Ctx, cfg: &Dict, flags: &SyncFlags) -> Result<()> {
    let options = pull_options(&flags.max_file, !flags.watch)?;
    let include = normalize_includes(&flags.include)?;
    if flags.watch && ctx.json() {
        return Err(Error::usage(
            "--watch runs until Ctrl-C and cannot be used with --json.",
        ));
    }
    let wanted = candidates(ctx, flags.id.as_deref(), flags.dir.as_deref())?;
    let live = prune(ctx, cfg);
    let wanted: Vec<Link> = wanted
        .into_iter()
        .filter(|l| store::links(&ctx.env).contains(l))
        .collect();
    if wanted.is_empty() {
        return Err(match &flags.id {
            Some(id) => Error::usage(format!("{id} is not linked to a local directory")),
            None => Error::usage("No instance is linked to this directory"),
        }
        .next("gpu up --dir . (launch one) or gpu link ID (attach a running one)"));
    }
    let is_running = |link: &Link| match &live {
        Some(live) => live
            .items
            .iter()
            .any(|item| id_of(item).as_deref() == Some(link.id.as_str()) && running(item)),
        None => true,
    };
    let mut rows = Vec::new();
    for mut link in wanted {
        if !include.is_empty() {
            for path in &include {
                if !link.include.contains(path) {
                    link.include.push(path.clone());
                }
            }
            store::save_link(&ctx.env, &link)?;
        }
        let (alias, listing, state) = connect(ctx, &link, is_running(&link));
        rows.push(Row {
            link,
            state,
            alias,
            listing,
        });
    }
    record_rows(ctx, &rows);
    print_rows(ctx, &rows)?;
    if flags.list {
        return Ok(());
    }
    let syncable: Vec<usize> = (0..rows.len())
        .filter(|&i| rows[i].alias.is_some())
        .collect();
    let chosen = match syncable.as_slice() {
        [] => {
            return Err(
                Error::msg("None of these instances is running and reachable over SSH")
                    .next("gpu ls"),
            )
        }
        [only] => *only,
        _ => {
            if !ctx.interactive() {
                return Err(
                    Error::usage("Several linked instances: pass the instance ID to sync")
                        .next("gpu sync ID"),
                );
            }
            let options: Vec<crate::picker::Opt<String>> = syncable
                .iter()
                .map(|&i| {
                    crate::picker::Opt::new(
                        i.to_string(),
                        format!(
                            "{}  {}  {}",
                            rows[i].link.id, rows[i].link.remote, rows[i].state
                        ),
                    )
                })
                .collect();
            let screen = crate::picker::Screen::new(
                "Pull from which instance",
                move || options.clone(),
                crate::picker::Next::Done,
            );
            crate::picker::pick(ctx, screen)?
                .parse::<usize>()
                .expect("an index")
        }
    };
    let row = rows.swap_remove(chosen);
    let remote = Remote::new(ctx, row.alias.expect("reachable"));
    if flags.watch {
        return watch(ctx, &remote, &row.link, &options, row.listing, None);
    }
    let outcome = pull_once(ctx, &remote, &row.link, &options, row.listing)?;
    record_outcome(ctx, &outcome);
    say_outcome(ctx, &row.link, &outcome)?;
    if outcome.quiet() {
        ctx.println(&format!(
            "{} is in sync with {}",
            row.link.local.display(),
            row.link.id
        ))?;
    }
    Ok(())
}

/// `--watch`: pull, then keep pulling as the instance changes. `cycles` bounds the
/// loop for tests.
fn watch(
    ctx: &Ctx,
    remote: &Remote,
    link: &Link,
    options: &PullOptions,
    first: Option<transport::Listing>,
    cycles: Option<u32>,
) -> Result<()> {
    ctx.println(&format!(
        "Watching {}:{} -> {}. Ctrl-C to stop.",
        link.id,
        link.remote,
        link.local.display()
    ))?;
    let mut sig = String::new();
    let mut pending = first;
    let (mut failures, mut idle, mut waited) = (0u32, 0u32, 0u32);
    // Warn about each oversized file once per run, not on every cycle.
    let mut warned: std::collections::BTreeSet<String> = Default::default();
    loop {
        let step = (|| -> Result<()> {
            let listing = match pending.take() {
                Some(listing) => Some(listing),
                None if sig.is_empty() => Some(remote.manifest(&link.remote)?),
                None => {
                    let found = remote.wait(&link.remote, WATCH_WAIT, &sig)?;
                    waited += 1;
                    if found.is_none() {
                        idle += 1;
                    }
                    match found {
                        Some(listing) => Some(listing),
                        None if idle >= BACKSTOP_EVERY => {
                            idle = 0;
                            Some(remote.manifest(&link.remote)?)
                        }
                        None => None,
                    }
                }
            };
            if let Some(listing) = listing {
                sig = listing.sig.clone();
                let mut outcome = pull_once(ctx, remote, link, options, Some(listing))?;
                outcome
                    .large
                    .retain(|(path, _)| warned.insert(path.clone()));
                say_outcome(ctx, link, &outcome)?;
            }
            Ok(())
        })();
        match step {
            Ok(()) => failures = 0,
            Err(error) => {
                failures += 1;
                if failures >= WATCH_RETRIES {
                    return Err(error);
                }
                ctx.eprintln(&format!(
                    "Sync paused: {}. Retrying in 10s ...",
                    crate::redact::redact_text(error.summary())
                ))?;
                ctx.clock.sleep(10.0);
            }
        }
        if cycles.is_some_and(|limit| waited >= limit) {
            return Ok(());
        }
    }
}

/// After a destroy: forget links to instances that are now gone.
pub(crate) fn after_down(ctx: &Ctx, cfg: &Dict, id: &str) {
    if store::links(&ctx.env).iter().any(|l| l.id == id) {
        prune(ctx, cfg);
    }
}

/// `gpu unlink`.
pub(crate) fn unlink_command(
    ctx: &Ctx,
    cfg: &Dict,
    id: Option<&str>,
    dir: Option<&str>,
) -> Result<()> {
    let id = match id {
        Some(id) => Some(id.to_string()),
        None => {
            let here = resolve_dir(dir)?;
            let linked: Vec<Link> = store::links(&ctx.env)
                .into_iter()
                .filter(|l| here.starts_with(&l.local))
                .collect();
            match linked.as_slice() {
                [only] => Some(only.id.clone()),
                [] => return Err(Error::usage("No instance is linked to this directory")),
                _ => crate::provider::selected_id(ctx, cfg, None, "unlink")?,
            }
        }
    };
    let Some(id) = id else { return Ok(()) };
    let links = candidates(ctx, Some(&id), dir)?;
    if links.is_empty() {
        return Err(Error::usage(format!(
            "{id} is not linked to a local directory"
        )));
    }
    for link in &links {
        store::remove_link(&ctx.env, link);
        ctx.println(&format!(
            "Unlinked {id} from {}. Files were left as they are.",
            link.local.display()
        ))?;
    }
    ctx.result("unlinked", Value::Bool(true));
    Ok(())
}

/// A short list of `paths`.
fn some_paths(paths: &[String]) -> String {
    let mut shown = paths.iter().take(5).cloned().collect::<Vec<_>>().join(", ");
    if paths.len() > 5 {
        shown.push_str(&format!(" and {} more", paths.len() - 5));
    }
    shown
}

/// What `gpu down` would lose: files a pull could not copy, and changed files
/// it leaves on the instance.
fn unpulled_problem(failed: usize, ignored: &[String], unpullable: &[String]) -> Option<String> {
    let mut parts = Vec::new();
    if failed > 0 {
        parts.push(format!("{} could not be pulled", plural(failed, "file")));
    }
    if !ignored.is_empty() {
        parts.push(format!(
            "{} changed on the GPU but not pulled because git ignores {} ({})",
            plural(ignored.len(), "file"),
            if ignored.len() == 1 { "it" } else { "them" },
            some_paths(ignored)
        ));
    }
    if !unpullable.is_empty() {
        parts.push(format!(
            "{} changed on the GPU but never pulled because {}; copy by hand what you need ({})",
            plural(unpullable.len(), "file"),
            if unpullable.len() == 1 {
                "it looks like a secret"
            } else {
                "they look like secrets"
            },
            some_paths(unpullable)
        ));
    }
    (!parts.is_empty()).then(|| parts.join("; "))
}

/// Before `gpu down` destroys `id`: pull what the local copy has not seen,
/// or refuse to go on without an explicit say-so.
pub(crate) fn before_down(ctx: &Ctx, id: &str, discard: bool) -> Result<()> {
    let links: Vec<Link> = store::links(&ctx.env)
        .into_iter()
        .filter(|l| l.id == id)
        .collect();
    for link in links {
        ctx.progress(&format!(
            "{id} is linked to {}: pulling its latest changes before destroying it ...",
            link.local.display()
        ))?;
        let attempt = || -> Result<Outcome> {
            let alias = crate::provider::parse(id)?.wait_ssh(ctx, 60.0)?;
            let remote = Remote::new(ctx, alias);
            let options = PullOptions {
                max_file: engine::DEFAULT_MAX_FILE,
                ask_large: true,
            };
            pull_once(ctx, &remote, &link, &options, None)
        };
        let (problem, sync) = match attempt() {
            Ok(outcome) => {
                say_outcome(ctx, &link, &outcome)?;
                ctx.result("final_pull", Value::Bool(true));
                let left = outcome.large.len() + outcome.missing.len();
                let sync = if !outcome.ignored.is_empty() {
                    Some(format!("gpu sync {id} --include PATH"))
                } else {
                    (left > 0).then(|| format!("gpu sync {id}"))
                };
                (
                    unpulled_problem(left, &outcome.ignored, &outcome.unpullable),
                    sync,
                )
            }
            Err(error) => (
                Some(error.summary().to_string()),
                Some(format!("gpu sync {id}")),
            ),
        };
        let Some(problem) = problem else { continue };
        let warning = format!(
            "The latest changes on {id} are not all in {}: {problem}. They are lost when the instance is destroyed.",
            link.local.display()
        );
        if discard {
            ctx.eprintln(&format!("Warning: {warning}"))?;
            continue;
        }
        if !ctx.interactive() {
            let discard = format!("gpu down {id} --yes --discard-unsynced");
            let next = match sync {
                Some(sync) => format!("{sync}, or {discard}"),
                None => discard,
            };
            return Err(Error::usage(warning).next(next));
        }
        ctx.eprintln(&format!("Warning: {warning}"))?;
        if !crate::prompt::confirm(ctx, Some("Destroy it anyway?"))? {
            return Err(Error::Cancelled(130));
        }
    }
    Ok(())
}
