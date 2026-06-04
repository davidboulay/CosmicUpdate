// SPDX-License-Identifier: GPL-3.0-only
//
// Update discovery backend.
//
// System (apt/rpm/...) updates are queried through PackageKit over D-Bus, the
// same mechanism the COSMIC Store uses. Flatpak app updates are queried with
// the `flatpak` CLI. Both run without root for *checking*; refreshing the
// system package cache triggers a polkit prompt handled by the desktop agent.

use std::time::Duration;

use futures::StreamExt;

/// Where an update comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    System,
    Flatpak,
}

/// A single pending update.
#[derive(Debug, Clone)]
pub struct UpdateItem {
    pub name: String,
    pub version: String,
    pub summary: String,
    pub source: Source,
}

/// The result of a check: the updates found plus any non-fatal errors so the
/// UI can show partial results (e.g. flatpak worked but PackageKit failed).
#[derive(Debug, Clone, Default)]
pub struct UpdateReport {
    pub system: Vec<UpdateItem>,
    pub flatpak: Vec<UpdateItem>,
    pub errors: Vec<String>,
}

impl UpdateReport {
    pub fn total(&self) -> usize {
        self.system.len() + self.flatpak.len()
    }
}

// PackageKit's daemon object: hands out per-request transaction objects.
#[zbus::proxy(
    interface = "org.freedesktop.PackageKit",
    default_service = "org.freedesktop.PackageKit",
    default_path = "/org/freedesktop/PackageKit"
)]
trait PackageKit {
    fn create_transaction(&self) -> zbus::Result<zbus::zvariant::OwnedObjectPath>;
}

// A transaction is bound to the connection that created it, so the same
// `zbus::Connection` must issue both `create_transaction` and the call below.
#[zbus::proxy(
    interface = "org.freedesktop.PackageKit.Transaction",
    default_service = "org.freedesktop.PackageKit"
)]
trait PkTransaction {
    /// `filter` is a PkBitfield; `1 << PK_FILTER_ENUM_NONE` (== 2) means no filter.
    fn get_updates(&self, filter: u64) -> zbus::Result<()>;
    fn refresh_cache(&self, force: bool) -> zbus::Result<()>;

    #[zbus(signal)]
    fn package(&self, info: u32, package_id: String, summary: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn finished(&self, exit: u32, runtime: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn error_code(&self, code: u32, details: String) -> zbus::Result<()>;
}

const PK_FILTER_NONE: u64 = 1 << 1;
const CHECK_TIMEOUT: Duration = Duration::from_secs(180);

/// Build a fresh transaction proxy on the given connection.
async fn new_transaction<'a>(
    conn: &zbus::Connection,
    pk: &PackageKitProxy<'a>,
) -> zbus::Result<PkTransactionProxy<'a>> {
    let path = pk.create_transaction().await?;
    PkTransactionProxy::builder(conn).path(path)?.build().await
}

/// Ask PackageKit for available system updates (reads the local cache).
async fn pk_get_updates(conn: &zbus::Connection) -> zbus::Result<Vec<UpdateItem>> {
    let pk = PackageKitProxy::new(conn).await?;
    let tx = new_transaction(conn, &pk).await?;

    // Subscribe before issuing the call so no signal is missed.
    let mut packages = tx.receive_package().await?;
    let mut finished = tx.receive_finished().await?;
    let mut errors = tx.receive_error_code().await?;

    tx.get_updates(PK_FILTER_NONE).await?;

    let mut items = Vec::new();
    loop {
        tokio::select! {
            Some(sig) = packages.next() => {
                if let Ok(args) = sig.args() {
                    // package_id is "name;version;arch;data"
                    let id = args.package_id().to_string();
                    let mut parts = id.splitn(4, ';');
                    let name = parts.next().unwrap_or_default().to_string();
                    let version = parts.next().unwrap_or_default().to_string();
                    items.push(UpdateItem {
                        name,
                        version,
                        summary: args.summary().to_string(),
                        source: Source::System,
                    });
                }
            }
            Some(_) = finished.next() => break,
            Some(sig) = errors.next() => {
                let details = sig.args().map(|a| a.details().to_string()).unwrap_or_default();
                return Err(zbus::Error::Failure(format!("PackageKit: {details}")));
            }
            else => break,
        }
    }

    items.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(items)
}

