use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy)]
pub struct SubIds {
    pub uid: (u64, u64),
    pub gid: (u64, u64),
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct SubIdRange {
    start: u64,
    count: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct WorkspaceData {
    #[serde(default)]
    sub_uid: Option<SubIdRange>,
    #[serde(default)]
    sub_gid: Option<SubIdRange>,
}

pub struct Workspace {
    root: PathBuf,
    sub_ids: Option<SubIds>,
    _tempdir: Option<tempfile::TempDir>,
}

impl Workspace {
    const SUBDIRS: &[&str] = &["layers", "volumes", "run"];

    pub fn temporary() -> Workspace {
        let dir = tempfile::Builder::new()
            .prefix("cbuildrt-")
            .tempdir()
            .expect("failed to create temporary workspace");
        let root = dir.path().to_path_buf();
        for sub in Self::SUBDIRS {
            fs::create_dir_all(root.join(sub)).expect("failed to create workspace subdirs");
        }
        Workspace {
            root,
            sub_ids: None,
            _tempdir: Some(dir),
        }
    }

    pub fn load(path: &Path) -> Workspace {
        let json_path = path.join("workspace.json");
        let file = fs::File::open(&json_path).unwrap_or_else(|e| {
            panic!(
                "failed to open workspace metadata {}: {}",
                json_path.display(),
                e
            )
        });
        let data: WorkspaceData = serde_json::from_reader(file).unwrap_or_else(|e| {
            panic!(
                "failed to parse workspace metadata {}: {}",
                json_path.display(),
                e
            )
        });
        let sub_ids = match (data.sub_uid, data.sub_gid) {
            (Some(uid), Some(gid)) => Some(SubIds {
                uid: (uid.start, uid.count),
                gid: (gid.start, gid.count),
            }),
            (None, None) => None,
            _ => panic!(
                "workspace metadata at {} must specify both subUid and subGid or neither",
                json_path.display()
            ),
        };
        Workspace {
            root: path.to_path_buf(),
            sub_ids,
            _tempdir: None,
        }
    }

    pub fn init(path: &Path, sub_ids: Option<SubIds>) -> Workspace {
        fs::create_dir_all(path).expect("failed to create workspace directory");
        let json_path = path.join("workspace.json");
        if json_path.exists() {
            panic!(
                "workspace at {} is already initialized ({} exists)",
                path.display(),
                json_path.display()
            );
        }
        for sub in Self::SUBDIRS {
            fs::create_dir_all(path.join(sub)).expect("failed to create workspace subdir");
        }

        let data = WorkspaceData {
            sub_uid: sub_ids.map(|ids| SubIdRange {
                start: ids.uid.0,
                count: ids.uid.1,
            }),
            sub_gid: sub_ids.map(|ids| SubIdRange {
                start: ids.gid.0,
                count: ids.gid.1,
            }),
        };
        let file = fs::File::create(&json_path).expect("failed to create workspace.json");
        serde_json::to_writer_pretty(file, &data).expect("failed to write workspace.json");

        Workspace {
            root: path.to_path_buf(),
            sub_ids,
            _tempdir: None,
        }
    }

    pub fn layers_dir(&self) -> PathBuf {
        self.root.join("layers")
    }

    pub fn volumes_dir(&self) -> PathBuf {
        self.root.join("volumes")
    }

    pub fn run_dir(&self) -> PathBuf {
        self.root.join("run")
    }

    pub fn sub_ids(&self) -> Option<&SubIds> {
        self.sub_ids.as_ref()
    }

    pub fn purge_layers(&self) {
        let layers_dir = self.layers_dir();
        if layers_dir.exists() {
            for entry in std::fs::read_dir(&layers_dir).expect("failed to read layers directory") {
                let entry = entry.expect("failed to read directory entry");
                let path = entry.path();
                if path.is_dir() {
                    std::fs::remove_dir_all(&path).expect("failed to remove layer directory");
                }
            }
        }
    }
}

fn parse_subordinate_file(path: &str, username: &str) -> (u64, u64) {
    let content =
        fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {}", path, e));
    for line in content.lines() {
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        if parts.len() == 3 && parts[0] == username {
            let start: u64 = parts[1]
                .parse()
                .expect("invalid start in subordinate ID file");
            let count: u64 = parts[2]
                .parse()
                .expect("invalid count in subordinate ID file");
            return (start, count);
        }
    }
    panic!("no entry for user '{}' in {}", username, path);
}

pub fn auto_subordinate_range(file: &str) -> (u64, u64) {
    let euid = nix::unistd::geteuid();
    let user = nix::unistd::User::from_uid(euid)
        .expect("failed to look up user")
        .unwrap_or_else(|| panic!("no passwd entry for uid {}", euid));
    parse_subordinate_file(file, &user.name)
}
