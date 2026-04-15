use libc::{gid_t, uid_t};
use nix::fcntl::{flock, open, FlockArg, OFlag};
use nix::sys::stat::Mode;
use rustix::fd::AsFd;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::ffi::CString;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::process::exit;

#[derive(Serialize, Deserialize)]
struct BindMount {
    destination: PathBuf,
    source: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct Volume {
    name: String,
    destination: PathBuf,
}

#[derive(Serialize, Deserialize)]
struct User {
    uid: uid_t,
    gid: gid_t,
}

#[derive(Serialize, Deserialize)]
struct Process {
    args: Vec<String>,
    #[serde(default)]
    environ: HashMap<String, String>,
}

#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RootFs {
    Path(PathBuf),
    Overlay {
        layers: Vec<PathBuf>,
        #[serde(default, rename = "withUpper")]
        with_upper: bool,
        #[serde(default, rename = "extractUpper")]
        extract_upper: Option<PathBuf>,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SubIdRange {
    #[serde(default)]
    auto: bool,
    #[serde(default)]
    start: u64,
    #[serde(default)]
    count: u64,
    #[serde(rename = "self")]
    self_id: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
pub struct Config {
    // If no rootfs is passed, we are running in namespace only mode.
    // That is, we will still enter namespace but not chroot().
    #[serde(default)]
    rootfs: Option<RootFs>,
    user: User,
    process: Process,
    #[serde(default)]
    isolate_network: bool,
    bind_mounts: Vec<BindMount>,
    #[serde(default)]
    volumes: Vec<Volume>,
    #[serde(default)]
    sub_uid: Option<SubIdRange>,
    #[serde(default)]
    sub_gid: Option<SubIdRange>,
    // Do not chroot() into the rootfs. Note that we still chdir() into it.
    // This is useful to build container base images by using host tools (e.g., debootstrap).
    #[serde(default)]
    no_chroot: bool,
    // Do not perform system mounts such as /proc, /tmp, /sys etc.
    #[serde(default)]
    no_system_mounts: bool,
}

// Concatenates lhs and rhs as-if the rhs was a relative path.
fn concat_absolute<L: AsRef<Path>, R: AsRef<Path>>(lhs: L, rhs: R) -> PathBuf {
    lhs.as_ref().join(rhs.as_ref().strip_prefix("/").unwrap())
}

fn resolve_tar_layer(workspace: Option<&Path>, path: &Path) -> PathBuf {
    let ext = path.extension().and_then(|e| e.to_str());
    if ext != Some("tar") {
        return path.to_path_buf();
    }

    let stem = path
        .file_stem()
        .expect("filename of tar lower layer has empty prefix");

    let workspace =
        workspace.expect("--workspace is required when an overlay uses tar lower layers");
    let layers_root = workspace.join("layers");

    // If the tar layer is already extracted, re-use the extracted version.
    let extracted_dir = layers_root.join(stem);
    if extracted_dir.exists() {
        return extracted_dir;
    }

    // Extract into a staging directory.
    std::fs::create_dir_all(&layers_root).expect("failed to create layer cache directory");
    let staging = tempfile::Builder::new()
        .prefix(".tmp-")
        .tempdir_in(&layers_root)
        .expect("failed to create staging dir for tar layer extraction");

    let tar_file = File::open(path).expect("failed to open tar layer");
    let mut archive = tar::Archive::new(tar_file);
    archive.set_preserve_permissions(true);
    archive.set_preserve_ownerships(true);
    archive
        .unpack(staging.path())
        .expect("failed to extract tar layer");

    // Rename to the final directory name.
    let staging_path = staging.keep();
    std::fs::rename(&staging_path, &extracted_dir)
        .expect("failed to move extracted tar layer into cache");

    extracted_dir
}

fn run_init(cfg: &Config, workspace: Option<&Path>, run_dir: Option<&Path>) -> ! {
    // Derive the rootfs path.
    let rootfs_owned: Option<PathBuf> = match &cfg.rootfs {
        Some(RootFs::Path(path)) => Some(path.clone()),
        Some(RootFs::Overlay { .. }) => {
            let merged = run_dir.unwrap().join("merged");
            std::fs::create_dir(&merged).expect("failed to create overlay merged dir");
            Some(merged)
        }
        None => None,
    };
    let rootfs = rootfs_owned.as_deref();

    // We can now set up the remaining namespaces and perform mounts.
    let mut clone_flags = nix::sched::CloneFlags::CLONE_NEWNS;
    if cfg.isolate_network {
        clone_flags |= nix::sched::CloneFlags::CLONE_NEWNET;
    }
    nix::sched::unshare(clone_flags).expect("failed to unshare()");

    // Skip rootfs setup if we are running in namespace only mode.
    if let Some(rootfs) = rootfs {
        if let Some(RootFs::Overlay {
            layers, with_upper, ..
        }) = &cfg.rootfs
        {
            let resolved_layers: Vec<PathBuf> = layers
                .iter()
                .map(|p| resolve_tar_layer(workspace, p))
                .collect();

            let mount =
                rustix::mount::fsopen("overlay", rustix::mount::FsOpenFlags::FSOPEN_CLOEXEC)
                    .expect("failed to open overlay filesystem");
            for path in &resolved_layers {
                rustix::mount::fsconfig_set_string(&mount, "lowerdir+", path)
                    .expect("failed to set overlay lowerdir option");
            }
            if *with_upper {
                let run_dir =
                    run_dir.expect("withUpper is set but no overlay tempdir was provided");
                let upper = run_dir.join("upper");
                let work = run_dir.join("work");
                std::fs::create_dir(&upper).expect("failed to create overlay upper dir");
                std::fs::create_dir(&work).expect("failed to create overlay work dir");
                rustix::mount::fsconfig_set_string(&mount, "upperdir", &upper)
                    .expect("failed to set overlay upperdir option");
                rustix::mount::fsconfig_set_string(&mount, "workdir", &work)
                    .expect("failed to set overlay workdir option");
                rustix::mount::fsconfig_set_flag(&mount, "userxattr")
                    .expect("failed to set overlay userxattr option");
            }

            rustix::mount::fsconfig_create(&mount).expect("failed to create overlay filesystem");

            let mount = rustix::mount::fsmount(
                &mount,
                rustix::mount::FsMountFlags::FSMOUNT_CLOEXEC,
                rustix::mount::MountAttrFlags::empty(),
            )
            .expect("failed to mount overlay filesystem");

            rustix::mount::move_mount(
                &mount,
                "",
                rustix::fs::CWD,
                rootfs,
                rustix::mount::MoveMountFlags::MOVE_MOUNT_F_EMPTY_PATH,
            )
            .expect("failed to move overlay filesystem to rootfs");
        } else {
            // First, we need to get a read-only rootfs.
            // Mounting with MS_BIND ignored MS_RDONLY, but MS_REMOUNT respects it.
            nix::mount::mount(
                Some(rootfs),
                rootfs,
                None::<&str>,
                nix::mount::MsFlags::MS_BIND,
                None::<&str>,
            )
            .expect("failed to bind mount rootfs to itself");

            // The fs might be mounted as nosuid/nodev and we will not have permissions
            // to strip these mount options.
            // Instead of parsing the current mount table, just set these flags unconditionally for now.
            nix::mount::mount(
                Some(rootfs),
                rootfs,
                None::<&str>,
                nix::mount::MsFlags::MS_REMOUNT
                    | nix::mount::MsFlags::MS_BIND
                    | nix::mount::MsFlags::MS_RDONLY
                    | nix::mount::MsFlags::MS_NOSUID
                    | nix::mount::MsFlags::MS_NODEV,
                None::<&str>,
            )
            .expect("failed to make rootfs read-only");
        }

        // Perform mounts of /dev, /dev/pts, /dev/shm, /run, /tmp, /var/tmp, /sys, and /proc.
        if !cfg.no_system_mounts {
            let dev_overlays = vec!["tty", "null", "zero", "full", "random", "urandom"];
            for f in dev_overlays {
                nix::mount::mount(
                    Some(&Path::new("/dev/").join(f)),
                    &concat_absolute(rootfs, "/dev/").join(f),
                    None::<&str>,
                    nix::mount::MsFlags::MS_BIND,
                    None::<&str>,
                )
                .expect("failed to mount device");
            }

            if !cfg.isolate_network {
                nix::mount::mount(
                    Some(&std::fs::canonicalize("/etc/resolv.conf").unwrap()),
                    &concat_absolute(rootfs, "/etc/resolv.conf"),
                    None::<&str>,
                    nix::mount::MsFlags::MS_BIND,
                    None::<&str>,
                )
                .expect("failed to mount /etc/resolv.conf");
            }

            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/dev/pts"),
                Some("devpts"),
                nix::mount::MsFlags::empty(),
                None::<&str>,
            )
            .expect("failed to mount /dev/pts");

            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/dev/shm"),
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                None::<&str>,
            )
            .expect("failed to mount /dev/shm");

            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/run"),
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                None::<&str>,
            )
            .expect("failed to mount /run");

            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/tmp"),
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                None::<&str>,
            )
            .expect("failed to mount /tmp");

            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/proc"),
                Some("proc"),
                nix::mount::MsFlags::empty(),
                None::<&str>,
            )
            .expect("failed to mount /proc");

