//! Command-line definitions and usage rendering.

use clap::error::ErrorKind;
use clap::{ArgAction, CommandFactory, Parser, Subcommand};

use crate::provider::Provider;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

fn up_help() -> String {
    let codes = crate::error::help_codes(false);
    format!("Non-terminal example: gpu up --gpu 'RTX 4090' --mode container --no-repo --expire 2h --yes --json\nIDs: \
        numeric Vast ID; gcp:PROJECT:ZONE:NAME or an unambiguous TPU name; runpod:ID; do:REGION:ID.\nAMD providers support manual workspaces only (no --expire); use gpu offers --provider runpod|do.\nExit codes: 0 success; 1 error; 2 \
        usage; {codes}; 128+signal interrupted.")
}

fn run_help() -> String {
    let codes = crate::error::help_codes(true);
    format!("Example: gpu run --gpu 'RTX 4090' --expire 2h --yes --json -- python train.py\nNon-terminal launches \
        require --gpu, --expire and --yes (or --dry-run).\nExit codes: remote job status, or 1 error; 2 usage; \
        {codes}; 130 cancelled. IDs: numeric Vast or modal:sb-<id>.")
}

#[derive(Parser, Debug)]
#[command(
    name = "gpu",
    about = "GPU jobs and workspaces on Vast.ai, Google Cloud, Modal, Runpod and DigitalOcean",
    disable_version_flag = true,
    infer_long_args = true,
    args_override_self = true
)]
pub struct Cli {
    /// show program's version number and exit
    #[arg(short = 'v', short_alias = 'V', long = "version", action = ArgAction::SetTrue)]
    pub version: bool,

