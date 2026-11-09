//! SIGTERM/SIGINT/SIGHUP delivered through a self-pipe so the main loop can poll(2) for them.

use anyhow::{Result, bail};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicI32, Ordering};

static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_signal(signal: libc::c_int) {
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }
    // SAFETY: only async-signal-safe calls; errno is saved and restored.
    unsafe {
        let errno = *libc::__errno_location();
        let byte = signal as u8;
        libc::write(fd, &byte as *const u8 as *const libc::c_void, 1);
        *libc::__errno_location() = errno;
    }
}

/// Read end of the self-pipe; each byte is one received signal number.
pub struct Signals {
    read: OwnedFd,
}

impl Signals {
    /// Install handlers for SIGTERM, SIGINT and SIGHUP.
    pub fn install() -> Result<Self> {
        Self::install_for(&[libc::SIGTERM, libc::SIGINT, libc::SIGHUP])
    }

    pub(crate) fn install_for(signals: &[libc::c_int]) -> Result<Self> {
        let mut fds = [0; 2];
        // SAFETY: valid out-array for pipe2.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_NONBLOCK | libc::O_CLOEXEC) } < 0 {
            bail!(
                "Creating the signal pipe: {}",
                std::io::Error::last_os_error()
            );
        }
        // SAFETY: fresh descriptor we own. The write end stays open for the process lifetime.
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let previous = WRITE_FD.swap(fds[1], Ordering::SeqCst);
        if previous >= 0 {
            // SAFETY: closing the write end of an older pipe this module created.
            unsafe { libc::close(previous) };
        }
        for signal in signals {
            // SAFETY: zeroed sigaction with a valid handler and an empty mask.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = on_signal as *const () as libc::sighandler_t;
                action.sa_flags = libc::SA_RESTART;
                libc::sigemptyset(&mut action.sa_mask);
                if libc::sigaction(*signal, &action, std::ptr::null_mut()) < 0 {
                    bail!(
                        "Installing a handler for signal {signal}: {}",
                        std::io::Error::last_os_error()
                    );
                }
            }
        }
        Ok(Self { read })
    }

    pub fn fd(&self) -> RawFd {
        self.read.as_raw_fd()
    }

    /// Signals received since the last call.
    pub fn take(&self) -> Vec<libc::c_int> {
        let mut out = Vec::new();
        let mut buffer = [0u8; 32];
        loop {
            // SAFETY: reading into a valid stack buffer from a live non-blocking pipe.
            let read = unsafe {
                libc::read(
                    self.read.as_raw_fd(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if read <= 0 {
                return out;
            }
            out.extend(
                buffer[..read as usize]
                    .iter()
                    .map(|byte| libc::c_int::from(*byte)),
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raised_signal_arrives_on_the_pipe() {
        let signals = Signals::install_for(&[libc::SIGUSR2]).unwrap();
        // SAFETY: raising a signal we installed a handler for.
        unsafe { libc::raise(libc::SIGUSR2) };
        let mut fds = [super::super::util::pollfd(signals.fd(), libc::POLLIN)];
        super::super::util::poll(&mut fds, Some(std::time::Duration::from_secs(1))).unwrap();
        assert_eq!(signals.take(), vec![libc::SIGUSR2]);
        assert!(signals.take().is_empty());
    }
}
