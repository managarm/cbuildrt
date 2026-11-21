use clap::{error::ErrorKind, Args, CommandFactory, Parser, Subcommand};
use nix::errno::Errno;
use nix::fcntl::FlockArg;
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
    /// Manage persistent volumes in the workspace
    Volume(VolumeCommandArgs),
    /// Manage named layers in the workspace
    Layer(LayerCommandArgs),
}

#[derive(Args)]
struct LayerCommandArgs {
    #[command(subcommand)]
    command: LayerCommand,
}

#[derive(Subcommand)]
enum LayerCommand {
    /// List layers in the workspace
    Ls,
    /// Remove a layer from the workspace unless it is in use
    Rm(LayerRmArgs),
}

#[derive(Args)]
struct LayerRmArgs {
    /// Name of the layer to remove
    name: String,
}

#[derive(Args)]
struct VolumeCommandArgs {
    #[command(subcommand)]
    command: VolumeCommand,
}

#[derive(Subcommand)]
enum VolumeCommand {
    /// List volumes in the workspace
    Ls,
    /// Remove a volume from the workspace
    Rm(VolumeRmArgs),
}

#[derive(Args)]
struct VolumeRmArgs {
    /// Name of the volume to remove
    name: String,
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
        Some(Command::Volume(args)) => {
            do_volume_subcmd(cli.workspace.as_deref(), &args);
        }
        Some(Command::Layer(args)) => {
            do_layer_subcmd(cli.workspace.as_deref(), &args);
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

fn do_volume_subcmd(workspace_path: Option<&Path>, args: &VolumeCommandArgs) {
    let workspace_path = workspace_path.unwrap_or_else(|| {
        Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "--workspace is required for volume",
            )
            .exit();
    });
    let workspace = Workspace::load(workspace_path);
    match &args.command {
        VolumeCommand::Ls => {
            for name in workspace.list_volumes() {
                println!("{}", name);
            }
        }
        VolumeCommand::Rm(rm) => {
            if !workspace::is_valid_volume_name(&rm.name) {
                Cli::command()
                    .error(
                        ErrorKind::InvalidValue,
                        format!("invalid volume name '{}'", rm.name),
                    )
                    .exit();
            }
            let exit_code = unsafe {
                runtime::run_userns(&workspace, None, None, || {
                    workspace.remove_volume(&rm.name);
                    exit(0);
                })
            };
            exit(exit_code);
        }
    }
}

fn do_layer_subcmd(workspace_path: Option<&Path>, args: &LayerCommandArgs) {
    let workspace_path = workspace_path.unwrap_or_else(|| {
        Cli::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "--workspace is required for layer",
            )
            .exit();
    });
    let workspace = Workspace::load(workspace_path);
    match &args.command {
        LayerCommand::Ls => {
            for name in workspace.list_layers() {
                println!("{}", name);
            }
        }
        LayerCommand::Rm(rm) => {
            if !workspace::is_valid_layer_name(&rm.name) {
                Cli::command()
                    .error(
                        ErrorKind::InvalidValue,
                        format!("invalid layer name '{}'", rm.name),
                    )
                    .exit();
            }
            let exit_code = unsafe {
                runtime::run_userns(&workspace, None, None, || {
                    // Lock inside the user namespace (see runtime::run()).
                    match workspace.lock_layer(&rm.name, FlockArg::LockExclusiveNonblock) {
                        // The lock is held until the layer is gone.
                        Ok(Some(_lock)) => workspace.remove_layer(&rm.name),
                        Ok(None) => {}
                        Err(Errno::EWOULDBLOCK) => {
                            eprintln!("cbuildrt: layer '{}' is in use", rm.name);
                            exit(1);
                        }
                        Err(e) => panic!("failed to lock layer: {}", e),
                    }
                    exit(0);
                })
            };
            exit(exit_code);
        }
    }
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