    /// Emit structured JSON without progress chatter (ls retains its legacy array)
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
    #[command(
        after_help = up_help()
    )]
    Up(UpFlags),
    /// Run a one-shot job on Vast or Modal (AMD providers: dry-run planning only)
    #[command(
        after_help = run_help()
    )]
    Run(RunFlags),
    /// Start, check, collect and cancel one-shot jobs in separate calls (Vast only)
    #[command(subcommand, after_help = job_help())]
    Job(JobCommand),
    /// List single-MI300X catalog prices and stock hints (not a capacity guarantee)
    Offers {
        /// AMD provider whose current catalog to read (runpod or do)
        #[arg(long, required = true, value_parser = Provider::parser(&[Provider::Runpod, Provider::DigitalOcean]))]
        provider: Provider,
    },
    /// List rented instances
    Ls {
        /// Filled from the global JSON flag
        #[arg(skip)]
        json: bool,
        /// Wrap JSON in the standard command envelope, including partial failures
        #[arg(long, requires = "output_json")]
        envelope: bool,
        /// Show full provider-qualified IDs
        #[arg(long)]
        long: bool,
        /// Limit listing to one provider
        #[arg(long, value_parser = Provider::parser(&Provider::ALL))]
        provider: Option<Provider>,
        /// Only list instances launched with this owner (see gpu up --owner)
        #[arg(long)]
        owner: Option<String>,
    },
    /// Wait until the instance answers a non-interactive SSH probe
    Wait {
        /// Numeric Vast ID, gcp:PROJECT:ZONE:NAME, or unambiguous TPU name
        id: String,
        /// Also wait for workspace setup to finish successfully
        #[arg(long)]
        wait_setup: bool,
        /// Maximum readiness wait in seconds
        #[arg(long, default_value = "120", value_parser = positive_seconds)]
        timeout: f64,
    },
    /// Execute a remote command without a terminal; preserve its exit code
    Exec {
        /// Numeric Vast ID, gcp:PROJECT:ZONE:NAME, unambiguous TPU name, or modal:sb-ID
        id: String,
        /// Remote program and arguments after -- (use sh -c for shell syntax)
        #[arg(required = true, last = true, num_args = 1..)]
        command: Vec<String>,
        /// Maximum SSH readiness wait in seconds, not remote command runtime
        #[arg(long, default_value = "120", value_parser = positive_seconds)]
        timeout: f64,
        /// Maximum remote command runtime in seconds; exit 20 on timeout
        #[arg(long, value_parser = positive_seconds)]
        command_timeout: Option<f64>,
    },
    /// Copy files; prefix exactly one path with :/ to select the remote side
    Cp {
        /// Numeric Vast ID, gcp:PROJECT:ZONE:NAME, unambiguous TPU name, modal:sb-ID, or modal:volume:NAME
        id: String,
        /// Local source path, or :/absolute/remote/path
        source: String,
        /// Local destination path, or :/absolute/remote/path
        destination: String,
        /// Copy directories recursively
        #[arg(short, long)]
        recursive: bool,
        /// Maximum SSH readiness wait in seconds, not transfer time
        #[arg(long, default_value = "120", value_parser = positive_seconds)]
        timeout: f64,
        /// Maximum transfer runtime in seconds; exit 20 on timeout
        #[arg(long, value_parser = positive_seconds)]
        command_timeout: Option<f64>,
    },
    /// Print or execute the SSH command
    Ssh {
        /// Numeric Vast ID, full GCP ID, short TPU name, or modal:sb-ID (picker if omitted)
        id: Option<String>,
        /// Replace this process with an interactive SSH connection
        #[arg(long)]
        exec: bool,
    },
    /// Open an instance workspace in an editor over SSH
    Edit {
        /// Numeric Vast ID, full GCP ID, or short TPU name (picker if omitted)
        id: Option<String>,
        /// Editor to launch (otherwise use the most recently selected editor)
        #[arg(long, value_parser = ["cursor", "code", "zed", "nvim"])]
        editor: Option<String>,
        /// Neovim config git URL or local directory
        #[arg(long)]
        nvim_config: Option<String>,
    },
    /// Stop an instance
    Stop {
        /// Numeric Vast ID, full GCP ID, or short TPU name (picker if omitted)
        id: Option<String>,
    },
    /// Destroy an instance
    Down {
        /// Vast ID, GCP ID/short TPU name, modal:sb-ID, or modal:volume:NAME (picker if omitted)
        id: Option<String>,
        /// Approve permanent destruction without a prompt (required without a terminal)
        #[arg(long)]
        yes: bool,
        /// Destroy a linked instance even if its latest changes could not be pulled first
        #[arg(long)]
        discard_unsynced: bool,
    },
    /// Pull GPU changes into the local directory an instance is linked to
    #[command(after_help = SYNC_HELP)]
    Sync(SyncFlags),
    /// Link a running instance to a local directory: copy it up once, then mirror the GPU copy back
    Link(LinkFlags),
    /// The background syncer that `--autosync` starts; not meant to be run by hand
    #[command(name = "autosync-run", hide = true)]
    AutosyncRun(AutosyncRunFlags),
    /// Stop syncing a local directory with an instance (files stay where they are)
    Unlink {
        /// Instance ID (picker if omitted)
        id: Option<String>,
        /// Local directory (default: the current directory)
        #[arg(long)]
        dir: Option<String>,
    },
    /// Schedule destruction, show or cancel it
    #[command(
        override_usage = "gpu expire [ID] WHEN|cancel|show",
        long_about = "Auto-destroy (disk deleted). WHEN: 2h, 1h30m, 18:30, 6:30PM, 6pm, or ISO datetime. Clock times use the \
            next local occurrence; bare numbers are instance IDs.",
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
            Self::Run(_) => "run",
            Self::Job(action) => action.name(),
            Self::Ls { .. } => "ls",
            Self::Offers { .. } => "offers",
            Self::Wait { .. } => "wait",
            Self::Exec { .. } => "exec",
            Self::Cp { .. } => "cp",
            Self::Ssh { .. } => "ssh",
            Self::Edit { .. } => "edit",
            Self::Stop { .. } => "stop",
            Self::Down { .. } => "down",
            Self::Expire { .. } => "expire",
            Self::Sync(_) => "sync",
            Self::Link(_) => "link",
            Self::Unlink { .. } => "unlink",
            Self::AutosyncRun(_) => "autosync-run",
        }
    }
}

fn job_help() -> String {
    let codes = crate::error::help_codes(true);
    format!("A job is a gpu run split into separate calls, so a caller never has to wait for it:\n\
        gpu job start --gpu 'RTX 4090' --expire 2h --yes --json -- python train.py   (prints the job ID once the job runs)\n\
        gpu job status ID; gpu job logs ID [--follow]; gpu job outputs ID; gpu job cancel ID; gpu job ls [--owner NAME]\n\
        Job records live in ~/.local/state/gpu/jobs, so any later gpu process can resume a job by ID. An uncollected job's \
        instance is destroyed at --expire; keep outputs by running gpu job outputs ID before then.\n\
        Exit codes: 0 success; 1 error; 2 usage; {codes}.")
}

