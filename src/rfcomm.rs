// RFCOMM tty ioctls. Layout and flag numbers from bluez/lib/bluetooth/rfcomm.h:
// struct rfcomm_dev_req is 24 bytes with natural #[repr(C)] padding.
// The canonical REUSE_DLC | RELEASE_ONHUP call pattern lives in
// bluez/tools/rfcomm.c:491-499.

use std::os::fd::{AsRawFd, FromRawFd};

const AF_BLUETOOTH: libc::c_int = 31;
const BTPROTO_RFCOMM: libc::c_int = 3;

#[repr(C)]
#[derive(Default, Clone, Copy)]
pub struct RfcommDevReq {
    pub dev_id: i16,
    pub flags: u32,
    pub src: [u8; 6],
    pub dst: [u8; 6],
    pub channel: u8,
}

// Bit numbers, not masks. cf. rfcomm.h:63-66.
pub const RFCOMM_REUSE_DLC: u32 = 0;
pub const RFCOMM_HANGUP_NOW: u32 = 2;

// _IOW('R', 200, int) = 0x400452c8; _IOW('R', 201, int) = 0x400452c9.
// The kernel argument is actually a pointer to RfcommDevReq, not sizeof(int),
// so nix's size check would reject these — use the `_bad` variant.
nix::ioctl_write_ptr_bad!(rfcomm_create_dev_raw, 0x400452c8, RfcommDevReq);
nix::ioctl_write_ptr_bad!(rfcomm_release_dev_raw, 0x400452c9, RfcommDevReq);

#[repr(C)]
#[derive(Default, Clone, Copy)]
struct SockaddrRc {
    rc_family: libc::sa_family_t,
    rc_bdaddr: [u8; 6],
    rc_channel: u8,
}

