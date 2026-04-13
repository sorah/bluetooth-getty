// PTY proxy for rfcomm tty sessions.
//
// Sits between the rfcomm tty (provided as stdin by systemd) and agetty/login.
// Creates a PTY pair, forks agetty on the slave, and shuttles data between
// the rfcomm fd and the PTY master. This isolates the rfcomm connection from
// login's vhangup() call — vhangup only affects the PTY slave, not the
// underlying Bluetooth RFCOMM DLC.
//
// This is the same pattern systemd uses for container consoles
// (src/shared/ptyfwd.c), where EIO on PTY master during vhangup is treated
// as a transient condition.

use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};

const BUF_SIZE: usize = 4096;

nix::ioctl_read_bad!(tiocgwinsz, libc::TIOCGWINSZ, nix::pty::Winsize);
nix::ioctl_write_int_bad!(tiocsctty, libc::TIOCSCTTY);

pub fn run(child_cmd: &[String]) -> anyhow::Result<std::process::ExitCode> {
    if child_cmd.is_empty() {
        anyhow::bail!("session-proxy: no command specified");
    }

    // Block SIGCHLD so we can receive it via signalfd. Also ignore SIGHUP
    // so we don't die on rfcomm tty hangup events.
    let mut chld_mask = nix::sys::signal::SigSet::empty();
    chld_mask.add(nix::sys::signal::Signal::SIGCHLD);
    nix::sys::signal::sigprocmask(
        nix::sys::signal::SigmaskHow::SIG_BLOCK,
        Some(&chld_mask),
        None,
    )?;
    unsafe {
        nix::sys::signal::signal(
            nix::sys::signal::Signal::SIGHUP,
            nix::sys::signal::SigHandler::SigIgn,
        )
    }?;

    let sig_fd = nix::sys::signalfd::SignalFd::with_flags(
        &chld_mask,
        nix::sys::signalfd::SfdFlags::SFD_NONBLOCK | nix::sys::signalfd::SfdFlags::SFD_CLOEXEC,
    )?;

    // Read termios and window size from the rfcomm tty (stdin) to propagate
    // to the PTY slave, so agetty's --keep-baud reads the right values.
    let rfcomm_termios = nix::sys::termios::tcgetattr(std::io::stdin())?;
    let mut ws: nix::pty::Winsize = unsafe { std::mem::zeroed() };
    unsafe { tiocgwinsz(libc::STDIN_FILENO, &mut ws) }.ok();

    let pty = nix::pty::openpty(Some(&ws), Some(&rfcomm_termios))?;

    // Put rfcomm stdin into raw mode: no echo, no line editing, no signal
    // generation. The rfcomm side is a dumb byte pipe; all line discipline
    // processing happens on the PTY slave side (managed by agetty/login/shell).
    let mut raw_termios = rfcomm_termios.clone();
    nix::sys::termios::cfmakeraw(&mut raw_termios);
    nix::sys::termios::tcsetattr(
        std::io::stdin(),
        nix::sys::termios::SetArg::TCSANOW,
        &raw_termios,
    )?;
    let master_fd = pty.master;
    let slave_fd = pty.slave;

    let child_pid = match unsafe { nix::unistd::fork() }? {
        nix::unistd::ForkResult::Child => {
            drop(master_fd);
            drop(sig_fd);

            // New session so the slave becomes our controlling terminal.
            nix::unistd::setsid().ok();
            nix::unistd::dup2(slave_fd.as_raw_fd(), libc::STDIN_FILENO).ok();
            nix::unistd::dup2(slave_fd.as_raw_fd(), libc::STDOUT_FILENO).ok();
            nix::unistd::dup2(slave_fd.as_raw_fd(), libc::STDERR_FILENO).ok();
            if slave_fd.as_raw_fd() > libc::STDERR_FILENO {
                drop(slave_fd);
            } else {
                // slave_fd IS one of 0/1/2 — don't close it via Drop
                std::mem::forget(slave_fd);
            }
            // Close any inherited fds above stderr (rfcomm fds, etc.)
            unsafe {
                libc::close_range(3, libc::c_uint::MAX, 0);
            }
            // Acquire controlling terminal
            unsafe { tiocsctty(libc::STDIN_FILENO, 0) }.ok();
            // Restore default signal handling in child
            unsafe {
                nix::sys::signal::signal(
                    nix::sys::signal::Signal::SIGHUP,
                    nix::sys::signal::SigHandler::SigDfl,
                )
            }
            .ok();
            nix::sys::signal::sigprocmask(
                nix::sys::signal::SigmaskHow::SIG_UNBLOCK,
                Some(&chld_mask),
                None,
            )
            .ok();

            let c_args: Vec<std::ffi::CString> = match child_cmd
                .iter()
                .map(|s| std::ffi::CString::new(s.as_str()))
                .collect::<Result<Vec<_>, _>>()
            {
                Result::Ok(args) => args,
                Err(e) => {
                    let msg = format!("session-proxy: invalid command argument: {e}\n");
                    nix::unistd::write(std::io::stderr(), msg.as_bytes()).ok();
                    std::process::exit(127);
                }
            };
            let Err(e) = nix::unistd::execvp(&c_args[0], &c_args);
            let msg = format!("session-proxy: exec {:?}: {e}\n", child_cmd[0]);
            nix::unistd::write(std::io::stderr(), msg.as_bytes()).ok();
            std::process::exit(127);
        }
        nix::unistd::ForkResult::Parent { child } => {
            drop(slave_fd);
            child
        }
    };

    // Set rfcomm (stdin) and master to nonblocking for poll loop.
    set_nonblock(libc::STDIN_FILENO)?;
    set_nonblock(master_fd.as_raw_fd())?;

    let exit_code = shuttle(
        libc::STDIN_FILENO,
        master_fd.as_raw_fd(),
        &sig_fd,
        child_pid,
    );

    // Ensure child is reaped.
    let status =
        match nix::sys::wait::waitpid(child_pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)) {
            Ok(nix::sys::wait::WaitStatus::StillAlive) | Err(_) => {
                nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGTERM).ok();
                nix::sys::wait::waitpid(child_pid, None).ok()
            }
            Ok(ws) => Some(ws),
        };

    match status {
        Some(nix::sys::wait::WaitStatus::Exited(_, code)) => {
            Ok(std::process::ExitCode::from(code as u8))
        }
        Some(nix::sys::wait::WaitStatus::Signaled(_, sig, _)) => {
            Ok(std::process::ExitCode::from(128u8.wrapping_add(sig as u8)))
        }
        _ => Ok(std::process::ExitCode::from(exit_code)),
    }
}

