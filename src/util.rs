use nix::sys::signal::{sigprocmask, SigSet, SigmaskHow, Signal};

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
