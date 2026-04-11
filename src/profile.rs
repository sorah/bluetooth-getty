// org.bluez.Profile1 server-side implementation plus the client proxy for
// org.bluez.ProfileManager1.

#[zbus::proxy(
    interface = "org.bluez.ProfileManager1",
    default_service = "org.bluez",
    default_path = "/org/bluez"
)]
pub trait ProfileManager1 {
    fn register_profile(
        &self,
        profile: &zbus::zvariant::ObjectPath<'_>,
        uuid: &str,
        options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>>,
    ) -> zbus::Result<()>;

    fn unregister_profile(&self, profile: &zbus::zvariant::ObjectPath<'_>) -> zbus::Result<()>;
}

// Per-connection session state we track in the daemon.
struct Session {
    dev_num: i16,
    // Held for the lifetime of the session. If we close this before
    // RequestDisconnection, and we're the only holder, rfcomm's
    // tty_port_shutdown runs and closes the underlying DLC — at which
    // point agetty's subsequent read() on its own open of the tty
    // returns EOF and exits in a loop.
    _tty_fd: std::os::fd::OwnedFd,
}

pub struct Profile {
    pub unit_template: String,
    // Active sessions keyed by BlueZ device object path. We insert in
    // NewConnection and remove in RequestDisconnection, which bluetoothd
    // calls when the *peer* (not the user's login session) disconnects.
    sessions: std::sync::Mutex<std::collections::HashMap<String, Session>>,
}

impl Profile {
    pub fn new(unit_template: String) -> Self {
        Self {
            unit_template,
            sessions: std::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }
}

#[zbus::interface(name = "org.bluez.Profile1")]
impl Profile {
    async fn release(&self) -> zbus::fdo::Result<()> {
        tracing::info!("Profile1.Release");
        Ok(())
    }

    async fn new_connection(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        device: zbus::zvariant::ObjectPath<'_>,
        fd: zbus::zvariant::OwnedFd,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        use std::os::fd::AsRawFd;

        let raw_fd = fd.as_raw_fd();
        tracing::debug!(
            %device,
            fd = raw_fd,
            version = ?options.get("Version"),
            features = ?options.get("Features"),
            "Profile1.NewConnection"
        );

        let dev_num = crate::rfcomm::create_tty(raw_fd).map_err(|e| {
            tracing::error!(error = ?e, "RFCOMMCREATEDEV failed");
            zbus::fdo::Error::Failed(format!("RFCOMMCREATEDEV: {e}"))
        })?;

        tracing::info!(%device, dev_num, "RFCOMM tty created");

        // Open /dev/rfcommN in-process, set CLOCAL on its termios, and
        // keep the fd alive for the lifetime of the session. Two
        // independent requirements are satisfied by this single step:
        //
        //   1. CLOCAL tells the kernel's tty_port_block_til_ready to
        //      treat carrier as always-on. Without it, systemd's
        //      StandardInput=tty opens /dev/rfcommN in blocking mode
        //      and hangs in tty_port_block_til_ready because a fresh
        //      rfcomm tty reports DCD off until the DLC state
        //      reconciles. agetty never reaches the login prompt.
        //
        //   2. Keeping a reference means we stay a tty_port holder.
        //      If we were the only holder and closed, the rfcomm
        //      tty_port's shutdown() path would run and close the
        //      underlying DLC — every later agetty open would read
        //      EOF and exit in a restart loop.
        let tty_fd = crate::rfcomm::prime_tty(dev_num).map_err(|e| {
            tracing::error!(error = ?e, dev_num, "prime_tty failed");
            let _ = crate::rfcomm::release_tty(dev_num);
            zbus::fdo::Error::Failed(format!("prime_tty: {e}"))
        })?;
        tracing::info!(dev_num, "CLOCAL set on /dev/rfcomm{dev_num}");

        drop(fd);

        {
            let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
            sessions.insert(
                device.to_string(),
                Session {
                    dev_num,
                    _tty_fd: tty_fd,
                },
            );
        }

        if let Err(e) = crate::systemd::start_getty(conn, &self.unit_template, dev_num).await {
            tracing::error!(error = ?e, dev_num, "StartUnit failed; releasing tty");
            {
                let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
                sessions.remove(&device.to_string());
            }
            if let Err(rel_err) = crate::rfcomm::release_tty(dev_num) {
                tracing::error!(
                    error = ?rel_err,
                    dev_num,
                    "RFCOMMRELEASEDEV cleanup also failed"
                );
            }
            return Err(zbus::fdo::Error::Failed(format!("StartUnit: {e}")));
        }

        Ok(())
    }

    async fn request_disconnection(
        &self,
        #[zbus(connection)] conn: &zbus::Connection,
        device: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<()> {
        // bluetoothd calls this when the peer's RFCOMM channel goes
        // away. Since this daemon now owns the unit lifecycle (no
        // BindsTo in rfcomm-getty@.service), tear down explicitly:
        //   1. StopUnit — so agetty exits via systemd, not because
        //      its tty got yanked out from under it.
        //   2. release_tty — free the rfcomm_dev and /dev/rfcommN.
        //   3. drop(tty_fd) — our tty_port anchor, no longer needed.
        let session = {
            let mut sessions = self.sessions.lock().expect("sessions mutex poisoned");
            sessions.remove(&device.to_string())
        };
        match session {
            Some(Session { dev_num, .. }) => {
                tracing::info!(%device, dev_num, "Profile1.RequestDisconnection");
                if let Err(e) = crate::systemd::stop_getty(conn, &self.unit_template, dev_num).await
                {
                    tracing::warn!(error = ?e, dev_num, "StopUnit failed on disconnect");
                }
                if let Err(e) = crate::rfcomm::release_tty(dev_num) {
                    tracing::warn!(
                        error = ?e,
                        dev_num,
                        "release_tty failed on disconnect"
                    );
                }
            }
            None => {
                tracing::warn!(
                    %device,
                    "Profile1.RequestDisconnection for unknown device"
                );
            }
        }
        Ok(())
    }
}