fn set_nonblock(fd: RawFd) -> anyhow::Result<()> {
    let flags = nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_GETFL)?;
    let mut oflags = nix::fcntl::OFlag::from_bits_truncate(flags);
    oflags |= nix::fcntl::OFlag::O_NONBLOCK;
    nix::fcntl::fcntl(fd, nix::fcntl::FcntlArg::F_SETFL(oflags))?;
    Ok(())
}

/// Bidirectional data shuttle between rfcomm fd and PTY master.
/// Returns a default exit code (0 for normal exit, 1 for error).
fn shuttle(
    rfcomm_fd: RawFd,
    master_fd: RawFd,
    sig_fd: &nix::sys::signalfd::SignalFd,
    child_pid: nix::unistd::Pid,
) -> u8 {
    let rfcomm_borrow = unsafe { BorrowedFd::borrow_raw(rfcomm_fd) };
    let master_borrow = unsafe { BorrowedFd::borrow_raw(master_fd) };
    let sig_borrow = sig_fd.as_fd();

    let mut rfcomm_to_master = Buffer::new();
    let mut master_to_rfcomm = Buffer::new();
    let mut child_exited = false;

    loop {
        let mut poll_fds = Vec::with_capacity(5);

        // Index 0: always poll signalfd for SIGCHLD
        poll_fds.push(nix::poll::PollFd::new(
            sig_borrow,
            nix::poll::PollFlags::POLLIN,
        ));

        // Track which indices correspond to which operations.
        // We build the poll array dynamically, so record offsets.
        let rfcomm_read_idx = if rfcomm_to_master.has_space() && !child_exited {
            let idx = poll_fds.len();
            poll_fds.push(nix::poll::PollFd::new(
                rfcomm_borrow,
                nix::poll::PollFlags::POLLIN,
            ));
            Some(idx)
        } else {
            None
        };

        let master_read_idx = if master_to_rfcomm.has_space() {
            let idx = poll_fds.len();
            poll_fds.push(nix::poll::PollFd::new(
                master_borrow,
                nix::poll::PollFlags::POLLIN,
            ));
            Some(idx)
        } else {
            None
        };

        let master_write_idx = if rfcomm_to_master.has_data() {
            let idx = poll_fds.len();
            poll_fds.push(nix::poll::PollFd::new(
                master_borrow,
                nix::poll::PollFlags::POLLOUT,
            ));
            Some(idx)
        } else {
            None
        };

        let rfcomm_write_idx = if master_to_rfcomm.has_data() {
            let idx = poll_fds.len();
            poll_fds.push(nix::poll::PollFd::new(
                rfcomm_borrow,
                nix::poll::PollFlags::POLLOUT,
            ));
            Some(idx)
        } else {
            None
        };

        // If child exited and no more data to shuttle, we're done.
        if child_exited && !rfcomm_to_master.has_data() && !master_to_rfcomm.has_data() {
            return 0;
        }

        match nix::poll::poll(&mut poll_fds, nix::poll::PollTimeout::from(1000u16)) {
            Err(nix::errno::Errno::EINTR) => continue,
            Err(_) => return 1,
            Ok(0) => {
                if !child_exited {
                    child_exited = try_reap(child_pid);
                }
                continue;
            }
            Ok(_) => {}
        }

        // Handle SIGCHLD
        if let Some(flags) = poll_fds[0].revents()
            && flags.contains(nix::poll::PollFlags::POLLIN)
        {
            sig_fd.read_signal().ok();
            child_exited = try_reap(child_pid);
        }

        let readable = nix::poll::PollFlags::POLLIN
            | nix::poll::PollFlags::POLLHUP
            | nix::poll::PollFlags::POLLERR;

        // Read from rfcomm -> buffer for master
        if let Some(idx) = rfcomm_read_idx
            && poll_fds[idx]
                .revents()
                .is_some_and(|f| f.intersects(readable))
        {
            match rfcomm_to_master.read_from(rfcomm_fd) {
                ReadResult::Ok | ReadResult::WouldBlock => {}
                ReadResult::Eof | ReadResult::Eio | ReadResult::Error => {
                    // Peer disconnected. Kill child and exit.
                    nix::sys::signal::kill(child_pid, nix::sys::signal::Signal::SIGHUP).ok();
                    return 0;
                }
            }
        }

        // Read from master -> buffer for rfcomm
        if let Some(idx) = master_read_idx
            && poll_fds[idx]
                .revents()
                .is_some_and(|f| f.intersects(readable))
        {
            match master_to_rfcomm.read_from(master_fd) {
                ReadResult::Ok | ReadResult::WouldBlock => {}
                ReadResult::Eof => {
                    // Slave has no open fds. During vhangup this is transient.
                    // If child already exited, we're done draining.
                    if child_exited {
                        return 0;
                    }
                    // Otherwise treat as transient (vhangup in progress, or
                    // login closed/reopened the slave).
                }
                ReadResult::Eio => {
                    // EIO on master: vhangup() in progress on the slave.
                    // Treat as transient per systemd ptyfwd.c pattern.
                    // poll() will re-arm when the slave is reopened.
                }
                ReadResult::Error => {
                    if child_exited {
                        return 0;
                    }
                }
            }
        }

        // Write buffered data to master
        if let Some(idx) = master_write_idx
            && poll_fds[idx]
                .revents()
                .is_some_and(|f| f.contains(nix::poll::PollFlags::POLLOUT))
        {
            rfcomm_to_master.write_to(master_fd);
        }

        // Write buffered data to rfcomm
        if let Some(idx) = rfcomm_write_idx
            && poll_fds[idx]
                .revents()
                .is_some_and(|f| f.contains(nix::poll::PollFlags::POLLOUT))
        {
            master_to_rfcomm.write_to(rfcomm_fd);
        }
    }
}

