use libc::{gid_t, uid_t};
use nix::fcntl::{open, Flock, FlockArg, OFlag};
use nix::sys::signal::SigSet;
use nix::sys::signalfd::{SfdFlags, SignalFd};
use nix::sys::stat::Mode;
use rustix::event::{poll, PollFd, PollFlags};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::ffi::CString;
use std::fs::File;
use std::os::fd::AsFd;
use std::path::{Path, PathBuf};
use std::process::exit;

use crate::util::{
    classify_archive, open_tar_reader, open_tar_writer, termination_signal_set, ArchiveKind,
    SignalMaskGuard,
};
use crate::workspace::Workspace;

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
#[serde(deny_unknown_fields, rename_all = "camelCase")]
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
        #[serde(default, rename = "importUpper")]
        import_upper: Option<PathBuf>,
    },
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
    // Provide a /dev fs instead of relying on the device placerholders on the rootfs.
    #[serde(default)]
    provide_dev: bool,
    bind_mounts: Vec<BindMount>,
    #[serde(default)]
    volumes: Vec<Volume>,
    #[serde(default)]
    map_current_user_to: Option<User>,
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

fn resolve_tar_layer(workspace: &Workspace, path: &Path) -> PathBuf {
    let (kind, stem) = classify_archive(path);
    if matches!(kind, ArchiveKind::Plain) {
        return path.to_path_buf();
    }

    let layers_root = workspace.layers_dir();

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
    std::os::unix::fs::chown(&staging, Some(0), Some(0))
        .expect("failed to chown() overlay lower dir");

    let reader = open_tar_reader(path, &kind);
    let mut archive = tar::Archive::new(reader);
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

fn setup_dev(rootfs: &Path, provide_dev: bool, run_dir: Option<&Path>) {
    let subdirs = ["pts", "shm"];

    let devices = ["tty", "null", "zero", "full", "random", "urandom"];

    if provide_dev {
        // We put /dev onto the host filesystem since mounting a tmpfs inside the user namespace
        // behaves differently compared to a bind mounted host directory.
        // In particular, opening bind mounted devices on such a tmpfs with O_CREAT fails with EACCES.
        let skeleton = run_dir.expect("provide_dev requires run_dir").join("dev");
        std::fs::create_dir(&skeleton).expect("failed to create dev dir");

        // Create contents of /dev.
        let symlinks = [
            ("fd", "/proc/self/fd"),
            ("ptmx", "pts/ptmx"),
            ("stdin", "/proc/self/fd/0"),
            ("stdout", "/proc/self/fd/1"),
            ("stderr", "/proc/self/fd/2"),
        ];

        for d in &subdirs {
            std::fs::create_dir(skeleton.join(d)).expect("failed to create /dev subdirectory");
        }
        for f in &devices {
            File::create(skeleton.join(f)).expect("failed to create /dev device placeholder");
        }
        for (link, target) in &symlinks {
            std::os::unix::fs::symlink(target, skeleton.join(link))
                .expect("failed to create /dev symlink");
        }

        // Mount /dev.
        nix::mount::mount(
            Some(&skeleton),
            &concat_absolute(rootfs, "/dev"),
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .expect("failed to bind mount dev dir");
    }

    // Mount subdirectories.
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

    // Mount devices.
    for f in &devices {
        nix::mount::mount(
            Some(&Path::new("/dev/").join(f)),
            &concat_absolute(rootfs, "/dev/").join(f),
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .expect("failed to bind mount device");
    }
}

fn run_init(cfg: &Config, workspace: &Workspace, run_dir: Option<&Path>) -> ! {
    // Derive the rootfs path.
    let rootfs_owned: Option<PathBuf> = match &cfg.rootfs {
        Some(RootFs::Path(path)) => Some(path.clone()),
        Some(RootFs::Overlay { .. }) => {
            let merged = run_dir.unwrap().join("merged");
            std::fs::create_dir(&merged).expect("failed to create overlay merged dir");
            std::os::unix::fs::chown(&merged, Some(0), Some(0))
                .expect("failed to chown() merged overlay dir");
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
            layers,
            with_upper,
            import_upper,
            ..
        }) = &cfg.rootfs
        {
            if import_upper.is_some() && !with_upper {
                panic!("importUpper requires withUpper to be set");
            }

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
                std::os::unix::fs::chown(&upper, Some(0), Some(0))
                    .expect("failed to chown() overlay upper dir");
                std::os::unix::fs::chown(&work, Some(0), Some(0))
                    .expect("failed to chown() overlay work dir");

                if let Some(import_path) = import_upper {
                    let (kind, _) = classify_archive(import_path);
                    let reader = open_tar_reader(import_path, &kind);
                    let mut archive = tar::Archive::new(reader);
                    archive.set_preserve_permissions(true);
                    archive.set_preserve_ownerships(true);
                    archive
                        .unpack(&upper)
                        .expect("failed to extract importUpper tar into upper dir");
                }

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
            setup_dev(rootfs, cfg.provide_dev, run_dir);

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
        let source = workspace.volumes_dir().join(&vol.name);
        std::fs::create_dir_all(&source).expect("failed to create volume directory");
        std::os::unix::fs::chown(&source, Some(0), Some(0)).expect("failed to chown() volume dir");
        let dest = concat_absolute(rootfs.unwrap_or(Path::new("/")), &vol.destination);
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
                    let (kind, _) = classify_archive(dest);
                    let writer = open_tar_writer(dest, &kind);
                    let mut builder = tar::Builder::new(writer);
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

fn setup_userns_direct(
    self_uid: u64,
    self_gid: u64,
    euid: nix::unistd::Uid,
    egid: nix::unistd::Gid,
) {
    // Write the uid_map and gid_map files. Linux demands that we write setgroups first
    // (otherwise, we need to be root in the outer namespace).
    std::fs::write("/proc/self/setgroups", "deny").expect("unable to write setgroups file");

    std::fs::write("/proc/self/uid_map", format!("{} {} 1", self_uid, euid))
        .expect("unable to write uid_map file");
    std::fs::write("/proc/self/gid_map", format!("{} {} 1", self_gid, egid))
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
    self_uid: Option<u64>,
    self_gid: Option<u64>,
    target_pid: libc::pid_t,
    sock: rustix::fd::BorrowedFd,
) {
    // Wait for the target process to signal that it has unshared.
    let mut ready = [0u8; 1];
    let (_, n) = rustix::net::recv(sock, &mut ready, rustix::net::RecvFlags::empty())
        .expect("failed to read from target process");
    if n == 0 {
        eprintln!("target process closed socket before signaling");
        exit(1);
    }

    let uid_mappings = match self_uid {
        Some(uid) => build_id_mapping_args(uid, euid.as_raw().into(), subuid.0, subuid.1),
        None => vec![(0, subuid.0, subuid.1)],
    };
    let mut cmd = std::process::Command::new("newuidmap");
    cmd.arg(target_pid.to_string());
    for (inner, outer, count) in &uid_mappings {
        cmd.args([inner.to_string(), outer.to_string(), count.to_string()]);
    }
    let status = cmd.status();
    match status {
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

    let gid_mappings = match self_gid {
        Some(gid) => build_id_mapping_args(gid, egid.as_raw().into(), subgid.0, subgid.1),
        None => vec![(0, subgid.0, subgid.1)],
    };
    let mut cmd = std::process::Command::new("newgidmap");
    cmd.arg(target_pid.to_string());
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

    // Signal the target process that mappings are done.
    rustix::net::send(sock, &[1u8], rustix::net::SendFlags::empty())
        .expect("failed to signal target process");
}

unsafe fn raw_clone(flags: libc::c_int) -> libc::pid_t {
    let ret = unsafe {
        libc::syscall(
            libc::SYS_clone,
            (flags | libc::SIGCHLD) as libc::c_ulong,
            std::ptr::null::<libc::c_void>(),
        )
    };
    if ret < 0 {
        let error = std::io::Error::last_os_error();
        panic!("clone() failed: {}", error);
    }
    ret as libc::pid_t
}

// Wait for a child process. Terminate the child (with SIGTERM) if a termination signal becomes
// pending for the current process. Note that this requires the caller to block the signal.
// This function does *not* dequeue the termination signal from the current process.
fn wait_and_forward_termination(child_pid: libc::pid_t, mask: &SigSet) -> i32 {
    let child_pidfd = rustix::process::pidfd_open(
        rustix::process::Pid::from_raw(child_pid).unwrap(),
        rustix::process::PidfdFlags::empty(),
    )
    .expect("failed to get pidfd for child process");
    let sigfd = SignalFd::with_flags(mask, SfdFlags::SFD_CLOEXEC).expect("signalfd() failed");
    let pidfd = child_pidfd.as_fd();

    loop {
        let mut fds = [
            PollFd::new(&sigfd, PollFlags::IN),
            PollFd::new(&pidfd, PollFlags::IN),
        ];
        match poll(&mut fds, None) {
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => panic!("poll() failed: {:?}", e),
        }
        let sigfd_ready = fds[0].revents().contains(PollFlags::IN);
        let pidfd_ready = fds[1].revents().contains(PollFlags::IN);

        if sigfd_ready {
            match rustix::process::pidfd_send_signal(pidfd, rustix::process::Signal::TERM) {
                Ok(_) => {}
                Err(e) => {
                    // Ignore ESRCH as the child has already terminated in that case.
                    if e != rustix::io::Errno::SRCH {
                        panic!("failed to signal child process: {:?}", e);
                    }
                }
            }
        }
        if pidfd_ready {
            break;
        }
    }

    let status = rustix::process::waitid(
        rustix::process::WaitId::PidFd(pidfd),
        rustix::process::WaitIdOptions::EXITED,
    )
    .expect("waitid failed")
    .expect("waitid returned no status despite ready pidfd");
    if let Some(code) = status.exit_status() {
        code
    } else if let Some(sig) = status.terminating_signal() {
        128 + sig
    } else {
        panic!("unexpected wait status: {:?}", status);
    }
}

/// Run a callback inside a workspace's user namespace.
///
/// # Safety
/// The caller must ensure the current process is single-threaded.
// TODO: Use ! as a return type instead of Infallible once it is stabilized.
pub unsafe fn run_userns<F: FnOnce() -> Infallible>(
    workspace: &Workspace,
    self_uid: Option<u64>,
    self_gid: Option<u64>,
    f: F,
) -> i32 {
    let euid = nix::unistd::geteuid();
    let egid = nix::unistd::getegid();

    // Create socketpair for synchronization (only needed for sub_ids case).
    let (sock_parent, sock_child) = if workspace.sub_ids().is_some() {
        let (sp, sc) = rustix::net::socketpair(
            rustix::net::AddressFamily::UNIX,
            rustix::net::SocketType::SEQPACKET,
            rustix::net::SocketFlags::CLOEXEC,
            None,
        )
        .expect("failed to create socketpair");
        (Some(sp), Some(sc))
    } else {
        (None, None)
    };

    let child_pid = raw_clone(libc::CLONE_NEWUSER);
    if child_pid == 0 {
        drop(sock_parent);

        if workspace.sub_ids().is_some() {
            setup_userns_with_helper(sock_child.as_ref().unwrap().as_fd());
        } else {
            // If no self_uid / self_gid is passed, we default to zero here
            // as the direct mapping method has to pick some uid / gid.
            setup_userns_direct(self_uid.unwrap_or(0), self_gid.unwrap_or(0), euid, egid);
        }

        // Explicit drop since we exit() below.
        drop(sock_child);

        f();
        exit(0);
    } else {
        drop(sock_child);

        if let Some(sub_ids) = workspace.sub_ids().copied() {
            run_userns_helper(
                euid,
                egid,
                sub_ids.uid,
                sub_ids.gid,
                self_uid,
                self_gid,
                child_pid,
                sock_parent.as_ref().unwrap().as_fd(),
            );
        }

        // Wait for child.
        let status = nix::sys::wait::waitpid(nix::unistd::Pid::from_raw(child_pid), None)
            .expect("failed to wait for child");
        match status {
            nix::sys::wait::WaitStatus::Exited(_, code) => code,
            _ => panic!("unexpected wait status: {:?}", status),
        }
    }
}

pub unsafe fn run(cfg: Config, workspace: Workspace) -> i32 {
    let _rootfs_lock = if let Some(RootFs::Path(path)) = &cfg.rootfs {
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

        Some(
            Flock::lock(root_dir, FlockArg::LockShared)
                .map_err(|(_, e)| e)
                .expect("failed to lock rootdir"),
        )
    } else {
        None
    };

    let map_to = cfg.map_current_user_to.as_ref().unwrap_or(&cfg.user);
    unsafe {
        run_userns(
            &workspace,
            Some(map_to.uid as u64),
            Some(map_to.gid as u64),
            || {
                let exit_code = run_supervisor(cfg, &workspace);
                exit(exit_code);
            },
        )
    }
}

// Runs a supervisor process that spawns init (as yet another process)
// and that cleans up afterwards.
unsafe fn run_supervisor(cfg: Config, workspace: &Workspace) -> i32 {
    // We want to make sure that cleanup runs even if we receive SIGTERM etc.
    // Block termination signals and kill the container PID 1 if we receive any signals.
    let blocked = termination_signal_set();
    let _mask_guard = SignalMaskGuard::block(&blocked);

    // Some configurations need a per-run directory to store temporary data.
    // Note that cleanup of the per-run directory requires us to be in the user namespace.
    let need_tempdir = matches!(cfg.rootfs, Some(RootFs::Overlay { .. }));
    let run_tempdir: Option<tempfile::TempDir> = if need_tempdir {
        let run_root = workspace.run_dir();
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

    let child_pid = raw_clone(libc::CLONE_NEWPID);
    if child_pid == 0 {
        // Restore the default (= empty) signal mask in the child.
        SigSet::empty().thread_set_mask().unwrap();

        run_init(&cfg, workspace, run_dir);
    }

    wait_and_forward_termination(child_pid, &blocked)
}