/// `gpu job`: the pieces of `gpu run`, one call each.
#[derive(Subcommand, Debug)]
#[allow(clippy::large_enum_variant)] // like Command, which holds RunFlags inline
pub enum JobCommand {
    /// Rent, upload and launch like gpu run, then return the job ID while the job runs
    #[command(
        after_help = "Takes the flags of gpu run. --linger keeps the instance that long after gpu job outputs; \
        the default destroys it. Example: gpu job start --gpu 'RTX 4090' --expire 2h --yes --json -- python train.py"
    )]
    Start(RunFlags),
    /// Show a job's state, exit code, elapsed time and estimated cost
    Status {
        /// Job ID from gpu job start (or its Vast instance ID)
        id: String,
    },
    /// Print a job's output so far
    Logs {
        /// Job ID from gpu job start (or its Vast instance ID)
        id: String,
        /// Keep streaming until the job ends
        #[arg(long, short = 'f')]
        follow: bool,
        /// Start at this byte offset (the next_offset of an earlier call)
        #[arg(long, default_value_t = 0, value_name = "BYTES")]
        offset: u64,
    },
    /// Download a finished job's outputs, then destroy its instance unless it was started with --linger
    Outputs {
        /// Job ID from gpu job start (or its Vast instance ID)
        id: String,
    },
    /// Stop a job and destroy its instance
    Cancel {
        /// Job ID from gpu job start (or its Vast instance ID)
        id: String,
    },
    /// List jobs with their state
    Ls {
        /// Only list jobs started with this owner (see gpu run --owner)
        #[arg(long)]
        owner: Option<String>,
    },
}

impl JobCommand {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Start(_) => "job start",
            Self::Status { .. } => "job status",
            Self::Logs { .. } => "job logs",
            Self::Outputs { .. } => "job outputs",
            Self::Cancel { .. } => "job cancel",
            Self::Ls { .. } => "job ls",
        }
    }
}

const SYNC_HELP: &str = "The GPU copy leads: edit on the instance (Zed or VS Code remote, or Neovim over SSH) and sync pulls its changes here. \
A local file edited since the last sync is never overwritten: it stays, and the GPU version is saved beside it as NAME.gpu-ID-TIME.EXT. \
Local files are never deleted. Gitignored GPU files (outputs/) are pulled only with --include.\n\
Autosync does the same as --watch in the background, surviving a closed terminal, laptop sleep and dropped connections: \
gpu sync --autosync starts it, gpu sync --no-autosync stops it, gpu sync shows whether it runs and its recent errors; its log is under ~/.local/state/gpu/sync.\n\
Examples: gpu sync; gpu sync --watch; gpu sync --autosync; gpu sync 12345678 --include outputs; gpu sync --list";

#[derive(clap::Args, Clone, Debug, Default)]
pub struct SyncFlags {
    /// Instance ID (default: the instance linked to the current directory; picker if several)
    pub id: Option<String>,
    /// Local directory (default: the current directory)
    #[arg(long)]
    pub dir: Option<String>,
    /// Keep pulling as the GPU copy changes until Ctrl-C
    #[arg(long, conflicts_with_all = ["list", "autosync", "no_autosync"])]
    pub watch: bool,
    /// Start autosync: keep pulling in the background until the instance or link is gone
    #[arg(long, conflicts_with_all = ["list", "no_autosync"])]
    pub autosync: bool,
    /// Stop the background autosync of this link (after pulling what it has not seen)
    #[arg(long, conflicts_with = "list")]
    pub no_autosync: bool,
    /// Only show linked instances and whether they are in sync; pull nothing
    #[arg(long)]
    pub list: bool,
    /// Also pull this gitignored path (repeatable, remembered for the link)
    #[arg(long, value_name = "PATH")]
    pub include: Vec<String>,
    /// Do not pull files larger than this (a terminal asks first), e.g. 500M or 2G
    #[arg(long, default_value = "256M")]
    pub max_file: String,
}

#[derive(clap::Args, Clone, Debug, Default)]
pub struct LinkFlags {
    /// Instance ID (picker if omitted)
    pub id: Option<String>,
    /// Local directory (default: the current directory)
    #[arg(long)]
    pub dir: Option<String>,
    /// Also pull this gitignored path (repeatable, remembered for the link)
    #[arg(long, value_name = "PATH")]
    pub include: Vec<String>,
    /// Maximum size of the one-time copy up, e.g. 500M or 1G
    #[arg(long, default_value = "1G")]
    pub max_upload: String,
    /// Approve uploading over files already in the instance's directory
    #[arg(long)]
    pub yes: bool,
    /// Maximum SSH readiness wait in seconds
    #[arg(long, default_value = "120", value_parser = positive_seconds)]
    pub timeout: f64,
    /// Keep pulling the GPU copy into the directory in the background (see gpu sync)
    #[arg(long)]
    pub autosync: bool,
}

