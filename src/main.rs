use clap::{error::ErrorKind, Args, CommandFactory, Parser, Subcommand};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::exit;

use runtime::{run, Config};
use workspace::{SubIds, Workspace};

mod runtime;
mod util;
mod workspace;

#[derive(Parser)]
#[command(name = "cbuildrt", version, subcommand_precedence_over_arg = true)]
struct Cli {
    #[arg(
        long,
        value_name = "DIR",
        help = "Directory to store cbuildrt's data (such as overlayfs layers)"
    )]
    workspace: Option<PathBuf>,

    /// cbuild.json file (legacy invocation without subcommand)
    cbuild_json: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Initialize a workspace
    Init(InitCommandArgs),
    /// Run a container from a cbuild.json description
    Run(RunCommandArgs),
    /// Purge extracted layers from the workspace
    Purge,
}

#[derive(Args)]
struct RunCommandArgs {
    /// cbuild.json file
    cbuild_json: PathBuf,
}

#[derive(Args)]
struct InitCommandArgs {
    #[arg(long)]
    pub no_sub_ids: bool,
    #[arg(
        long,
        value_name = "START",
        conflicts_with = "no_sub_ids",
        requires = "sub_uid_count"
    )]
    pub sub_uid_start: Option<u64>,
    #[arg(
        long,
        value_name = "COUNT",
        conflicts_with = "no_sub_ids",
        requires = "sub_uid_start"
    )]
    pub sub_uid_count: Option<u64>,
    #[arg(
        long,
        value_name = "START",
        conflicts_with = "no_sub_ids",
        requires = "sub_gid_count"
    )]
    pub sub_gid_start: Option<u64>,
    #[arg(
        long,
        value_name = "COUNT",
        conflicts_with = "no_sub_ids",
        requires = "sub_gid_start"
    )]
    pub sub_gid_count: Option<u64>,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Init(args)) => {
            do_init_subcmd(cli.workspace.as_deref(), &args);
        }
        Some(Command::Run(args)) => {
            do_run_subcmd(cli.workspace.as_deref(), &args.cbuild_json);
        }
        Some(Command::Purge) => {
            do_purge_subcmd(cli.workspace.as_deref());
        }
        None => {
            let cbuild_json = cli.cbuild_json.unwrap_or_else(|| {
                Cli::command()
                    .error(
                        ErrorKind::MissingRequiredArgument,
                        "cbuild.json is required if no subcommand is passed",
                    )
                    .exit();
            });
            do_run_subcmd(cli.workspace.as_deref(), &cbuild_json);
        }
    }
}

fn do_init_subcmd(workspace_path: Option<&Path>, args: &InitCommandArgs) {
    let workspace = workspace_path.unwrap_or_else(|| {
        Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "--workspace is required for init",
            )
            .exit();
    });

    let sub_ids = if args.no_sub_ids {
        None
    } else {
        let uid = match (args.sub_uid_start, args.sub_uid_count) {
            (Some(start), Some(count)) => (start, count),
            (None, None) => workspace::auto_subordinate_range("/etc/subuid"),
            _ => panic!("need both sub_uid_start and sub_uid_count"),
        };
        let gid = match (args.sub_gid_start, args.sub_gid_count) {
            (Some(start), Some(count)) => (start, count),
            (None, None) => workspace::auto_subordinate_range("/etc/subgid"),
            _ => panic!("need both sub_gid_start and sub_gid_count"),
        };
        Some(SubIds { uid, gid })
    };

    Workspace::init(workspace, sub_ids);
}

fn do_purge_subcmd(workspace_path: Option<&Path>) {
    let workspace_path = workspace_path.unwrap_or_else(|| {
        Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "--workspace is required for purge",
            )
            .exit();
    });
    let workspace = Workspace::load(workspace_path);
    let exit_code = unsafe {
        runtime::run_userns(&workspace, None, None, || {
            workspace.purge_layers();
            exit(0);
        })
    };
    exit(exit_code);
}

fn do_run_subcmd(workspace_path: Option<&Path>, cbuild_json: &Path) {
    let cfg_f = File::open(cbuild_json).expect("unable to open cbuild.json");
    let cfg: Config = serde_json::from_reader(cfg_f).expect("failed to parse cbuild.json");

    let workspace = match workspace_path {
        Some(p) => Workspace::load(p),
        None => Workspace::temporary(),
    };

    let exit_code = unsafe { run(cfg, workspace) };
    std::process::exit(exit_code);
}