fn try_reap(pid: nix::unistd::Pid) -> bool {
    matches!(
        nix::sys::wait::waitpid(pid, Some(nix::sys::wait::WaitPidFlag::WNOHANG)),
        Ok(ws) if ws != nix::sys::wait::WaitStatus::StillAlive
    )
}

enum ReadResult {
    Ok,
    Eof,
    Eio,
    WouldBlock,
    Error,
}

struct Buffer {
    data: [u8; BUF_SIZE],
    start: usize,
    len: usize,
}

impl Buffer {
    fn new() -> Self {
        Self {
            data: [0u8; BUF_SIZE],
            start: 0,
            len: 0,
        }
    }

    fn has_space(&self) -> bool {
        self.len < BUF_SIZE
    }

    fn has_data(&self) -> bool {
        self.len > 0
    }

    fn read_from(&mut self, fd: RawFd) -> ReadResult {
        // Compact buffer if needed to make contiguous space at the end.
        if self.start > 0 && self.start + self.len >= BUF_SIZE {
            self.data.copy_within(self.start..self.start + self.len, 0);
            self.start = 0;
        }
        let write_pos = self.start + self.len;
        let space = BUF_SIZE - write_pos;
        if space == 0 {
            return ReadResult::Ok;
        }
        match nix::unistd::read(fd, &mut self.data[write_pos..write_pos + space]) {
            Result::Ok(0) => ReadResult::Eof,
            Result::Ok(n) => {
                self.len += n;
                ReadResult::Ok
            }
            Err(nix::errno::Errno::EAGAIN) => ReadResult::WouldBlock,
            Err(nix::errno::Errno::EIO) => ReadResult::Eio,
            Err(_) => ReadResult::Error,
        }
    }

    fn write_to(&mut self, fd: RawFd) {
        if self.len == 0 {
            return;
        }
        let buf = &self.data[self.start..self.start + self.len];
        match nix::unistd::write(unsafe { BorrowedFd::borrow_raw(fd) }, buf) {
            Result::Ok(n) => {
                self.start += n;
                self.len -= n;
                if self.len == 0 {
                    self.start = 0;
                }
            }
            Err(_) => {
                // On write error (EAGAIN, EIO, etc.) just ignore — next poll
                // iteration will retry when the fd is writable again.
            }
        }
    }
}
