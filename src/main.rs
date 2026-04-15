use clap::crate_version;

use runtime::{run, Config};
use std::fs::File;
use std::path::PathBuf;

mod runtime;

// Returns the config and the workspace directory (if one is passed).
// TODO: This function does not really perform error checking;
//       for now, we assume that xbstrap passes sane values.
fn make_config_from_cli() -> (Config, Option<PathBuf>) {
    let matches = clap::App::new("cbuildrt")
        .version(crate_version!())
        .arg(
            clap::Arg::with_name("workspace")
                .long("workspace")
                .value_name("DIR")
                .help("Directory to store cbuildrt's data (such as overlayfs layers)")
                .takes_value(true),
        )
        .arg(
            clap::Arg::with_name("cbuild-json")
                .help("cbuild.json file")
                .required(true),
        )
        .get_matches();

    let workspace = matches.value_of("workspace").map(PathBuf::from);

    let cfg_f =
        File::open(matches.value_of("cbuild-json").unwrap()).expect("unable to open cbuild.json");

    let cfg = serde_json::from_reader(cfg_f).expect("failed to parse cbuild.json");
    (cfg, workspace)
}

fn main() {
    let (cfg, workspace) = make_config_from_cli();
    run(cfg, workspace);
}
