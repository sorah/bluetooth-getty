mod profile;
mod rfcomm;
mod systemd;

#[derive(clap::Parser, Debug)]
#[command(version, about = "Bluetooth SPP -> serial-getty@ bridge")]
struct Args {
    /// Human-readable profile name published in the SDP record.
    #[arg(long, default_value = "Bluetooth getty")]
    name: String,

    /// Service UUID to register. Default is the SPP UUID.
    #[arg(long, default_value = "00001101-0000-1000-8000-00805f9b34fb")]
    uuid: String,

    /// RFCOMM channel number embedded in the auto-generated SDP record.
    #[arg(long, default_value_t = 1)]
    channel: u16,

    /// D-Bus object path hosting our Profile1 implementation.
    #[arg(long, default_value = "/jp/sorah/BluetoothGetty/spp")]
    object_path: String,

    /// Systemd unit template prefix. `rfcommN.service` is appended.
    /// Defaults to our own `bluetooth-getty-session@` template, which
    /// is a near-copy of `serial-getty@.service` with `TTYVHangup=yes`
    /// removed — that directive would destroy the rfcomm tty before
    /// agetty could open it.
    #[arg(long, default_value = "bluetooth-getty-session@")]
    unit_template: String,

    /// Well-known bus name to request on the system bus.
    #[arg(long, default_value = "jp.sorah.BluetoothGetty")]
    bus_name: String,

    /// Force bluetoothd to require pairing before accepting the RFCOMM channel.
    #[arg(long)]
    require_authentication: bool,

    /// Force bluetoothd to require authorization before accepting the channel.
    #[arg(long)]
    require_authorization: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = <Args as clap::Parser>::parse();

    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                tracing_subscriber::EnvFilter::new("bluetooth_getty=info,zbus=warn")
            }),
        )
        .init();

    let object_path = zbus::zvariant::ObjectPath::try_from(args.object_path.as_str())?;

    let profile = crate::profile::Profile::new(args.unit_template.clone());

    let connection = zbus::connection::Builder::system()?
        .name(args.bus_name.as_str())?
        .serve_at(object_path.clone(), profile)?
        .build()
        .await?;

    tracing::info!(bus_name = %args.bus_name, object_path = %object_path.as_str(), "D-Bus connection up");

    let pm = crate::profile::ProfileManager1Proxy::new(&connection).await?;

    let mut options: std::collections::HashMap<&str, zbus::zvariant::Value<'_>> =
        std::collections::HashMap::new();
    options.insert("Name", zbus::zvariant::Value::from(args.name.as_str()));
    options.insert("Role", zbus::zvariant::Value::from("server"));
    options.insert("Channel", zbus::zvariant::Value::from(args.channel));
    if args.require_authentication {
        options.insert("RequireAuthentication", zbus::zvariant::Value::from(true));
    }
    if args.require_authorization {
        options.insert("RequireAuthorization", zbus::zvariant::Value::from(true));
    }

    pm.register_profile(&object_path, args.uuid.as_str(), options)
        .await?;
    tracing::info!(
        uuid = %args.uuid,
        channel = args.channel,
        "Profile registered with bluetoothd"
    );

    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
    tokio::select! {
        _ = sigterm.recv() => tracing::info!("SIGTERM received"),
        _ = sigint.recv() => tracing::info!("SIGINT received"),
    }

    if let Err(e) = pm.unregister_profile(&object_path).await {
        tracing::warn!(error = ?e, "UnregisterProfile failed");
    } else {
        tracing::info!("Profile unregistered");
    }

    drop(pm);
    drop(connection);

    tracing::info!("bluetooth-getty exiting");
    Ok(())
}
