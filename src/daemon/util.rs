//! Small blocking-I/O helpers: an eventfd waker and poll(2) with deadlines.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::time::{Duration, Instant};

/// eventfd used to wake a thread blocked in poll(2) (e.g. after sending it a command).
pub struct Waker {
    fd: OwnedFd,
}

impl Waker {
    pub fn new() -> io::Result<Self> {
        // SAFETY: eventfd has no pointer arguments.
        let fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor we own.
        Ok(Self {
            fd: unsafe { OwnedFd::from_raw_fd(fd) },
        })
    }
    pub fn wake(&self) {
        let one: u64 = 1;
        // SAFETY: writes 8 bytes from a valid u64 to a live eventfd.
        unsafe {
            libc::write(
                self.fd.as_raw_fd(),
                &one as *const u64 as *const libc::c_void,
                8,
            )
        };
    }
    pub fn drain(&self) {
        let mut value: u64 = 0;
        // SAFETY: reads 8 bytes into a valid u64 from a live non-blocking eventfd.
        unsafe {
            libc::read(
                self.fd.as_raw_fd(),
                &mut value as *mut u64 as *mut libc::c_void,
                8,
            )
        };
    }
    pub fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }
}

pub fn pollfd(fd: RawFd, events: libc::c_short) -> libc::pollfd {
    libc::pollfd {
        fd,
        events,
        revents: 0,
    }
}

pub const POLL_GONE: libc::c_short = libc::POLLERR | libc::POLLHUP | libc::POLLNVAL;

/// Time left until `deadline` (None = wait forever).
pub fn timeout_until(deadline: Option<Instant>, now: Instant) -> Option<Duration> {
    deadline.map(|deadline| deadline.saturating_duration_since(now))
}

/// poll(2); EINTR counts as "nothing ready". Entries with fd < 0 are ignored by the kernel.
pub fn poll(fds: &mut [libc::pollfd], timeout: Option<Duration>) -> io::Result<usize> {
    for fd in fds.iter_mut() {
        fd.revents = 0;
    }
    let millis = match timeout {
        None => -1,
        // Round up so a deadline a few µs away does not spin.
        Some(timeout) => timeout.as_micros().div_ceil(1000).min(i32::MAX as u128) as libc::c_int,
    };
    // SAFETY: `fds` is a valid, exclusively borrowed slice of pollfd.
    let ready = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) };
    if ready < 0 {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::Interrupted {
            return Ok(0);
        }
        return Err(error);
    }
    Ok(ready as usize)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn waker_wakes_poll_and_drains() {
        let waker = Waker::new().unwrap();
        let mut fds = [pollfd(waker.fd(), libc::POLLIN)];
        assert_eq!(poll(&mut fds, Some(Duration::ZERO)).unwrap(), 0);
        waker.wake();
        waker.wake();
        assert_eq!(poll(&mut fds, Some(Duration::from_secs(1))).unwrap(), 1);
        waker.drain();
        assert_eq!(poll(&mut fds, Some(Duration::ZERO)).unwrap(), 0);
        let now = Instant::now();
        assert_eq!(timeout_until(None, now), None);
        assert_eq!(
            timeout_until(Some(now), now + Duration::from_secs(1)),
            Some(Duration::ZERO)
        );
    }
}
