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

type SessionMap = std::sync::Arc<std::sync::Mutex<std::collections::HashMap<String, SessionEntry>>>;

// What the sessions map stores for each active device. `cancel` is a
// oneshot channel the watcher awaits — sending () tells it to tear down.
// `id` disambiguates entries across reconnects so a natural teardown
// doesn't accidentally remove a newer session that replaced it.
struct SessionEntry {
    id: u64,
    dev_num: i16,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
}

pub struct Profile {
    unit_template: String,
    sessions: SessionMap,
    next_id: std::sync::atomic::AtomicU64,
}

impl Profile {
    pub fn new(unit_template: String) -> Self {
        Self {
            unit_template,
            sessions: std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
            next_id: std::sync::atomic::AtomicU64::new(1),
        }
    }

    fn allocate_id(&self) -> u64 {
        self.next_id
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    }

    // Remove any existing session for `device_key` and nudge its
    // watcher to tear down. Called from NewConnection (reconnect) and
    // RequestDisconnection (explicit). Idempotent.
    fn evict_existing(&self, device_key: &str) {
        let stale = self
            .sessions
            .lock()
            .expect("sessions mutex poisoned")
            .remove(device_key);
        let Some(mut entry) = stale else {
            return;
        };
        tracing::warn!(
            device = device_key,
            dev_num = entry.dev_num,
            "evicting existing session"
        );
        if let Some(tx) = entry.cancel.take() {
            let _ = tx.send(());
        }
    }

    // Register a live session in the map and spawn its watcher task.
    fn register_session(
        &self,
        conn: zbus::Connection,
        device_key: String,
        dev_num: i16,
        tty_fd: std::os::fd::OwnedFd,
    ) {
        let id = self.allocate_id();
        let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();

        self.sessions
            .lock()
            .expect("sessions mutex poisoned")
            .insert(
                device_key.clone(),
                SessionEntry {
                    id,
                    dev_num,
                    cancel: Some(cancel_tx),
                },
            );

        let watcher = SessionWatcher {
            conn,
            unit_name: crate::systemd::unit_name_for(&self.unit_template, dev_num),
            id,
            dev_num,
            tty_fd,
            sessions: self.sessions.clone(),
            device_key,
        };
        tokio::spawn(watcher.run(cancel_rx));
    }

    // Body of Profile1.NewConnection. Kept off the #[interface] impl so
    // the interface method can stay a thin delegator.
    async fn handle_new_connection(
        &self,
        conn: &zbus::Connection,
        device: zbus::zvariant::ObjectPath<'_>,
        fd: zbus::zvariant::OwnedFd,
        options: std::collections::HashMap<String, zbus::zvariant::OwnedValue>,
    ) -> zbus::fdo::Result<()> {
        use std::os::fd::AsRawFd;

        tracing::debug!(
            %device,
            version = ?options.get("Version"),
            features = ?options.get("Features"),
            "Profile1.NewConnection"
        );

        let dev_num = crate::rfcomm::create_tty(fd.as_raw_fd()).map_err(|e| {
            tracing::error!(error = ?e, "RFCOMMCREATEDEV failed");
            zbus::fdo::Error::Failed(format!("RFCOMMCREATEDEV: {e}"))
        })?;
        tracing::info!(%device, dev_num, "RFCOMM tty created");

        let tty_fd = crate::rfcomm::prime_tty(dev_num).map_err(|e| {
            tracing::error!(error = ?e, dev_num, "prime_tty failed");
            let _ = crate::rfcomm::release_tty(dev_num);
            zbus::fdo::Error::Failed(format!("prime_tty: {e}"))
        })?;
        tracing::info!(dev_num, "CLOCAL set on /dev/rfcomm{dev_num}");

        drop(fd);

        let device_key = device.to_string();
        self.evict_existing(&device_key);

        if let Err(e) = crate::systemd::start_getty(conn, &self.unit_template, dev_num).await {
            tracing::error!(error = ?e, dev_num, "StartUnit failed");
            let _ = crate::rfcomm::release_tty(dev_num);
            drop(tty_fd);
            return Err(zbus::fdo::Error::Failed(format!("StartUnit: {e}")));
        }

        self.register_session(conn.clone(), device_key, dev_num, tty_fd);
        Ok(())
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
        self.handle_new_connection(conn, device, fd, options).await
    }

    async fn request_disconnection(
        &self,
        device: zbus::zvariant::ObjectPath<'_>,
    ) -> zbus::fdo::Result<()> {
        tracing::info!(%device, "Profile1.RequestDisconnection");
        self.evict_existing(&device.to_string());
        Ok(())
    }
}

// Detached per-session task. Owns tty_fd and the dev_num, watches the
// getty unit's ActiveState over D-Bus, and on either a natural
// transition to inactive/failed OR an explicit cancel, runs cleanup:
// release_tty, drop tty_fd (closing our tty_port holder), remove the
// session's entry from the shared map (iff it still belongs to us).
struct SessionWatcher {
    conn: zbus::Connection,
    unit_name: String,
    id: u64,
    dev_num: i16,
    tty_fd: std::os::fd::OwnedFd,
    sessions: SessionMap,
    device_key: String,
}

impl SessionWatcher {
    async fn run(self, cancel_rx: tokio::sync::oneshot::Receiver<()>) {
        tokio::select! {
            res = crate::systemd::wait_unit_inactive(&self.conn, &self.unit_name) => {
                match res {
                    Ok(()) => tracing::info!(
                        dev_num = self.dev_num,
                        unit = %self.unit_name,
                        "getty unit is inactive"
                    ),
                    Err(e) => tracing::warn!(
                        error = ?e,
                        dev_num = self.dev_num,
                        "wait_unit_inactive failed; tearing down anyway"
                    ),
                }
            }
            _ = cancel_rx => {
                tracing::info!(dev_num = self.dev_num, "session cancelled");
            }
        }
        self.cleanup();
    }

    fn cleanup(self) {
        let SessionWatcher {
            id,
            dev_num,
            tty_fd,
            sessions,
            device_key,
            ..
        } = self;

        if let Err(e) = crate::rfcomm::release_tty(dev_num) {
            tracing::warn!(error = ?e, dev_num, "release_tty failed during cleanup");
        }
        drop(tty_fd);

        let mut map = sessions.lock().expect("sessions mutex poisoned");
        if map.get(&device_key).map(|e| e.id) == Some(id) {
            map.remove(&device_key);
        }
        tracing::info!(dev_num, "session cleanup complete");
    }
}
