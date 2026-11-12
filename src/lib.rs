//! GPU jobs and workspaces on Vast.ai, Google Cloud and Modal.
//! External work always crosses the Runner seam.

mod atomic_file;
mod cache;
pub(crate) mod cli;
pub(crate) mod commands;
pub mod config;
pub mod ctx;
pub(crate) mod editors;
pub mod error;
pub mod expiry;
pub(crate) mod gcp;
pub(crate) mod gcp_catalog;
pub(crate) mod gcp_expiry;
mod gcp_facts;
pub(crate) mod gcp_geo;
mod gcp_http;
mod gcp_preferences;
pub(crate) mod gcp_prices;
mod gcp_retry;
pub(crate) mod gcp_ssh;
mod gcp_types;
pub(crate) mod gcp_wizard;
mod gcp_zones;
mod guardrails;
pub(crate) mod history;
pub mod instances;
pub mod launch;
pub(crate) mod modal;
pub mod offers;
mod output;
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
mod ssh;
pub(crate) mod ssh_config;
pub mod startup;
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
use repos::Repo;

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
                let error = Err(Error::Usage(
                    text.lines().next().unwrap_or("Invalid arguments").into(),
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

fn up_args(flags: cli::UpFlags) -> UpArgs {
    let repo = if flags.no_repo {
        Some(Repo::NoRepo)
    } else {
        flags.repo.as_deref().map(Repo::from_arg)
    };
    UpArgs {
        repo,
        mode: flags.mode,
        gpu: flags.gpu,
        num_gpus: flags.num_gpus,
        max_price: flags.max_price,
        interruptible: flags.interruptible,
        offer: flags.offer,
        disk: flags.disk,
        yes: flags.yes,
        dry_run: flags.dry_run,
        expire: flags.expire,
        reliability: flags.reliability,
        unverified: flags.unverified,
    }
}

fn dispatch(ctx: &Ctx, mut command: Command) -> Result<()> {
    let mut cfg = config::load_config(&ctx.env)?;
    ctx.set_gcp_ready(setup::gcp_ready(&cfg));
    if let Command::Up(flags) = &mut command {
        guardrails::defaults(flags, &cfg)?;
        launch::validate_launch_args(&up_args(flags.clone()))?;
        // Syntax validation does not need an inferred account name.
        if let Some(repo) = &flags.repo {
            repos::resolve_repo(repo, "owner")?;
        }
    }
    // Validate expiry syntax before making dependency calls or creating an instance.
    let mut expire_plan = None;
    if let Command::Expire { expiry_args } = &command {
        let (id, when) = expiry::parse_expire_arguments(expiry_args)
            .map_err(|e| Error::Usage(cli::usage_error("expire", &e)))?;
        let action = if when == "show" || when == "cancel" {
            when.clone()
        } else {
            "arm".to_string()
        };
        let deadline = if action == "arm" {
            let zone = expiry::local_zone(&ctx.env);
            Some(
                expiry::parse_when(&when, ctx.clock.now(), &zone)
                    .map_err(|e| Error::Usage(cli::usage_error("expire", &e)))?,
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
                    .map_err(|e| Error::Usage(cli::usage_error("up", &e)))?,
            );
        }
    }
    if ctx.json()
        && (matches!(&command, Command::Ssh { exec: true, .. })
            || matches!(&command, Command::Edit { editor: Some(editor), .. } if editor == "nvim"))
    {
        return Err(output::usage("Interactive SSH/editor sessions cannot use --json. Use gpu exec ID -- COMMAND for terminal-free execution."));
    }
    guardrails::nonterminal(ctx, &command, &cfg)?;
    if let Command::Up(flags) = &command {
        config::infer_identity(ctx, &mut cfg);
        if let Some(repo) = &flags.repo {
            repos::resolve_repo(repo, &pyfmt::py_str(config::get(&cfg, "owner")?))?;
        }
    }
    match command {
        Command::Setup(flags) => setup::run(ctx, &cfg, &flags),
        Command::Run(flags) => run::run(ctx, &flags, &cfg, &std::env::current_dir()?),
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
            let wait = flags.wait || flags.wait_setup;
            let wait_setup = flags.wait_setup;
            ctx.result("setup_done", pyjson::Value::Null);
            let timeout = flags.timeout;
            if wait {
                ctx.set_ssh_timeout(timeout);
            }
            ctx.result("dry_run", pyjson::Value::Bool(flags.dry_run));
            let tpu_flags = flags.tpu.is_some()
                || flags.zone.is_some()
                || flags.runtime.is_some()
                || flags.spot
                || flags.on_demand;
            // TPU flags imply Google Cloud; only an explicit --provider vast rejects them.
            if flags.provider.as_deref() == Some("gcp") || (flags.provider.is_none() && tpu_flags) {
                gcp::launch(ctx, &flags, &cfg, up_deadline)?;
                return remote::after_up(ctx, wait && !flags.dry_run, timeout, wait_setup);
            }
            if tpu_flags {
                return Err(Error::msg(
                    "TPU flags do not apply to Vast. Next: gpu up --provider gcp --help",
                ));
            }
            let dry_run = flags.dry_run;
            launch::launch(ctx, &up_args(flags), &cfg, up_deadline)?;
            remote::after_up(ctx, wait && !dry_run, timeout, wait_setup)
        }
        Command::Ls {
            json,
            long,
            provider,
            envelope,
        } => provider::ls(ctx, &cfg, json, long, provider.as_deref(), envelope),
        Command::Expire { .. } => {
            let (id, action, deadline) = expire_plan.expect("parsed above");
            ctx.result("action", pyjson::Value::str(&action));
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "expire")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => {
                    commands::expire_instance(ctx, &id, &action, deadline).map(drop)
                }
                provider::InstanceId::Gcp(id) => {
                    gcp_expiry::manage(ctx, &cfg, &id, &action, deadline)
                }
                provider::InstanceId::Modal(_) => Err(modal::unsupported()),
            }
        }
        Command::Edit {
            id,
            editor,
            nvim_config,
        } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "edit")? else {
                return Ok(());
            };
            let nvim_config = match nvim_config {
                Some(config) => Some(pyjson::Value::Str(config)),
                None => config::nvim_config(&cfg)?,
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::edit_instance(
                    ctx,
                    &id,
                    editor.as_deref(),
                    nvim_config.as_ref(),
                    120.0,
                ),
                provider::InstanceId::Gcp(id) => {
                    gcp_ssh::edit(ctx, &cfg, &id, editor.as_deref(), nvim_config.as_ref())
                }
                provider::InstanceId::Modal(_) => Err(modal::unsupported()),
            }
        }
        Command::Ssh { id, exec } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "ssh")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::ssh(ctx, &id, exec),
                provider::InstanceId::Gcp(id) => gcp_ssh::ssh(ctx, &id, exec),
                provider::InstanceId::Modal(_) => Err(modal::unsupported()),
            }
        }
        Command::Stop { id } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "stop")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::lifecycle(ctx, &id, false, false),
                provider::InstanceId::Gcp(id) => gcp::lifecycle(ctx, &id, &cfg, false, false),
                provider::InstanceId::Modal(_) => Err(modal::unsupported()),
            }
        }
        Command::Down { id, yes } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "down")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::lifecycle(ctx, &id, true, yes),
                provider::InstanceId::Gcp(id) => gcp::lifecycle(ctx, &id, &cfg, true, yes),
                provider::InstanceId::Modal(id) => modal::down(ctx, &id, yes),
            }
        }
    }
}
