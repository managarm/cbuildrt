use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow, Signal};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

// Set of signals that the user may use to terminate the process.
pub fn termination_signal_set() -> SigSet {
    let mut set = SigSet::empty();
    set.add(Signal::SIGTERM);
    set.add(Signal::SIGINT);
    set.add(Signal::SIGHUP);
    set.add(Signal::SIGQUIT);
    set
}

// RAII guard to restore a signal mask on drop.
pub struct SignalMaskGuard {
    old_mask: SigSet,
}

impl SignalMaskGuard {
    pub fn block(set: &SigSet) -> Self {
        let mut old_mask = SigSet::empty();
        sigprocmask(SigmaskHow::SIG_BLOCK, Some(set), Some(&mut old_mask))
            .expect("failed to block signals");
        Self { old_mask }
    }
}

impl Drop for SignalMaskGuard {
    fn drop(&mut self) {
        sigprocmask(SigmaskHow::SIG_SETMASK, Some(&self.old_mask), None)
            .expect("failed to restore former signal mask");
    }
}

pub enum ArchiveKind {
    Plain,
    Tar,
    TarZstd,
}

// Classifies an archive path and returns its stem.
pub fn classify_archive(path: &Path) -> (ArchiveKind, &OsStr) {
    let name = path.file_name().expect("archive path has no file name");
    let bytes = name.as_bytes();
    if let Some(stem) = bytes.strip_suffix(b".tar.zstd") {
        (ArchiveKind::TarZstd, OsStr::from_bytes(stem))
    } else if let Some(stem) = bytes.strip_suffix(b".tar") {
        (ArchiveKind::Tar, OsStr::from_bytes(stem))
    } else {
        (ArchiveKind::Plain, name)
    }
}

pub fn open_tar_reader(path: &Path, kind: &ArchiveKind) -> Box<dyn Read> {
    let file = File::open(path).expect("failed to open tar archive");
    match kind {
        ArchiveKind::TarZstd => Box::new(
            zstd::stream::read::Decoder::new(file).expect("failed to initialize zstd decoder"),
        ),
        _ => Box::new(file),
    }
}

pub fn open_tar_writer(path: &Path, kind: &ArchiveKind) -> Box<dyn Write> {
    let file = File::create(path).expect("failed to create tar archive");
    match kind {
        ArchiveKind::TarZstd => Box::new(
            zstd::stream::write::Encoder::new(file, 0)
                .expect("failed to initialize zstd encoder")
                .auto_finish(),
        ),
        _ => Box::new(file),
    }
}
