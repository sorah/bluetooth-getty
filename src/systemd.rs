// org.freedesktop.systemd1.Manager client proxy and helpers for starting,
// stopping, and watching rfcomm-getty@rfcommN instances.

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait SystemdManager {
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn get_unit(&self, name: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

pub fn unit_name_for(unit_template: &str, dev_num: i16) -> String {
    format!("{unit_template}rfcomm{dev_num}.service")
}

pub async fn start_getty(
    conn: &zbus::Connection,
    unit_template: &str,
    dev_num: i16,
) -> zbus::Result<()> {
    let proxy = SystemdManagerProxy::new(conn).await?;
    let unit_name = unit_name_for(unit_template, dev_num);
    let job = proxy.start_unit(&unit_name, "replace").await?;
    tracing::info!(unit = %unit_name, job = %job.as_str(), "StartUnit dispatched");
    Ok(())
}

// Resolve the unit name to its D-Bus object path and return a Proxy
// bound to the org.freedesktop.systemd1.Unit interface. Used by
// wait_unit_inactive to subscribe to ActiveState changes.
async fn unit_proxy<'a>(conn: &zbus::Connection, unit_name: &str) -> zbus::Result<zbus::Proxy<'a>> {
    let manager = SystemdManagerProxy::new(conn).await?;
    let unit_path = manager.get_unit(unit_name).await?;
    zbus::Proxy::new(
        conn,
        "org.freedesktop.systemd1",
        unit_path,
        "org.freedesktop.systemd1.Unit",
    )
    .await
}

fn is_terminal_state(state: &str) -> bool {
    matches!(state, "inactive" | "failed")
}

// Block until the given unit reaches an `inactive` or `failed` state.
// Returns immediately if the unit is already in one of those states
// when we look. Intended to be awaited inside a tokio::select! so the
// watcher can be cancelled.
pub async fn wait_unit_inactive(conn: &zbus::Connection, unit_name: &str) -> zbus::Result<()> {
    use futures_util::StreamExt;

    let unit = unit_proxy(conn, unit_name).await?;

    // Sample the current state once before subscribing. The unit may
    // already be inactive — e.g. when a previous session was cleaned
    // up between our StartUnit returning and the watcher spinning up.
    if let Ok(state) = unit.get_property::<String>("ActiveState").await {
        tracing::debug!(%unit_name, state = %state, "initial ActiveState");
        if is_terminal_state(&state) {
            return Ok(());
        }
    }

    let mut changes = unit.receive_property_changed::<String>("ActiveState").await;
    while let Some(change) = changes.next().await {
        let Ok(state) = change.get().await else {
            continue;
        };
        tracing::debug!(%unit_name, state = %state, "ActiveState changed");
        if is_terminal_state(&state) {
            return Ok(());
        }
    }
    Ok(())
}
