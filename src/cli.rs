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

    /// Emit one JSON result object; progress goes to stderr
    #[arg(long = "json", global = true)]
    pub output_json: bool,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Subcommand, Debug)]
pub enum Command {
    /// Show provider setup state or configure a selected provider
    Setup(SetupFlags),
    /// Launch a GPU or TPU workspace with an optional repository
    Up(UpFlags),
    /// List rented instances
    Ls {
        /// Filled from the global JSON flag
        #[arg(skip)]
        json: bool,
        /// Show full provider-qualified IDs
        #[arg(long)]
        long: bool,
        /// Limit listing to one provider
        #[arg(long, value_parser = ["vast", "gcp"])]
        provider: Option<String>,
    },
    /// Wait until the instance answers a non-interactive SSH probe
    Wait {
        id: String,
        /// Maximum readiness wait in seconds
        #[arg(long, default_value = "120", value_parser = positive_seconds)]
        timeout: f64,
    },
    /// Execute a remote command without a terminal; preserve its exit code
    Exec {
        id: String,
        #[arg(required = true, last = true, num_args = 1..)]
        command: Vec<String>,
        /// Maximum SSH readiness wait in seconds, not remote command runtime
        #[arg(long, default_value = "120", value_parser = positive_seconds)]
        timeout: f64,
    },
    /// Copy files; prefix exactly one path with :/ to select the remote side
    Cp {
        id: String,
        source: String,
        destination: String,
        #[arg(short, long)]
        recursive: bool,
        /// Maximum SSH readiness wait in seconds, not transfer time
        #[arg(long, default_value = "120", value_parser = positive_seconds)]
        timeout: f64,
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
        /// Optional instance ID (short TPU name or full ID), then deadline, cancel, or show
        #[arg(value_name = "ID/WHEN", num_args = 0..)]
        expiry_args: Vec<String>,
    },
}

impl Command {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Setup(_) => "setup",
            Self::Up(_) => "up",
            Self::Ls { .. } => "ls",
            Self::Wait { .. } => "wait",
            Self::Exec { .. } => "exec",
            Self::Cp { .. } => "cp",
            Self::Ssh { .. } => "ssh",
            Self::Cursor { .. } => "cursor",
            Self::Edit { .. } => "edit",
            Self::Stop { .. } => "stop",
            Self::Down { .. } => "down",
            Self::Expire { .. } => "expire",
        }
    }
}

#[derive(clap::Args, Debug, Default)]
pub struct SetupFlags {
    /// Choose a provider explicitly (otherwise open the provider picker)
    #[arg(long, value_parser = ["vast", "gcp"])]
    pub provider: Option<String>,
    /// Google Cloud project ID (otherwise choose from your projects)
    #[arg(long)]
    pub project: Option<String>,
    /// Deprecated for setup: choose the TPU zone at launch
    #[arg(long, hide = true)]
    pub zone: Option<String>,
    /// Cloud Tasks queue region (default: us-central1, or recorded setup region)
    #[arg(long)]
    pub location: Option<String>,
    /// Explicitly approve setup mutations in exactly this project
    #[arg(long, value_name = "PROJECT")]
    pub confirm_project: Option<String>,
    /// Remove only infrastructure recorded as added by gpu setup
    #[arg(long)]
    pub remove: bool,
    /// Show the setup plan without changing cloud resources or config
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(clap::Args, Clone, Debug)]
pub struct UpFlags {
    /// Provider (optional; TPU flags or a TPU picked on the GPU screen use gcp)
    #[arg(long, value_parser = ["vast", "gcp"])]
    pub provider: Option<String>,
    /// Full TPU accelerator type, e.g. v5e-1 or v6e-1
    #[arg(long, visible_alias = "type")]
    pub tpu: Option<String>,
    /// Google Cloud zone (overrides gcp.zone)
    #[arg(long)]
    pub zone: Option<String>,
    /// Override the TPU runtime image (default: chosen automatically by generation)
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
    /// Wait for working SSH after launch
    #[arg(long)]
    pub wait: bool,
    /// Maximum SSH readiness wait in seconds (with --wait)
    #[arg(long, default_value = "120", value_parser = positive_seconds)]
    pub timeout: f64,
    #[arg(long, allow_negative_numbers = true)]
    pub reliability: Option<f64>,
    /// include verified, unverified, and deverified hosts
    #[arg(long)]
    pub unverified: bool,
}

fn positive_seconds(s: &str) -> std::result::Result<f64, String> {
    let value: f64 = s.parse().map_err(|_| "expected seconds".to_string())?;
    if value.is_finite() && value > 0.0 && value <= 86400.0 {
        Ok(value)
    } else {
        Err("timeout must be greater than 0 and at most 86400 seconds".into())
    }
}

/// What argument parsing produced.
pub enum Parsed {
    Run(Box<Command>),
    RunJson(Box<Command>),
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
            command: Some(mut command),
            output_json,
            ..
        }) => {
            if let Command::Ls { json, .. } = &mut command {
                *json = output_json;
            }
            if output_json {
                Parsed::RunJson(Box::new(command))
            } else {
                Parsed::Run(Box::new(command))
            }
        }
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
