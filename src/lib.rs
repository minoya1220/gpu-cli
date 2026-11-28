//! GPU jobs and workspaces on Vast.ai, Google Cloud and Modal.
//! External work always crosses the Runner seam.

pub(crate) mod amd;
mod atomic_file;
mod cache;
pub(crate) mod cli;
pub mod config;
pub mod ctx;
pub(crate) mod dirsync;
pub(crate) mod editors;
pub mod error;
pub mod expiry;
pub(crate) mod gcp;
mod guardrails;
pub(crate) mod history;
pub mod instances;
pub(crate) mod job;
pub mod launch;
pub(crate) mod modal;
mod output;
pub(crate) mod owner;
mod packing;
pub mod picker;
mod poll;
pub mod process;
mod progress;
pub mod prompt;
pub(crate) mod provider;
mod provider_errors;
pub mod pyfmt;
pub mod pyjson;
pub mod redact;
pub(crate) mod remote;
pub mod repos;
pub(crate) mod run;
mod run_files;
pub(crate) mod setup;
mod spend;
mod ssh;
mod sync;
mod table;
mod url;
pub mod vast;
pub(crate) mod wizard;

#[cfg(test)]
mod testing;
#[cfg(test)]
mod tests;

use cli::{Command, Parsed};
use ctx::Ctx;
use error::{Error, Result};
use launch::UpArgs;

/// Run the CLI with `argv` (program name first); the exit status.
pub fn main_with(argv: &[String], ctx: &Ctx) -> i32 {
    let (command, json) = match cli::parse(argv) {
        Parsed::Run(command) => (*command, false),
        Parsed::RunJson(command) => (*command, true),
        Parsed::Exit {
            code,
            text,
            to_stdout,
        } => {
            if !to_stdout && cli::wants_json(argv) {
                let command = cli::parsed_command_name(argv);
                let error = Err(Error::usage(
                    text.lines().next().unwrap_or("Invalid arguments"),
                ));
                let ctx = ctx.for_command(true);
                if let Some(usage) = text.lines().find(|line| line.starts_with("Usage:")) {
                    ctx.result("usage", pyjson::Value::str(usage));
                }
                let _ = output::finish(&ctx, &command, &error);
                return code;
            }
            let _ = if to_stdout {
                ctx.console.write_out(&text)
            } else {
                ctx.console.eprintln(text.trim_end())
            };
            return code;
        }
    };
    let ctx = ctx.for_command(json);
    let name = command.name();
    ctx.set_command(name);
    let outcome = dispatch(&ctx, command);
    if json {
        if let Err(error) = output::finish(&ctx, name, &outcome) {
            let _ = ctx.eprintln(&format!("gpu: {}", redact::redact_text(error.text())));
            return 1;
        }
    } else if let Err(error) = &outcome {
        match error {
            Error::Cancelled(_) => {
                let _ = ctx.println("Cancelled.");
                if let Some(notice) = ctx.interrupt_notice() {
                    let _ = ctx.eprintln(&notice);
                }
            }
            Error::Remote(_) => {}
            Error::Usage(text) => {
                let _ = ctx.eprintln(text.trim_end());
            }
            _ => {
                let _ = ctx.eprintln(&format!("gpu: {}", redact::redact_text(error.text())));
            }
        }
    }
    outcome.err().map_or(0, |error| error.exit_code())
}

