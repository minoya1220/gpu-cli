//! Command-line definitions (argparse in the Python tool).

use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};

pub const VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(
    name = "gpu",
    about = "Launch GPU workspaces on Vast.ai",
    disable_version_flag = true,
    infer_long_args = true,
    args_override_self = true
)]
pub struct Cli {
    /// Print version and exit
    #[arg(short = 'v', short_alias = 'V', long = "version", action = ArgAction::SetTrue)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch a GPU workspace with an optional repository
    Up(UpFlags),
    /// List rented instances
    Ls {
        /// Print credential-redacted JSON for scripts
        #[arg(long)]
        json: bool,
    },
    /// Print the SSH URL, or connect with --exec
    Ssh {
        /// Instance ID; omit to choose
        id: Option<String>,
        /// Connect in this terminal
        #[arg(long)]
        exec: bool,
    },
    /// Open an instance's repository in Cursor over SSH
    Cursor { id: Option<String> },
    /// Open an instance workspace in an editor over SSH
    Edit {
        id: Option<String>,
        #[arg(long, value_parser = ["cursor", "code", "zed", "nvim"])]
        editor: Option<String>,
        /// Neovim config git URL or local directory
        #[arg(long)]
        nvim_config: Option<String>,
    },
    /// Stop compute; keep disk (storage charges can continue)
    Stop { id: Option<String> },
    /// Delete an instance and its disk
    Down {
        /// Instance ID; omit to choose
        id: Option<String>,
        /// Skip the disk-deletion confirmation
        #[arg(long)]
        yes: bool,
    },
    /// Schedule disk deletion, check the timer, or cancel it
    #[command(
        override_usage = "gpu expire [ID] WHEN|cancel|show",
        long_about = "Schedule destruction of an instance and its disk. Scheduling authorizes deletion without another confirmation.\n\nWHEN: 2h, 1h30m, 18:30, 6:30PM, 6pm, or an ISO datetime. Clock times use the next local occurrence. Bare numbers are instance IDs; put the ID first.\n\nThe instance must stay running and reach Vast. Your computer can disconnect after arming.",
        after_help = "Examples:\n  gpu expire 2h\n  gpu expire 12345678 6:30pm\n  gpu expire 12345678 show\n  gpu expire 12345678 cancel"
    )]
    Expire {
        /// Optional digits-only ID, then deadline, cancel, or show
        #[arg(value_name = "ID/WHEN", num_args = 0..)]
        expiry_args: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct UpFlags {
    /// Clone HTTPS/git@ URL, GitHub owner/name, or a name under configured owner
    #[arg(long, conflicts_with = "no_repo")]
    pub repo: Option<String>,
    /// Launch without cloning a repository
    #[arg(long)]
    pub no_repo: bool,
    /// Container for general work; VM for CUDA/Nsight profiling
    #[arg(long, value_parser = ["container", "vm"])]
    pub mode: Option<String>,
    /// GPU model, such as 'RTX 4090'
    #[arg(long)]
    pub gpu: Option<String>,
    /// Number of GPUs
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    pub num_gpus: i64,
    /// Maximum hourly price in USD
    #[arg(long, allow_negative_numbers = true)]
    pub max_price: Option<f64>,
    /// Use interruptible bid pricing
    #[arg(long)]
    pub interruptible: bool,
    /// Select a matching offer ID
    #[arg(long, allow_negative_numbers = true)]
    pub offer: Option<i64>,
    /// Disk size in GB (default: container 20, VM 130; configurable)
    #[arg(long, allow_negative_numbers = true)]
    pub disk: Option<i64>,
    /// Skip paid-launch confirmation; choose the cheapest offer if unspecified
    #[arg(long)]
    pub yes: bool,
    /// Print the request without renting or opening an editor
    #[arg(long)]
    pub dry_run: bool,
    /// Destroy instance and disk after 2h or 1h30m (includes boot time)
    #[arg(long, value_name = "DURATION")]
    pub expire: Option<String>,
    /// Reliability floor from 0 to 1 (default: 0.95; configurable)
    #[arg(long, allow_negative_numbers = true)]
    pub reliability: Option<f64>,
    /// Include verified, unverified, and deverified hosts
    #[arg(long)]
    pub unverified: bool,
}

/// What argument parsing produced.
pub enum Parsed {
    Run(Command),
    /// Print `text` (stdout when `to_stdout`) and exit with `code`.
    Exit {
        code: i32,
        text: String,
        to_stdout: bool,
    },
}

pub fn parse(argv: &[String]) -> Parsed {
    match Cli::try_parse_from(argv) {
        Ok(Cli { version: true, .. }) => Parsed::Exit {
            code: 0,
            text: format!("{VERSION}\n"),
            to_stdout: true,
        },
        Ok(Cli {
            command: Some(command),
            ..
        }) => Parsed::Run(command),
        Ok(Cli { command: None, .. }) => {
            let mut cmd = Cli::command();
            let err = cmd.error(
                ErrorKind::MissingSubcommand,
                "the following arguments are required: <COMMAND>",
            );
            Parsed::Exit {
                code: 2,
                text: err.render().to_string(),
                to_stdout: false,
            }
        }
        Err(err) => {
            let to_stdout = matches!(
                err.kind(),
                ErrorKind::DisplayHelp | ErrorKind::DisplayVersion
            );
            Parsed::Exit {
                code: err.exit_code(),
                text: err.render().to_string(),
                to_stdout,
            }
        }
    }
}

/// A usage error raised after parsing (`parser.error` in Python), rendered
/// with the subcommand's usage line.
pub fn usage_error(subcommand: &str, message: &str) -> String {
    let mut cmd = Cli::command();
    cmd.build();
    match cmd.find_subcommand_mut(subcommand) {
        Some(sub) => sub
            .error(ErrorKind::ValueValidation, message)
            .render()
            .to_string(),
        None => cmd
            .error(ErrorKind::ValueValidation, message)
            .render()
            .to_string(),
    }
}