            // Mount /var/tmp as tmpfs.
            // TODO: Technically /var/tmp is supposed to survive boots.
            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/var/tmp"),
                Some("tmpfs"),
                nix::mount::MsFlags::empty(),
                None::<&str>,
            )
            .expect("failed to mount /var/tmp");

            // Mount /sys via recursive bind + slave.
            // This is needed such that the container can access /sys/fs/* and similar.
            nix::mount::mount(
                Some(Path::new("/sys")),
                &concat_absolute(rootfs, "/sys"),
                None::<&str>,
                nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
                None::<&str>,
            )
            .expect("failed to bind mount /sys");

            nix::mount::mount(
                None::<&str>,
                &concat_absolute(rootfs, "/sys"),
                None::<&str>,
                nix::mount::MsFlags::MS_SLAVE | nix::mount::MsFlags::MS_REC,
                None::<&str>,
            )
            .expect("failed to make /sys slave");
        }
    }

    // Perform bind mounts requested by user.
    for bm in &cfg.bind_mounts {
        nix::mount::mount(
            Some(&bm.source),
            &concat_absolute(rootfs.unwrap_or(Path::new("/")), &bm.destination),
            None::<&str>,
            nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
            None::<&str>,
        )
        .expect("failed to perform bind mount");
    }

    // Perform volume mounts.
    for vol in &cfg.volumes {
        let source = workspace
            .expect("--workspace is required for volumes")
            .join("volumes")
            .join(&vol.name);
        std::fs::create_dir_all(&source).expect("failed to create volume directory");
        let dest = concat_absolute(rootfs.unwrap_or(Path::new("/")), &vol.destination);
        std::fs::create_dir_all(&dest).expect("failed to create volume mount point");
        nix::mount::mount(
            Some(&source),
            &dest,
            None::<&str>,
            nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
            None::<&str>,
        )
        .expect("failed to perform volume mount");
    }

    // TODO: We could drop privileges here.
    //       (However, cbuildrt does not really protect against malicious sandbox escapes.)

    // fork() and execve() in the child.
    // The parent waits for the child to terminate.
    // (We cannot use Rust's high-level API since we need to reap orphans.)
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            if let Some(rootfs) = rootfs {
                nix::unistd::chdir(rootfs).expect("failed to chdir() to rootfs");
                if !cfg.no_chroot {
                    nix::unistd::chroot(".").expect("failed to chroot()");
                }
            }

            // setuid/setgid in the child only such that the init process can do cleanup as root.
            // setgid before setuid since setuid drops capabilities.
            nix::unistd::setgid(nix::unistd::Gid::from_raw(cfg.user.gid))
                .expect("failed to set GID");
            nix::unistd::setuid(nix::unistd::Uid::from_raw(cfg.user.uid))
                .expect("failed to set UID");

            // Reset PATH to the default value
            if !cfg.no_chroot {
                if cfg.user.uid == 0 {
                    std::env::set_var(
                        "PATH",
                        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                    );
                } else {
                    std::env::set_var("PATH", "/usr/local/bin:/usr/bin:/bin");
                }
            }

            // Apply user-specified environment variables.
            for (key, value) in &cfg.process.environ {
                std::env::set_var(key, value);
            }

            let exec_result = nix::unistd::execvp(
                &CString::new(cfg.process.args[0].as_str()).unwrap(),
                &cfg.process
                    .args
                    .iter()
                    .map(|a| CString::new(a.as_str()).unwrap())
                    .collect::<Vec<_>>(),
            );
            eprintln!("error when executing program: {}", exec_result.unwrap_err());
            exit(1);
        }
        Ok(nix::unistd::ForkResult::Parent { child: child_pid }) => {
            let child_exit_code;
            loop {
                // Now, let's wait for the child to terminate.
                let child_status = nix::sys::wait::wait().expect("failed to wait for children");
                if let nix::sys::wait::WaitStatus::Exited(pid, code) = child_status {
                    if pid == child_pid {
                        child_exit_code = code;
                        break;
                    }
                }
            }

            // If the command succeeded, extract the upper layer as a tar.
            if child_exit_code == 0 {
                if let Some(RootFs::Overlay {
                    with_upper: true,
                    extract_upper: Some(dest),
                    ..
                }) = &cfg.rootfs
                {
                    let upper = run_dir
                        .expect("extractUpper is set but no overlay tempdir was provided")
                        .join("upper");
                    let tar_file = File::create(dest).expect("failed to create output tar file");
                    let mut builder = tar::Builder::new(tar_file);
                    builder.follow_symlinks(false);
                    builder
                        .append_dir_all(".", upper)
                        .expect("failed to write upper layer to tar");
                    builder.finish().expect("failed to finalize tar archive");
                }
            }

            if child_exit_code != 0 {
                eprintln!("child returned non-zero exit code");
            }
            exit(child_exit_code);
        }
        Err(_) => panic!("failed to fork from init"),
    };
}