fn getsockname_rc(fd: std::os::fd::RawFd) -> std::io::Result<SockaddrRc> {
    let mut addr = SockaddrRc::default();
    let mut len = std::mem::size_of::<SockaddrRc>() as libc::socklen_t;
    let rc = unsafe { libc::getsockname(fd, (&mut addr as *mut SockaddrRc).cast(), &mut len) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(addr)
}

fn getpeername_rc(fd: std::os::fd::RawFd) -> std::io::Result<SockaddrRc> {
    let mut addr = SockaddrRc::default();
    let mut len = std::mem::size_of::<SockaddrRc>() as libc::socklen_t;
    let rc = unsafe { libc::getpeername(fd, (&mut addr as *mut SockaddrRc).cast(), &mut len) };
    if rc < 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(addr)
}

// Promote a connected RFCOMM DLC socket into a /dev/rfcommN tty by issuing
// RFCOMMCREATEDEV with REUSE_DLC | RELEASE_ONHUP. Returns the device number.
// Must be called on the DLC socket itself — the kernel reuses its private
// DLC state. See tools/rfcomm.c:499.
pub fn create_tty(fd: std::os::fd::RawFd) -> anyhow::Result<i16> {
    let local = getsockname_rc(fd)?;
    let remote = getpeername_rc(fd)?;

    if local.rc_family as libc::c_int != AF_BLUETOOTH
        || remote.rc_family as libc::c_int != AF_BLUETOOTH
    {
        anyhow::bail!(
            "socket is not AF_BLUETOOTH (local={}, remote={})",
            local.rc_family,
            remote.rc_family,
        );
    }

    // Deliberately NOT setting RFCOMM_RELEASE_ONHUP. That flag fires on
    // *any* tty hangup, including the hangup kernel does when agetty's
    // session leader exits on user logout — it would tear down the
    // rfcomm_dev on every logout and stop systemd from giving the peer
    // a fresh login prompt. We release the device explicitly in
    // Profile1.RequestDisconnection instead, which bluetoothd calls
    // when the *peer* (not the login session) disconnects.
    let req = RfcommDevReq {
        dev_id: -1,
        flags: 1 << RFCOMM_REUSE_DLC,
        src: local.rc_bdaddr,
        dst: remote.rc_bdaddr,
        channel: remote.rc_channel,
    };

    let dev_num = unsafe { rfcomm_create_dev_raw(fd, &req) }?;
    Ok(dev_num as i16)
}

// Open /dev/rfcommN, set CLOCAL on its termios, and return the owned fd.
// The caller MUST hold the fd for the lifetime of the session — if this
// was the only holder, closing it triggers tty_port_shutdown which in
// the rfcomm driver closes the DLC, at which point every subsequent
// open by systemd/agetty sees an EOF'd tty and exits immediately.
//
// Why CLOCAL: systemd's StandardInput=tty opens /dev/rfcommN in blocking
// mode (no O_NONBLOCK). On a freshly-ioctl-created rfcomm tty the
// driver reports DCD off, so tty_port_block_til_ready blocks the open
// forever. CLOCAL makes the wait short-circuit.
//
// udev may lag the ioctl by a few ms, so retry ENOENT/ENODEV briefly.
pub fn prime_tty(dev_num: i16) -> anyhow::Result<std::os::fd::OwnedFd> {
    let path = format!("/dev/rfcomm{dev_num}");

    let owned = {
        let mut last_err: Option<nix::Error> = None;
        let mut opened: Option<std::os::fd::OwnedFd> = None;
        for attempt in 0..30u32 {
            // O_NONBLOCK on our own open so *we* don't block on DCD.
            // Once CLOCAL is set, later opens from systemd/agetty see
            // the CLOCAL termios and don't block.
            match nix::fcntl::open(
                path.as_str(),
                nix::fcntl::OFlag::O_RDWR
                    | nix::fcntl::OFlag::O_NOCTTY
                    | nix::fcntl::OFlag::O_NONBLOCK,
                nix::sys::stat::Mode::empty(),
            ) {
                Ok(raw_fd) => {
                    opened = Some(unsafe { std::os::fd::OwnedFd::from_raw_fd(raw_fd) });
                    break;
                }
                Err(
                    e @ (nix::errno::Errno::ENOENT
                    | nix::errno::Errno::ENODEV
                    | nix::errno::Errno::EACCES),
                ) => {
                    tracing::debug!(
                        dev_num,
                        attempt,
                        error = %e,
                        "waiting for /dev/rfcomm{dev_num} to appear"
                    );
                    last_err = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(50));
                    continue;
                }
                Err(e) => return Err(e.into()),
            }
        }
        match opened {
            Some(fd) => fd,
            None => {
                return Err(last_err
                    .map(|e| anyhow::anyhow!(e))
                    .unwrap_or_else(|| anyhow::anyhow!("timed out opening rfcomm tty")));
            }
        }
    };

    let mut termios = nix::sys::termios::tcgetattr(&owned)?;
    termios.control_flags |= nix::sys::termios::ControlFlags::CLOCAL;
    nix::sys::termios::tcsetattr(&owned, nix::sys::termios::SetArg::TCSANOW, &termios)?;

    Ok(owned)
}

// Manually tear down /dev/rfcommN via RFCOMMRELEASEDEV on a fresh control
// socket. Used only to clean up when StartUnit fails after a successful
// create_tty — the normal teardown path is RELEASE_ONHUP fired by peer
// hangup.
pub fn release_tty(dev_id: i16) -> anyhow::Result<()> {
    let ctl = unsafe { libc::socket(AF_BLUETOOTH, libc::SOCK_RAW, BTPROTO_RFCOMM) };
    if ctl < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    // Wrap in OwnedFd immediately so it gets closed on all exit paths.
    let ctl_fd = unsafe { std::os::fd::OwnedFd::from_raw_fd(ctl) };

    let req = RfcommDevReq {
        dev_id,
        flags: 1 << RFCOMM_HANGUP_NOW,
        ..RfcommDevReq::default()
    };
    unsafe { rfcomm_release_dev_raw(ctl_fd.as_raw_fd(), &req) }?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn rfcomm_dev_req_is_24_bytes() {
        assert_eq!(std::mem::size_of::<crate::rfcomm::RfcommDevReq>(), 24);
    }
}