/// The detached syncer's arguments.
#[derive(clap::Args, Clone, Debug)]
pub struct AutosyncRunFlags {
    /// Instance ID of the link
    pub id: String,
    /// The link's local directory
    #[arg(long)]
    pub dir: String,
    /// Do not pull files larger than this
    #[arg(long, default_value = "256M")]
    pub max_file: String,
}

#[derive(clap::Args, Debug, Default)]
pub struct SetupFlags {
    /// Choose a provider explicitly (otherwise open the provider picker)
    #[arg(long, value_parser = Provider::parser(&Provider::ALL))]
    pub provider: Option<Provider>,
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
    /// Provider (default: vast; TPU flags imply gcp; AMD workspaces have no expiry)
    #[arg(long, value_parser = Provider::parser(&[Provider::Vast, Provider::Gcp, Provider::Runpod, Provider::DigitalOcean]))]
    pub provider: Option<Provider>,
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
    /// Attach a local directory: copy it up once, then mirror the GPU copy back (see gpu sync)
    #[arg(long, value_name = "PATH", conflicts_with_all = ["repo", "no_repo"])]
    pub dir: Option<String>,
    /// With --dir: also pull this gitignored path (repeatable)
    #[arg(long, value_name = "PATH", requires = "dir")]
    pub include: Vec<String>,
    /// With --dir: maximum size of the one-time copy up
    #[arg(long, default_value = "1G")]
    pub max_upload: String,
    /// With --dir: keep pulling the GPU copy into the directory in the background (a terminal is asked otherwise)
    #[arg(long, requires = "dir")]
    pub autosync: bool,
    /// Workspace type (required for non-terminal Vast launches)
    #[arg(long, value_parser = ["container", "vm"])]
    pub mode: Option<String>,
    /// GPU model, e.g. 'RTX 4090' (required without a terminal)
    #[arg(long)]
    pub gpu: Option<String>,
    /// Number of GPUs per instance (at least 1)
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    pub num_gpus: i64,
    /// Maximum total compute price in USD per hour
    #[arg(long, allow_negative_numbers = true)]
    pub max_price: Option<f64>,
    /// Use interruptible bid pricing instead of on-demand
    #[arg(long)]
    pub interruptible: bool,
    /// Select a specific matching Vast offer ID
    #[arg(long, allow_negative_numbers = true)]
    pub offer: Option<i64>,
    /// Instance disk size in GB (at least 1; otherwise config default)
    #[arg(long, allow_negative_numbers = true)]
    pub disk: Option<i64>,
    /// Approve renting and choose the cheapest matching offer
    #[arg(long)]
    pub yes: bool,
    /// Show the exact create request without renting (offer search still runs)
    #[arg(long)]
    pub dry_run: bool,
    /// Auto-destroy after e.g. 2h or 1h30m (includes boot time)
    #[arg(long, value_name = "DURATION")]
    pub expire: Option<String>,
    /// Wait for working SSH after launch
    #[arg(long)]
    pub wait: bool,
    /// Also wait for workspace setup; implies --wait
    #[arg(long)]
    pub wait_setup: bool,
    /// Maximum SSH readiness wait in seconds (with --wait)
    #[arg(long, default_value = "120", value_parser = positive_seconds)]
    pub timeout: f64,
    /// Minimum host reliability from 0 to 1
    #[arg(long, allow_negative_numbers = true)]
    pub reliability: Option<f64>,
    /// include verified, unverified, and deverified hosts
    #[arg(long)]
    pub unverified: bool,
    /// Label the instance with who launched it (default: $GPU_OWNER); shown by gpu ls
    #[arg(long)]
    pub owner: Option<String>,
}

impl UpFlags {
    /// Whether any flag that only a TPU launch reads is present.
    pub fn tpu_flags(&self) -> bool {
        self.tpu.is_some()
            || self.zone.is_some()
            || self.runtime.is_some()
            || self.spot
            || self.on_demand
    }

    /// The launch's provider: `--provider`, else Google Cloud when TPU flags
    /// are present, else Vast.
    pub fn target(&self) -> Provider {
        self.provider.unwrap_or(if self.tpu_flags() {
            Provider::Gcp
        } else {
            Provider::Vast
        })
    }
}

