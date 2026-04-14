use clap::{error::ErrorKind, Args, CommandFactory, Parser, Subcommand};
use std::fs::File;
use std::path::{Path, PathBuf};

use runtime::{run, Config};

mod runtime;

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
    /// Run a container from a cbuild.json description
    Run(RunCommandArgs),
}

#[derive(Args)]
struct RunCommandArgs {
    /// cbuild.json file
    cbuild_json: PathBuf,
}

fn main() {
    let cli = Cli::parse();
    match cli.command {
        Some(Command::Run(args)) => {
            do_run_subcmd(cli.workspace, &args.cbuild_json);
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
            do_run_subcmd(cli.workspace, &cbuild_json);
        }
    }
}

fn do_run_subcmd(workspace: Option<PathBuf>, cbuild_json: &Path) {
    let cfg_f = File::open(cbuild_json).expect("unable to open cbuild.json");
    let cfg: Config = serde_json::from_reader(cfg_f).expect("failed to parse cbuild.json");
    run(cfg, workspace);
}
