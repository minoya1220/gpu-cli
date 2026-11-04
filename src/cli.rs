//! Command-line definitions (argparse in the Python tool).

use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};

pub const VERSION: &str = "0.1.0";

#[derive(Parser, Debug)]
#[command(
    name = "gpu",
    about = "Launch GPU workspaces on Vast.ai or TPU VMs on Google Cloud",
    disable_version_flag = true,
    infer_long_args = true,
    args_override_self = true
)]
pub struct Cli {
    /// show program's version number and exit
    #[arg(short = 'v', short_alias = 'V', long = "version", action = ArgAction::SetTrue)]
    pub version: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Launch a GPU or TPU workspace with an optional repository
    Up(UpFlags),
    /// List rented instances
    Ls {
        /// print raw JSON for scripts
        #[arg(long)]
        json: bool,
        /// Limit listing to one provider
        #[arg(long, value_parser = ["vast", "gcp"])]
        provider: Option<String>,
    },
    /// Print or execute the SSH command
    Ssh {
        id: Option<String>,
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
    /// Stop an instance
    Stop { id: Option<String> },
    /// Destroy an instance
    Down {
        id: Option<String>,
        #[arg(long)]
        yes: bool,
    },
    /// Schedule destruction, show or cancel it
    #[command(
        override_usage = "gpu expire [ID] WHEN|cancel|show",
        long_about = "Auto-destroy (disk deleted). WHEN: 2h, 1h30m, 18:30, 6:30PM, 6pm, or ISO datetime. Clock times use the next local occurrence; bare numbers are instance IDs.",
        after_help = "Examples: gpu expire 2h; gpu expire 12345678 6:30pm; gpu expire 12345678 cancel; gpu expire show"
    )]
    Expire {
        /// Optional Vast ID or gcp:PROJECT:ZONE:NAME, then deadline, cancel, or show
        #[arg(value_name = "ID/WHEN", num_args = 0..)]
        expiry_args: Vec<String>,
    },
}

#[derive(clap::Args, Debug)]
pub struct UpFlags {
    /// Provider (Vast by default; configured GCP enables the provider picker)
    #[arg(long, value_parser = ["vast", "gcp"])]
    pub provider: Option<String>,
    /// TPU accelerator type, e.g. v5e-1 or v6e-1
    #[arg(long)]
    pub tpu: Option<String>,
    /// Google Cloud zone (overrides gcp.zone)
    #[arg(long)]
    pub zone: Option<String>,
    /// TPU runtime image/version (overrides gcp.runtime)
    #[arg(long)]
    pub runtime: Option<String>,
    /// Rent a Spot TPU instead of on-demand
    #[arg(long, conflicts_with = "on_demand")]
    pub spot: bool,
    /// Rent an on-demand TPU
    #[arg(long)]
    pub on_demand: bool,
    /// Clone HTTPS/git@ URL, GitHub owner/name, or a name under configured owner
    #[arg(long, conflicts_with = "no_repo")]
    pub repo: Option<String>,
    /// Launch without cloning a repository
    #[arg(long)]
    pub no_repo: bool,
    #[arg(long, value_parser = ["container", "vm"])]
    pub mode: Option<String>,
    #[arg(long)]
    pub gpu: Option<String>,
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    pub num_gpus: i64,
    #[arg(long, allow_negative_numbers = true)]
    pub max_price: Option<f64>,
    #[arg(long)]
    pub interruptible: bool,
    #[arg(long, allow_negative_numbers = true)]
    pub offer: Option<i64>,
    #[arg(long, allow_negative_numbers = true)]
    pub disk: Option<i64>,
    #[arg(long)]
    pub yes: bool,
    #[arg(long)]
    pub dry_run: bool,
    /// Auto-destroy after e.g. 2h or 1h30m (includes boot time)
    #[arg(long, value_name = "DURATION")]
    pub expire: Option<String>,
    #[arg(long, allow_negative_numbers = true)]
    pub reliability: Option<f64>,
    /// include verified, unverified, and deverified hosts
    #[arg(long)]
    pub unverified: bool,
}

/// What argument parsing produced.
pub enum Parsed {
    Run(Box<Command>),
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
        }) => Parsed::Run(Box::new(command)),
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