/// Refresh the system package cache (downloads new metadata). Requires polkit
/// authorization, which the COSMIC desktop's polkit agent prompts for.
async fn pk_refresh_cache(conn: &zbus::Connection) -> zbus::Result<()> {
    let pk = PackageKitProxy::new(conn).await?;
    let tx = new_transaction(conn, &pk).await?;

    let mut finished = tx.receive_finished().await?;
    let mut errors = tx.receive_error_code().await?;

    tx.refresh_cache(false).await?;

    loop {
        tokio::select! {
            Some(_) = finished.next() => break,
            Some(sig) = errors.next() => {
                let details = sig.args().map(|a| a.details().to_string()).unwrap_or_default();
                return Err(zbus::Error::Failure(format!("refresh: {details}")));
            }
            else => break,
        }
    }
    Ok(())
}

/// Full system path: optionally refresh, then read the update list.
async fn system_updates(refresh: bool) -> Result<Vec<UpdateItem>, String> {
    let conn = zbus::Connection::system()
        .await
        .map_err(|e| format!("D-Bus connection failed: {e}"))?;

    if refresh {
        // A refresh failure is usually a per-repo warning (missing GPG key,
        // unsupported arch, transient network). It is not actionable from the
        // applet and shouldn't hide the updates we can still read from cache,
        // so we log it and carry on rather than aborting the whole check.
        if let Err(e) = pk_refresh_cache(&conn).await {
            tracing::warn!("system cache refresh reported: {e}");
        }
    }

    pk_get_updates(&conn)
        .await
        .map_err(|e| format!("Could not read system updates ({e})"))
}

/// Query flatpak for updatable refs. `remote-ls --updates` contacts the remotes,
/// so it always reflects the latest state regardless of `refresh`.
async fn flatpak_updates() -> Result<Vec<UpdateItem>, String> {
    let output = tokio::process::Command::new("flatpak")
        .args([
            "remote-ls",
            "--updates",
            // `name` is the human-readable application name (e.g. "Firefox"),
            // `application` is the reverse-DNS ID used as a fallback.
            "--columns=name,application,version",
        ])
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        // flatpak not installed is not an error worth surfacing loudly.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("flatpak failed to start: {e}")),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("flatpak: {}", stderr.trim()));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut items = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        // `--columns` output is tab-separated. Names can contain spaces, so we
        // must split on tabs rather than whitespace.
        let mut cols = line.split('\t');
        let name = cols.next().unwrap_or_default().trim();
        let app_id = cols.next().unwrap_or_default().trim();
        let version = cols.next().unwrap_or_default().trim();
        // Prefer the friendly name; fall back to the application ID.
        let display = if name.is_empty() { app_id } else { name };
        if display.is_empty() {
            continue;
        }
        items.push(UpdateItem {
            name: display.to_string(),
            version: version.to_string(),
            summary: String::new(),
            source: Source::Flatpak,
        });
    }
    items.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
    Ok(items)
}

/// Check both sources concurrently. Never fails as a whole — partial failures
/// are collected in `errors`.
pub async fn check_for_updates(refresh: bool) -> UpdateReport {
    let work = async {
        let (system, flatpak) = tokio::join!(system_updates(refresh), flatpak_updates());
        (system, flatpak)
    };

    let (system, flatpak) = match tokio::time::timeout(CHECK_TIMEOUT, work).await {
        Ok(pair) => pair,
        Err(_) => (
            Err("Timed out checking for system updates".to_string()),
            Err("Timed out checking for flatpak updates".to_string()),
        ),
    };

    let mut report = UpdateReport::default();
    match system {
        Ok(v) => report.system = v,
        Err(e) => report.errors.push(e),
    }
    match flatpak {
        Ok(v) => report.flatpak = v,
        Err(e) => report.errors.push(e),
    }
    report
}