fn setup_userns_direct(cfg: &Config, euid: nix::unistd::Uid, egid: nix::unistd::Gid) {
    // Write the uid_map and gid_map files. Linux demands that we write setgroups first
    // (otherwise, we need to be root in the outer namespace).
    std::fs::write("/proc/self/setgroups", "deny").expect("unable to write setgroups file");

    std::fs::write("/proc/self/uid_map", format!("{} {} 1", cfg.user.uid, euid))
        .expect("unable to write uid_map file");
    std::fs::write("/proc/self/gid_map", format!("{} {} 1", cfg.user.gid, egid))
        .expect("unable to write gid_map file");
}

fn setup_userns_with_helper(sock: rustix::fd::BorrowedFd) {
    // Signal the helper that we have unshare()d.
    rustix::net::send(sock, &[1u8], rustix::net::SendFlags::empty())
        .expect("failed to signal helper");

    // Wait for helper to finish setting up mappings.
    let mut done = [0u8; 1];
    let (_, n) = rustix::net::recv(sock, &mut done, rustix::net::RecvFlags::empty())
        .expect("failed to read from helper");
    if n == 0 {
        panic!("helper closed socket without signaling completion");
    }
}

fn parse_subordinate_file(path: &str, username: &str) -> (u64, u64) {
    let content =
        std::fs::read_to_string(path).unwrap_or_else(|e| panic!("failed to read {}: {}", path, e));
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

fn resolve_id_range(range: &SubIdRange, file: &str) -> (u64, u64) {
    if range.auto {
        let euid = nix::unistd::geteuid();
        let user = nix::unistd::User::from_uid(euid)
            .expect("failed to look up user")
            .unwrap_or_else(|| panic!("no passwd entry for uid {}", euid));
        parse_subordinate_file(file, &user.name)
    } else {
        (range.start, range.count)
    }
}

// Build setuidmap/setgidmap args to map [0, sub_count) inside userns
// to [sub_start, sub_start + sub_count) outside userns,
// except for self_id which is mapped to host_self_id.
fn build_id_mapping_args(
    self_id: u64,
    host_self_id: u64,
    sub_start: u64,
    sub_count: u64,
) -> Vec<(u64, u64, u64)> {
    let mut mappings = vec![(self_id, host_self_id, 1)];
    // Map part of [0, sub_count) that is below self_id.
    if self_id > 0 {
        if self_id < sub_count {
            mappings.push((0, sub_start, self_id));
        } else {
            mappings.push((0, sub_start, sub_count));
        }
    }
    // Map part of [0, sub_count) that is at or above (self_id + 1).
    if self_id + 1 < sub_count {
        mappings.push((
            self_id + 1,
            sub_start + self_id + 1,
            sub_count - (self_id + 1),
        ));
    }

    mappings
}

fn run_userns_helper(
    euid: nix::unistd::Uid,
    egid: nix::unistd::Gid,
    subuid: (u64, u64),
    subgid: (u64, u64),
    self_uid: u64,
    self_gid: u64,
    main_pid: nix::unistd::Pid,
    sock: rustix::fd::OwnedFd,
) -> ! {
    // Wait for the main process to signal that it has unshared.
    let mut ready = [0u8; 1];
    let (_, n) = rustix::net::recv(&sock, &mut ready, rustix::net::RecvFlags::empty())
        .expect("failed to read from main process");
    if n == 0 {
        exit(1);
    }

    let uid_mappings = build_id_mapping_args(self_uid, euid.as_raw().into(), subuid.0, subuid.1);
    let mut cmd = std::process::Command::new("newuidmap");
    cmd.arg(main_pid.as_raw().to_string());
    for (inner, outer, count) in &uid_mappings {
        cmd.args([inner.to_string(), outer.to_string(), count.to_string()]);
    }
    let status = cmd.status();
    match status {
        Ok(s) if s.success() => {}
        Ok(s) => {
            if !s.success() {
                eprintln!("newuidmap failed with exit code {:?}", s.code());
                exit(1);
            }
        }
        Err(e) => {
            eprintln!("failed to run newuidmap: {}", e);
            exit(1);
        }
    }

    let gid_mappings = build_id_mapping_args(self_gid, egid.as_raw().into(), subgid.0, subgid.1);
    let mut cmd = std::process::Command::new("newgidmap");
    cmd.arg(main_pid.as_raw().to_string());
    for (inner, outer, count) in &gid_mappings {
        cmd.args([inner.to_string(), outer.to_string(), count.to_string()]);
    }
    let status = cmd.status();
    match status {
        Ok(s) => {
            if !s.success() {
                eprintln!("newgidmap failed with exit code {:?}", s.code());
                exit(1);
            }
        }
        Err(e) => {
            eprintln!("failed to run newgidmap: {}", e);
            exit(1);
        }
    }

    // Signal the main process that mappings are done.
    rustix::net::send(&sock, &[1u8], rustix::net::SendFlags::empty())
        .expect("failed to signal main process");

    exit(0);
}

pub fn run(cfg: Config, workspace: Option<PathBuf>) {
    if let Some(RootFs::Path(path)) = &cfg.rootfs {
        let lockfile_path = path
            .parent()
            .and_then(|p| Some(p.join(path.file_name()?)))
            .map(|p| p.with_extension("cbrt_lock"))
            .expect("couldn't construct lockfile path");

        let root_dir = open(
            &lockfile_path,
            OFlag::O_RDONLY | OFlag::O_CREAT | OFlag::O_CLOEXEC,
            Mode::from_bits(0o444).unwrap(),
        )
        .expect("couldn't open rootfs for locking");

        flock(root_dir, FlockArg::LockShared).expect("failed to lock rootdir");
    }

    let euid = nix::unistd::geteuid();
    let egid = nix::unistd::getegid();

    // Launch a helper process to run set{uid,gid}map.
    let (helper_pid, sock_main) =
        if let (Some(sub_uid), Some(sub_gid)) = (&cfg.sub_uid, &cfg.sub_gid) {
            let resolved_subuid = resolve_id_range(sub_uid, "/etc/subuid");
            let resolved_subgid = resolve_id_range(sub_gid, "/etc/subgid");

            // Set up socketpair for helper synchronization if using subordinate IDs.
            let (sock_main, sock_helper) = rustix::net::socketpair(
                rustix::net::AddressFamily::UNIX,
                rustix::net::SocketType::SEQPACKET,
                rustix::net::SocketFlags::CLOEXEC,
                None,
            )
            .expect("failed to create socketpair for helper sync");

            let main_pid = nix::unistd::getpid();
            match unsafe { nix::unistd::fork() } {
                Ok(nix::unistd::ForkResult::Child) => {
                    drop(sock_main);
                    run_userns_helper(
                        euid,
                        egid,
                        resolved_subuid,
                        resolved_subgid,
                        sub_uid.self_id,
                        sub_gid.self_id,
                        main_pid,
                        sock_helper,
                    );
                }
                Ok(nix::unistd::ForkResult::Parent { child }) => {
                    drop(sock_helper);
                    (Some(child), Some(sock_main))
                }
                Err(_) => panic!("failed to fork helper for ID mapping"),
            }
        } else {
            (None, None)
        };

    // Enter the user namespace and let children enter a new PID namespace.
    // We cannot do mounts in this process yet, as this the process itself
    // is not moved to the new PID namespace.
    nix::sched::unshare(
        nix::sched::CloneFlags::CLONE_NEWUSER | nix::sched::CloneFlags::CLONE_NEWPID,
    )
    .expect("failed to unshare()");

    if cfg.sub_uid.is_some() && cfg.sub_gid.is_some() {
        setup_userns_with_helper(sock_main.unwrap().as_fd());
        nix::sys::wait::waitpid(
            helper_pid.expect("set{uid,gid}map helper PID must be known"),
            None,
        )
        .expect("failed to wait for helper");
    } else {
        setup_userns_direct(&cfg, euid, egid);
    }

    // Some configurations need a per-run directory to store temporary data.
    // Note that cleanup of the per-run directory requires us to be in the user namespace
    // (but this process is in the user namespace anyway).
    let need_tempdir = matches!(cfg.rootfs, Some(RootFs::Overlay { .. }));
    let run_tempdir: Option<tempfile::TempDir> = if need_tempdir {
        let run_root = workspace
            .as_deref()
            .expect("--workspace is required for overlay rootfs")
            .join("run");
        std::fs::create_dir_all(&run_root)
            .expect("failed to create cbuildrt overlay cache directory");
        let dir = tempfile::Builder::new()
            .tempdir_in(&run_root)
            .expect("failed to create per-run overlay tempdir");
        Some(dir)
    } else {
        None
    };
    let run_dir = run_tempdir.as_ref().map(|d| d.path());

    // fork() and run init in the child.
    // The parent waits for the child to terminate.
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            run_init(&cfg, workspace.as_deref(), run_dir);
        }
        Ok(nix::unistd::ForkResult::Parent { child: init_pid }) => {
            eprintln!("PID init is {} (outside the namespace)", init_pid);

            // Wait for init to terminate.
            let init_status =
                nix::sys::wait::waitpid(init_pid, None).expect("failed to wait for init");
            let init_code = match init_status {
                nix::sys::wait::WaitStatus::Exited(_, code) => code,
                _ => panic!("waiting for init returned {:?}", init_status),
            };

            exit(init_code);
        }
        Err(_) => panic!("failed to fork from cbuildrt"),
    };
}
