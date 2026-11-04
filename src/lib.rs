//! `gpu`: launch Vast.ai GPU and Google Cloud TPU workspaces through
//! authenticated provider CLIs. External work always crosses the Runner seam.

pub mod cli;
pub mod commands;
pub mod config;
pub mod ctx;
pub mod editors;
pub mod error;
pub mod expiry;
pub mod gcp;
pub mod gcp_expiry;
pub mod gcp_ssh;
pub mod gcp_wizard;
pub mod history;
pub mod instances;
pub mod launch;
pub mod offers;
pub mod picker;
pub mod process;
pub mod prompt;
pub mod provider;
pub mod pyfmt;
pub mod pyjson;
pub mod redact;
pub mod repos;
pub mod ssh_config;
pub mod startup;
pub mod vast;
pub mod wizard;

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
    let command = match cli::parse(argv) {
        Parsed::Run(command) => *command,
        Parsed::Exit {
            code,
            text,
            to_stdout,
        } => {
            let _ = if to_stdout {
                ctx.console.write_out(&text)
            } else {
                ctx.console.eprintln(text.trim_end())
            };
            return code;
        }
    };
    match dispatch(ctx, command) {
        Ok(()) => 0,
        Err(Error::Cancelled(status)) => {
            let _ = ctx.println("Cancelled.");
            status
        }
        Err(Error::Usage(text)) => {
            let _ = ctx.eprintln(text.trim_end());
            2
        }
        Err(Error::Msg(text)) => {
            let _ = ctx.eprintln(&format!("gpu: {}", redact::redact_text(&text)));
            1
        }
    }
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

fn dispatch(ctx: &Ctx, command: Command) -> Result<()> {
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
    let cfg = config::load_config(&ctx.env)?;
    match command {
        Command::Up(flags) => {
            let tpu_flags = flags.tpu.is_some()
                || flags.zone.is_some()
                || flags.runtime.is_some()
                || flags.spot
                || flags.on_demand;
            // TPU flags imply Google Cloud; only an explicit --provider vast rejects them.
            if flags.provider.as_deref() == Some("gcp") || (flags.provider.is_none() && tpu_flags) {
                return gcp::launch(ctx, &flags, &cfg, up_deadline);
            }
            if tpu_flags {
                return Err(Error::msg(
                    "TPU flags do not apply to Vast. Next: gpu up --provider gcp --help",
                ));
            }
            launch::launch(ctx, &up_args(flags), &cfg, up_deadline)
        }
        Command::Ls { json, provider } => provider::ls(ctx, &cfg, json, provider.as_deref()),
        Command::Expire { .. } => {
            let (id, action, deadline) = expire_plan.expect("parsed above");
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "expire")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => {
                    commands::expire_instance(ctx, &id, &action, deadline)
                }
                provider::InstanceId::Gcp(id) => {
                    gcp_expiry::manage(ctx, &cfg, &id, &action, deadline)
                }
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
            }
        }
        Command::Cursor { id } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "cursor")? else {
                return Ok(());
            };
            // Compatibility alias for `gpu edit --editor cursor`.
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => {
                    commands::edit_instance(ctx, &id, Some("cursor"), None, 120.0)
                }
                provider::InstanceId::Gcp(id) => {
                    gcp_ssh::edit(ctx, &cfg, &id, Some("cursor"), None)
                }
            }
        }
        Command::Ssh { id, exec } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "ssh")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::ssh(ctx, &id, exec),
                provider::InstanceId::Gcp(id) => gcp_ssh::ssh(ctx, &id, exec),
            }
        }
        Command::Stop { id } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "stop")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::lifecycle(ctx, &id, false, false),
                provider::InstanceId::Gcp(id) => gcp::lifecycle(ctx, &id, &cfg, false, false),
            }
        }
        Command::Down { id, yes } => {
            let Some(id) = provider::selected_id(ctx, &cfg, id.as_deref(), "down")? else {
                return Ok(());
            };
            match provider::InstanceId::parse(&id)? {
                provider::InstanceId::Vast(id) => commands::lifecycle(ctx, &id, true, yes),
                provider::InstanceId::Gcp(id) => gcp::lifecycle(ctx, &id, &cfg, true, yes),
            }
        }
    }
}
