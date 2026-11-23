//! Attach a GPU to a local-only codebase: `gpu up --dir`, `gpu link`,
//! `gpu sync`. The local directory is copied up once; afterwards the
//! instance's copy leads and `gpu sync` mirrors its changes back.
//! rsync over SSH moves the files; `engine` decides what moves and what is
//! kept as a conflict, `transport` is everything that crosses SSH.
mod autosync;
mod engine;
mod store;
mod transport;
mod watch;

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
use watch::watch;

pub(crate) use autosync::run_command as autosync_run;

/// Seconds one `wait` call may block on the instance.
const WATCH_WAIT: u32 = 30;
/// A full listing even when nothing was reported, every this many idle waits.
const BACKSTOP_EVERY: u32 = 4;
/// Consecutive failures after which `--watch` gives up.
const WATCH_RETRIES: u32 = 12;
const ONE_TIME_WAIT: f64 = 600.0;
/// Seconds a pull waits for another sync of the same link to finish.
const LOCK_WAIT: f64 = 120.0;
/// The file size limit autosync pulls under (as `gpu sync --max-file` writes it).
const DEFAULT_MAX_FILE_TEXT: &str = "256M";

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
pub(crate) fn dry_run_report(ctx: &Ctx, dir: &Path, max_upload: &str) -> Result<()> {
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

/// Start autosync for a link just made. The link stands either way, so a
/// failure to start says how to try again instead of failing the command.
fn start_after_link(ctx: &Ctx, link: &Link) -> Result<()> {
    autosync::start(ctx, link, DEFAULT_MAX_FILE_TEXT).or_else(|error| {
        ctx.eprintln(&format!(
            "{}. Start it later: gpu sync {} --autosync",
            error.text().trim_end_matches('.'),
            link.id
        ))
    })
}

/// `gpu link`.
pub(crate) fn link_command(ctx: &Ctx, cfg: &Dict, flags: &LinkFlags) -> Result<()> {
    let dir = resolve_dir(flags.dir.as_deref())?;
    let Some(id) = crate::provider::selected_id(ctx, cfg, flags.id.as_deref(), "link")? else {
        return Ok(());
    };
    let (link, alias) = link(
        ctx,
        &id,
        &dir,
        &flags.include,
        &flags.max_upload,
        flags.yes,
        flags.timeout,
    )?;
    if flags.autosync {
        start_after_link(ctx, &link)?;
    }
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
    let link = link(
        ctx,
        id,
        dir,
        &flags.include,
        &flags.max_upload,
        true,
        flags.timeout.max(ONE_TIME_WAIT),
    )
    .map(|(link, _)| link)
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
    // The picker's answer when the flag was not given.
    if flags.autosync || ctx.autosync_chosen() {
        start_after_link(ctx, &link)?;
    }
    if flags.expire.as_deref().is_some_and(|e| !e.is_empty()) {
        expiry_notice(ctx, id)?;
    }
    // The editor offer waited for the directory to exist on the instance.
    crate::editors::offer_after_launch(ctx, cfg, &*crate::provider::parse(id)?)
}

/// After `gpu up`: attach the directory from `--dir` or the one the launch
/// picker chose (`Ctx::picked_dir`); both continue the same way.
pub(crate) fn finish_up(
    ctx: &Ctx,
    cfg: &Dict,
    flags: &crate::cli::UpFlags,
    id: Option<String>,
    dry_run: bool,
) -> Result<()> {
    let dir = flags
        .dir
        .as_ref()
        .map(PathBuf::from)
        .or_else(|| ctx.picked_dir());
    match (dir, id) {
        (Some(dir), _) if dry_run => dry_run_report(ctx, &dir, &flags.max_upload),
        (Some(dir), Some(id)) => after_up(ctx, cfg, flags, &id, &dir),
        _ => Ok(()),
    }
}

/// Whether `gpu up` asks about autosync: a directory is attached, and
/// nothing said to launch without questions.
pub(crate) fn asks_autosync(flags: &crate::cli::UpFlags) -> bool {
    flags.dir.is_some() && !flags.autosync && !flags.yes && !flags.dry_run
}

/// In a terminal, `gpu up --dir` asks whether to keep the directory in
/// sync in the background; the answer is read by `after_up`.
pub(crate) fn ask_autosync(ctx: &Ctx, dir: &Path) -> Result<()> {
    if !ctx.interactive() {
        return Ok(());
    }
    let options = vec![
        crate::picker::Opt::new(
            "yes".to_string(),
            "Yes: pull GPU changes into the directory in the background (autosync)",
        ),
        crate::picker::Opt::new("no".to_string(), "No: I will run gpu sync myself"),
    ];
    let screen = crate::picker::Screen::new(
        format!("Keep {} in sync with the GPU automatically?", dir.display()),
        move || options.clone(),
        crate::picker::Next::Done,
    );
    let answer = crate::picker::pick(ctx, screen)?;
    ctx.set_autosync_choice(Some(answer == "yes"));
    Ok(())
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
    if links
        .iter()
        .any(|link| store::autosync_pid(&ctx.env, link).is_some())
    {
        return ctx.eprintln(&format!(
            "Note: {id} is linked to {} and autosync runs. It pulls once more a few minutes before the deadline \
             if the instance can be reached; changes made after that, or while it cannot connect, are lost.",
            links[0].local.display()
        ));
    }
    ctx.eprintln(&format!(
        "Note: {id} is linked to {}. Auto-destroy deletes the GPU copy without pulling it first: \
         changes not yet synced are lost. Run `gpu sync --autosync` (pulls once more before the deadline), \
         keep `gpu sync --watch` running, or run `gpu sync` before the deadline.",
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

/// The lines that tell what a pull did.
fn outcome_lines(link: &Link, outcome: &Outcome) -> Vec<String> {
    let mut lines = Vec::new();
    if !outcome.pulled.is_empty() {
        lines.push(format!(
            "Pulled {} ({}) from {} into {}",
            plural(outcome.pulled.len(), "file"),
            human(outcome.bytes),
            link.id,
            link.local.display()
        ));
    }
    for conflict in &outcome.conflicts {
        lines.push(format!(
            "CONFLICT {}: edited here and on the GPU. Kept yours; the GPU version is saved as {}",
            conflict.path, conflict.saved_as
        ));
    }
    if !outcome.large.is_empty() {
        let total: u64 = outcome.large.iter().map(|(_, size)| size).sum();
        let (path, size) = outcome.large.iter().max_by_key(|(_, size)| *size).unwrap();
        lines.push(format!(
            "Not pulled: {} above the size limit ({} in all; largest {path}, {}). Pull them with: gpu sync --max-file {}M",
            plural(outcome.large.len(), "file"),
            human(total),
            human(*size),
            size / (1024 * 1024) + 1,
        ));
    }
    if !outcome.missing.is_empty() {
        lines.push(format!(
            "Could not copy {}: {}",
            plural(outcome.missing.len(), "file"),
            outcome
                .missing
                .iter()
                .take(5)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
    if !outcome.kept_deleted.is_empty() {
        lines.push(format!(
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
        ));
    }
    lines
}

fn say_outcome(ctx: &Ctx, link: &Link, outcome: &Outcome) -> Result<()> {
    for line in outcome_lines(link, outcome) {
        ctx.println(&line)?;
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
    let Some(_lock) = store::lock_wait(ctx, link, LOCK_WAIT)? else {
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
    // A background syncer asked to stop finishes this pull first.
    let _critical = crate::process::critical();
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
    autosync: autosync::Info,
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
    let mut cells = vec![[
        "#",
        "Instance",
        "Directory",
        "On the GPU",
        "State",
        "Autosync",
    ]
    .map(String::from)];
    for (n, row) in rows.iter().enumerate() {
        cells.push([
            (n + 1).to_string(),
            row.link.id.clone(),
            row.link.local.display().to_string(),
            row.link.remote.clone(),
            row.state.clone(),
            row.autosync.label(),
        ]);
    }
    use crate::table::Column::{Left, Plain, Right};
    for line in crate::table::aligned_rows(&cells, [Right, Left, Left, Left, Left, Plain]) {
        ctx.println(line.trim_end())?;
    }
    for row in rows {
        if row.autosync.errors().is_empty() {
            continue;
        }
        ctx.println(&format!(
            "Recent autosync errors for {} (log: {}):",
            row.link.id,
            row.autosync.log.display()
        ))?;
        for (at, text) in row.autosync.errors() {
            ctx.println(&format!("  {at}  {text}"))?;
        }
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
                    item.insert("autosync", row.autosync.value());
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
    if flags.no_autosync {
        let mut stopped = false;
        for link in &wanted {
            if autosync::stop(ctx, link)? {
                stopped = true;
                ctx.println(&format!(
                    "Stopped autosync of {} for {}. Files were left as they are.",
                    link.id,
                    link.local.display()
                ))?;
            } else {
                ctx.println(&format!(
                    "Autosync is not running for {} and {}",
                    link.id,
                    link.local.display()
                ))?;
            }
        }
        ctx.result("autosync_stopped", Value::Bool(stopped));
        return Ok(());
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
            autosync: autosync::info(ctx, &link),
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
    if flags.autosync {
        autosync::start(ctx, &row.link, &flags.max_file)?;
        ctx.result("autosync", Value::Bool(true));
        return Ok(());
    }
    if let (true, Some(pid)) = (flags.watch, row.autosync.pid) {
        return Err(Error::usage(format!(
            "Autosync already syncs {} (process {pid}); watching as well would only repeat its work",
            row.link.local.display()
        ))
        .next(format!("gpu sync {} --no-autosync", row.link.id)));
    }
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

/// After a destroy: forget links to instances that are now gone.
pub(crate) fn after_down(ctx: &Ctx, cfg: &Dict, id: &str) {
    let links: Vec<Link> = store::links(&ctx.env)
        .into_iter()
        .filter(|l| l.id == id)
        .collect();
    if links.is_empty() {
        return;
    }
    // The final pull is done: the background syncer has nothing left to do.
    for link in &links {
        if let Err(error) = autosync::stop(ctx, link) {
            let _ = ctx.eprintln(&format!("Warning: {}", error.text()));
        }
    }
    prune(ctx, cfg);
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
        if let Err(error) = autosync::stop(ctx, link) {
            ctx.eprintln(&format!("Warning: {}", error.text()))?;
        }
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
            "The latest changes on {id} are not all in {}: {problem}. They are lost when the instance is destroyed",
            link.local.display()
        );
        if discard {
            ctx.eprintln(&format!("Warning: {warning}."))?;
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
        ctx.eprintln(&format!("Warning: {warning}."))?;
        if !crate::prompt::confirm(ctx, Some("Destroy it anyway?"))? {
            return Err(Error::Cancelled(130));
        }
    }
    Ok(())
}
