// org.freedesktop.systemd1.Manager client proxy and helpers for starting
// and stopping rfcomm-getty@rfcommN instances.

#[zbus::proxy(
    interface = "org.freedesktop.systemd1.Manager",
    default_service = "org.freedesktop.systemd1",
    default_path = "/org/freedesktop/systemd1"
)]
pub trait SystemdManager {
    fn start_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
    fn stop_unit(&self, name: &str, mode: &str) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

fn unit_name_for(unit_template: &str, dev_num: i16) -> String {
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

pub async fn stop_getty(
    conn: &zbus::Connection,
    unit_template: &str,
    dev_num: i16,
) -> zbus::Result<()> {
    let proxy = SystemdManagerProxy::new(conn).await?;
    let unit_name = unit_name_for(unit_template, dev_num);
    let job = proxy.stop_unit(&unit_name, "replace").await?;
    tracing::info!(unit = %unit_name, job = %job.as_str(), "StopUnit dispatched");
    Ok(())
}