fn dispatch(ctx: &Ctx, mut command: Command) -> Result<()> {
    let mut cfg = config::load_config(&ctx.env)?;
    ctx.set_gcp_ready(gcp::setup::gcp_ready(&cfg));
    if let Command::Up(flags) = &mut command {
        // A directory to attach replaces the repository pick.
        if let Some(dir) = &flags.dir {
            let dir = dirsync::resolve_dir(Some(dir))?;
            if !flags.dry_run {
                dirsync::preflight(ctx, &dir, &flags.max_upload)?;
            }
            flags.dir = Some(dir.to_string_lossy().into_owned());
            flags.no_repo = true;
            ctx.defer_editor_offer();
        }
        guardrails::defaults(flags, &cfg)?;
        if flags.target().amd() {
            amd::validate_up(flags)?;
        }
        if flags.target() == provider::Provider::Vast && flags.tpu_flags() {
            return Err(
                Error::msg("TPU flags do not apply to Vast").next("gpu up --provider gcp --help")
            );
        }
        launch::validate_launch_args(&UpArgs::from(&*flags))?;
        // Syntax validation does not need an inferred account name.
        if let Some(repo) = &flags.repo {
            repos::resolve_repo(repo, "owner")?;
        }
    }
    // Validate expiry syntax before making dependency calls or creating an instance.
    let mut expire_plan = None;
    if let Command::Expire { expiry_args } = &command {
        let (id, when) = expiry::parse_expire_arguments(expiry_args)
            .map_err(|e| Error::usage(cli::usage_error("expire", &e)))?;
        let action = if when == "show" || when == "cancel" {
            when.clone()
        } else {
            "arm".to_string()
        };
        let deadline = if action == "arm" {
            let zone = expiry::local_zone(&ctx.env);
            Some(
                expiry::parse_when(&when, ctx.clock.now(), &zone)
                    .map_err(|e| Error::usage(cli::usage_error("expire", &e)))?,
            )
        } else {
            None
        };
        expire_plan = Some((id, action, deadline));
    }
    let mut up_deadline = None;
    if let Command::Up(flags) = &command {
        if let Some(duration) = flags.expire.as_deref().filter(|e| !e.is_empty()) {
            up_deadline = Some(
                expiry::parse_deadline_duration(duration, ctx.clock.now())
                    .map_err(|e| Error::usage(cli::usage_error("up", &e)))?,
            );
        }
    }
    if ctx.json()
        && (matches!(&command, Command::Ssh { exec: true, .. })
            || matches!(&command, Command::Edit { editor: Some(editor), .. } if editor == "nvim"))
    {
        return Err(Error::usage("Interactive SSH/editor sessions cannot use --json. Use gpu exec ID -- COMMAND for terminal-free execution."));
    }
    guardrails::nonterminal(ctx, &command, &cfg)?;
    let launch_owner = match &command {
        Command::Up(flags) => owner::resolve(ctx, flags.owner.as_deref())?,
        Command::Run(flags) | Command::Job(cli::JobCommand::Start(flags)) => {
            owner::resolve(ctx, flags.owner.as_deref())?
        }
        _ => None,
    };
    if let Some(name) = &launch_owner {
        cfg.insert(owner::CFG_KEY, pyjson::Value::str(name));
    }
    if let Command::Up(flags) = &command {
        if !flags.target().amd() {
            config::infer_identity(ctx, &mut cfg);
        }
        if let Some(repo) = &flags.repo {
            repos::resolve_repo(repo, &pyfmt::py_str(config::get(&cfg, "owner")?))?;
        }
    }
    match command {
        Command::Setup(flags) => setup::run(ctx, &cfg, &flags),
        Command::Offers { provider } => amd::offers(ctx, provider),
        Command::Run(flags) => run::run(ctx, &flags, &cfg, &std::env::current_dir()?),
        Command::Job(action) => match action {
            cli::JobCommand::Start(flags) => {
                run::start_job(ctx, &flags, &cfg, &std::env::current_dir()?)
            }
            cli::JobCommand::Status { id } => job::status(ctx, &id),
            cli::JobCommand::Logs { id, follow, offset } => job::logs(ctx, &id, follow, offset),
            cli::JobCommand::Outputs { id } => job::outputs(ctx, &id),
            cli::JobCommand::Cancel { id } => job::cancel(ctx, &id),
            cli::JobCommand::Ls { owner } => job::ls(ctx, owner.as_deref()),
        },
        Command::Wait {
            id,
            timeout,
            wait_setup,
        } => {
            let id = provider::selected_id(ctx, &cfg, Some(&id), "wait")?.expect("explicit id");
            remote::wait_with_setup(ctx, &id, timeout, wait_setup).map(|_| ())
        }
        Command::Exec {
            id,
            command,
            timeout,
            command_timeout,
        } => {
            ctx.set_command_timeout(command_timeout);
            let id = provider::selected_id(ctx, &cfg, Some(&id), "exec")?.expect("explicit id");
            remote::exec(ctx, &id, &command, timeout)
        }
        Command::Cp {
            id,
            source,
            destination,
            recursive,
            timeout,
            command_timeout,
        } => {
            ctx.set_command_timeout(command_timeout);
            remote::validate_copy(&source, &destination)?;
            let id = provider::selected_id(ctx, &cfg, Some(&id), "cp")?.expect("explicit id");
            remote::copy(ctx, &id, &source, &destination, recursive, timeout)
        }
        Command::Up(flags) => {
            if let Some(dir) = flags
                .dir
                .as_deref()
                .filter(|_| dirsync::asks_autosync(&flags))
            {
                dirsync::ask_autosync(ctx, std::path::Path::new(dir))?;
            }
            let wait = flags.wait || flags.wait_setup;
            let wait_setup = flags.wait_setup;
            ctx.result("setup_done", pyjson::Value::Null);
            let timeout = flags.timeout;
            if wait {
                ctx.set_ssh_timeout(timeout);
            }
            ctx.result("dry_run", pyjson::Value::Bool(flags.dry_run));
            let dry_run = flags.dry_run;
            let launched = if flags.target().amd() {
                amd::up(ctx, &flags, &cfg)?
            } else if flags.target() == provider::Provider::Gcp {
                gcp::launch(ctx, &gcp::TpuArgs::from_flags(&flags)?, &cfg, up_deadline)?
            } else {
                launch::launch(ctx, &UpArgs::from(&flags), &cfg, up_deadline)?
            };
            let id = launched.id.clone();
            remote::after_up(ctx, launched, wait && !dry_run, timeout, wait_setup)?;
            dirsync::finish_up(ctx, &cfg, &flags, id, dry_run)
        }
        Command::Ls {
            json,
            long,
            provider,
            envelope,
            owner,
        } => provider::ls(ctx, &cfg, json, long, provider, envelope, owner.as_deref()),
        Command::Expire { .. } => {
            let (id, action, deadline) = expire_plan.expect("parsed above");
            ctx.result("action", pyjson::Value::str(&action));
            match provider::selected_id(ctx, &cfg, id.as_deref(), "expire")? {
                Some(id) => {
                    if action == "arm" {
                        dirsync::expiry_notice(ctx, &id)?;
                    }
                    provider::parse(&id)?.expire(ctx, &cfg, &action, deadline)
                }
                None => Ok(()),
            }
        }
        Command::Edit {
            id,
            editor,
            nvim_config,
        } => {
            let Some(instance) = provider::select(ctx, &cfg, id.as_deref(), "edit")? else {
                return Ok(());
            };
            let nvim_config = match nvim_config {
                Some(config) => Some(pyjson::Value::Str(config)),
                None => config::nvim_config(&cfg)?,
            };
            instance.edit(ctx, editor.as_deref(), nvim_config.as_ref())
        }
        Command::Ssh { id, exec } => match provider::select(ctx, &cfg, id.as_deref(), "ssh")? {
            Some(instance) => instance.ssh(ctx, exec),
            None => Ok(()),
        },
        Command::Stop { id } => match provider::select(ctx, &cfg, id.as_deref(), "stop")? {
            Some(instance) => instance.stop(ctx, &cfg),
            None => Ok(()),
        },
        Command::Down {
            id,
            yes,
            discard_unsynced,
        } => match provider::selected_id(ctx, &cfg, id.as_deref(), "down")? {
            Some(id) => {
                dirsync::before_down(ctx, &id, discard_unsynced)?;
                provider::parse(&id)?.down(ctx, &cfg, yes)?;
                dirsync::after_down(ctx, &cfg, &id);
                Ok(())
            }
            None => Ok(()),
        },
        Command::Sync(flags) => dirsync::sync_command(ctx, &cfg, &flags),
        Command::AutosyncRun(flags) => dirsync::autosync_run(ctx, &cfg, &flags),
        Command::Link(flags) => dirsync::link_command(ctx, &cfg, &flags),
        Command::Unlink { id, dir } => {
            dirsync::unlink_command(ctx, &cfg, id.as_deref(), dir.as_deref())
        }
    }
}
