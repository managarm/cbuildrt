use libc::{gid_t, uid_t};
use serde::{Deserialize, Serialize};
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
struct User {
    uid: uid_t,
    gid: gid_t,
}

#[derive(Serialize, Deserialize)]
struct Process {
    args: Vec<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Config {
    rootfs: PathBuf,
    user: User,
    process: Process,
    bind_mounts: Vec<BindMount>,
}

// TODO: This function does not really perform error checking;
//       for now, we assume that xbstrap passes sane values.
fn make_config_from_cli() -> Config {
    let matches = clap::App::new("cbuildrt")
        .arg(
            clap::Arg::with_name("cbuild-json")
                .help("cbuild.json file")
                .required(true),
        )
        .get_matches();

    let cfg_f =
        File::open(matches.value_of("cbuild-json").unwrap()).expect("unable to open cbuild.json");

    serde_json::from_reader(cfg_f).expect("failed to parse cbuild.json")
}

// Concatenates lhs and rhs as-if the rhs was a relative path.
fn concat_absolute<L: AsRef<Path>, R: AsRef<Path>>(lhs: L, rhs: R) -> PathBuf {
    lhs.as_ref().join(rhs.as_ref().strip_prefix("/").unwrap())
}

fn run_init(cfg: &Config) -> ! {
    // We can now set up the remaining namespaces and perform mounts.
    nix::sched::unshare(nix::sched::CloneFlags::CLONE_NEWNS).expect("failed to unshare()");

    // First, we need to get a read-only rootfs.
    // Mounting with MS_BIND ignored MS_RDONLY, but MS_REMOUNT respects it.

    nix::mount::mount(
        Some(&cfg.rootfs),
        &cfg.rootfs,
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .expect("failed to bind mount rootfs to itself");

    nix::mount::mount(
        Some(&cfg.rootfs),
        &cfg.rootfs,
        None::<&str>,
        nix::mount::MsFlags::MS_REMOUNT
            | nix::mount::MsFlags::MS_BIND
            | nix::mount::MsFlags::MS_RDONLY,
        None::<&str>,
    )
    .expect("failed to make rootfs read-only");

    // Perform mounts of /dev, /dev/pts, /dev/shm, /run and /tmp.

    let dev_overlays = vec!["tty", "null", "zero", "full", "random", "urandom"];
    for f in dev_overlays {
        nix::mount::mount(
            Some(&Path::new("/dev/").join(f)),
            &concat_absolute(&cfg.rootfs, "/dev/").join(f),
            None::<&str>,
            nix::mount::MsFlags::MS_BIND,
            None::<&str>,
        )
        .expect("failed to mount device");
    }

    nix::mount::mount(
        Some(&std::fs::canonicalize("/etc/resolv.conf").unwrap()),
        &concat_absolute(&cfg.rootfs, "/etc/resolv.conf"),
        None::<&str>,
        nix::mount::MsFlags::MS_BIND,
        None::<&str>,
    )
    .expect("failed to mount /etc/resolv.conf");

    nix::mount::mount(
        None::<&str>,
        &concat_absolute(&cfg.rootfs, "/dev/pts"),
        Some("devpts"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .expect("failed to mount /dev/pts");

    nix::mount::mount(
        None::<&str>,
        &concat_absolute(&cfg.rootfs, "/dev/shm"),
        Some("tmpfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .expect("failed to mount /dev/shm");

    nix::mount::mount(
        None::<&str>,
        &concat_absolute(&cfg.rootfs, "/run"),
        Some("tmpfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .expect("failed to mount /run");

    nix::mount::mount(
        None::<&str>,
        &concat_absolute(&cfg.rootfs, "/tmp"),
        Some("tmpfs"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    )
    .expect("failed to mount /tmp");

    // Perform bind mounts requested by user.
    for bm in &cfg.bind_mounts {
        nix::mount::mount(
            Some(&bm.source),
            &concat_absolute(&cfg.rootfs, &bm.destination),
            None::<&str>,
            nix::mount::MsFlags::MS_BIND | nix::mount::MsFlags::MS_REC,
            None::<&str>,
        )
        .expect("failed to perform bind mount");
    }

    // chroot() and change the current directory to /.
    nix::unistd::chroot(&cfg.rootfs).expect("failed to chroot()");
    nix::unistd::chdir("/").expect("failed to chdir() to root directory");

    // TODO: We could drop privileges here.
    //       (However, cbuildrt does not really protect against malicious sandbox escapes.)

    // fork() and execve() in the child.
    // The parent waits for the child to terminate.
    // (We cannot use Rust's high-level API since we need to reap orphans.)
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => {
            // Reset PATH to the default value
            if cfg.user.uid == 0 {
                std::env::set_var(
                    "PATH",
                    "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
                );
            } else {
                std::env::set_var("PATH", "/usr/local/bin:/usr/bin:/bin");
            }

            let exec_result = nix::unistd::execvp(
                &CString::new(cfg.process.args[0].as_str()).unwrap(),
                &cfg.process
                    .args
                    .iter()
                    .map(|a| CString::new(a.as_str()).unwrap())
                    .collect::<Vec<_>>(),
            );
            println!("error when executing program: {}", exec_result.unwrap_err());
            exit(1);
        }
        Ok(nix::unistd::ForkResult::Parent { child: child_pid }) => {
            loop {
                // Now, let's wait for the child to terminate.
                let child_status = nix::sys::wait::wait().expect("failed to wait for children");
                if let nix::sys::wait::WaitStatus::Exited(pid, code) = child_status {
                    if pid == child_pid {
                        if code != 0 {
                            println!("child returned non-zero exit code");
                        }
                        exit(code);
                    }
                }
            }
        }
        Err(_) => panic!("failed to fork from init"),
    };
}

fn main() {
    let cfg = make_config_from_cli();

    let euid = nix::unistd::geteuid();
    let egid = nix::unistd::getegid();

    // Enter the user namespace and let children enter a new PID namespace.
    // We cannot do mounts in this process yet, as this the process itself
    // is not moved to the new PID namespace.
    nix::sched::unshare(
        nix::sched::CloneFlags::CLONE_NEWUSER | nix::sched::CloneFlags::CLONE_NEWPID,
    )
    .expect("failed to unshare()");

    // Write the uid_map and gid_map files. Linux demands that we write setgroups first
    // (otherwise, we need to be root in the outer namespace).

    std::fs::write("/proc/self/setgroups", "deny").expect("unable to write setgroups file");

    std::fs::write("/proc/self/uid_map", format!("{} {} 1", cfg.user.uid, euid))
        .expect("unable to write uid_map file");
    std::fs::write("/proc/self/gid_map", format!("{} {} 1", cfg.user.gid, egid))
        .expect("unable to write gid_map file");

    // Change user IDs.
    nix::unistd::setuid(nix::unistd::Uid::from_raw(cfg.user.uid)).expect("failed to set UID");
    nix::unistd::setgid(nix::unistd::Gid::from_raw(cfg.user.gid)).expect("failed to set GID");

    // fork() and run init in the child.
    // The parent waits for the child to terminate.
    match unsafe { nix::unistd::fork() } {
        Ok(nix::unistd::ForkResult::Child) => run_init(&cfg),
        Ok(nix::unistd::ForkResult::Parent { child: init_pid }) => {
            println!("PID init is {} (outside the namespace)", init_pid);

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