#[derive(clap::Args, Clone, Debug)]
pub struct RunFlags {
    /// Provider (default: vast; runpod/do support only --dry-run)
    #[arg(long, value_parser = Provider::parser(&[Provider::Vast, Provider::Modal, Provider::Runpod, Provider::DigitalOcean]))]
    pub provider: Option<Provider>,
    /// Modal physical CPU cores (default: 4; request equals limit)
    #[arg(long, allow_negative_numbers = true)]
    pub cpu: Option<f64>,
    /// Modal memory in GiB (default: 16; request equals limit)
    #[arg(long, allow_negative_numbers = true)]
    pub memory: Option<f64>,
    /// GPU model, e.g. 'RTX 4090' (required without a terminal)
    #[arg(long)]
    pub gpu: Option<String>,
    /// Number of GPUs per instance (at least 1)
    #[arg(long, default_value_t = 1, allow_negative_numbers = true)]
    pub num_gpus: i64,
    /// Maximum total compute price in USD per hour
    #[arg(long, allow_negative_numbers = true)]
    pub max_price: Option<f64>,
    /// Select a specific matching Vast offer ID
    #[arg(long, allow_negative_numbers = true)]
    pub offer: Option<i64>,
    /// Instance disk size in GB (at least 1; otherwise config default)
    #[arg(long, allow_negative_numbers = true)]
    pub disk: Option<i64>,
    /// Minimum host reliability from 0 to 1
    #[arg(long, allow_negative_numbers = true)]
    pub reliability: Option<f64>,
    /// Include unverified and deverified hosts
    #[arg(long)]
    pub unverified: bool,
    /// Label the job with who launched it (default: $GPU_OWNER); shown by gpu ls
    #[arg(long)]
    pub owner: Option<String>,
    /// Approve renting and choose the cheapest matching offer
    #[arg(long)]
    pub yes: bool,
    /// Inspect the upload and exact create request without renting
    #[arg(long)]
    pub dry_run: bool,
    /// Lifetime ceiling including boot, upload and download (prompted in a terminal when unset)
    #[arg(long, value_name = "DURATION")]
    pub expire: Option<String>,
    /// Readiness timeout in seconds (SSH, or Modal image build/pull and scheduling), not job runtime [default: 600; Modal: 1200]
    #[arg(long, value_parser = positive_seconds)]
    pub timeout: Option<f64>,
    /// Override the container image
    #[arg(long)]
    pub image: Option<String>,
    /// Shell command to prepare dependencies before the job
    #[arg(long, conflicts_with = "no_setup")]
    pub setup: Option<String>,
    /// Skip automatic dependency setup
    #[arg(long)]
    pub no_setup: bool,
    /// Permit uploading secret-looking files (refused by default)
    #[arg(long)]
    pub allow_secrets: bool,
    /// Also upload this file (not a directory) even if git ignores or does not track it, e.g. gpu-runs/job.sh (repeatable)
    #[arg(long, value_name = "PATH")]
    pub include: Vec<String>,
    /// Take the code from this git repository (GitHub owner/name, name under your owner, or URL) instead of the current folder; needs --ref
    #[arg(long, value_name = "REPO", requires = "git_ref")]
    pub source_repo: Option<String>,
    /// Commit, branch or tag of --source-repo to upload; resolved locally to a full commit SHA
    #[arg(long = "ref", value_name = "REF", requires = "source_repo")]
    pub git_ref: Option<String>,
    /// Results directory relative to the job's working directory
    #[arg(long, default_value = "outputs")]
    pub outputs: String,
    /// Forward a local variable over transport stdin, never provider metadata
    #[arg(long = "env", value_name = "NAME")]
    pub env: Vec<String>,
    /// Maximum upload size, e.g. 500M or 1G
    #[arg(long, default_value = "1G")]
    pub max_upload: String,
    /// Opt-in unattended result retention after job finish (default: destroy immediately)
    #[arg(long, default_value = "0")]
    pub linger: String,
    /// Remote program and arguments after -- (use sh -c for shell syntax)
    #[arg(required = true, last = true, num_args = 1..)]
    pub command: Vec<String>,
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
            if let Command::Ls { json, envelope, .. } = &mut command {
                *json = output_json;
                if !*envelope {
                    return Parsed::Run(Box::new(command));
                }
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

/// Global --json accepts the same unambiguous prefixes as clap. Stop at -- so
/// a remote command's flags cannot change the local output protocol.
pub(crate) fn wants_json(argv: &[String]) -> bool {
    argv.iter()
        .skip(1)
        .take_while(|a| a.as_str() != "--")
        .any(|a| {
            let flag = a.split('=').next().unwrap_or("");
            flag.starts_with("--j") && "--json".starts_with(flag)
        })
}

pub(crate) fn parsed_command_name(argv: &[String]) -> String {
    let Ok(matches) = Cli::command()
        .ignore_errors(true)
        .try_get_matches_from(argv)
    else {
        return "parse".into();
    };
    matches.subcommand_name().unwrap_or("parse").into()
}

/// A usage error raised after parsing, rendered
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
