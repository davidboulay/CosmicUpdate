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
    /// Full PackageKit id ("name;version;arch;data") for system updates — needed
    /// to drive an in-place update transaction. Empty for flatpak (which we
    /// update as a whole).
    pub package_id: String,
}

/// Progress events emitted while installing updates.
#[derive(Debug, Clone)]
pub enum InstallEvent {
    /// Overall completion fraction in `0.0..=1.0`.
    Progress(f32),
    /// The install finished; `Err` carries a human-readable failure summary.
    Done(Result<(), String>),
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

    /// Broadcast whenever the set of available updates may have changed — e.g.
    /// right after the COSMIC Store (or anything else) installs updates. This is
    /// how the applet learns to refresh itself without waiting for its timer.
    #[zbus(signal)]
    fn updates_changed(&self) -> zbus::Result<()>;
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
    /// Install the given package ids. `transaction_flags` is a PkBitfield.
    fn update_packages(&self, transaction_flags: u64, package_ids: &[&str]) -> zbus::Result<()>;
    /// Hints for the daemon. `interactive=true` lets the polkit agent show a
    /// prompt rather than failing silently when authorization is needed.
    fn set_hints(&self, hints: &[&str]) -> zbus::Result<()>;

    /// Overall completion percentage (0–100, or 101 when unknown).
    #[zbus(property)]
    fn percentage(&self) -> zbus::Result<u32>;

    #[zbus(signal)]
    fn package(&self, info: u32, package_id: String, summary: String) -> zbus::Result<()>;

    #[zbus(signal)]
    fn item_progress(&self, package_id: String, status: u32, percentage: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn finished(&self, exit: u32, runtime: u32) -> zbus::Result<()>;

    #[zbus(signal)]
    fn error_code(&self, code: u32, details: String) -> zbus::Result<()>;
}

const PK_FILTER_NONE: u64 = 1 << 1;
// PK_TRANSACTION_FLAG_ENUM_ONLY_TRUSTED — only install signed packages, the same
// flag the COSMIC Store uses for its update transactions.
const PK_FLAG_ONLY_TRUSTED: u64 = 1 << 1;
// PackageKit error code that the COSMIC Store treats as non-fatal during a
// transaction; mirror that so a benign notice doesn't abort the install.
const PK_ERROR_TRANSACTION_CANCELLED_NONFATAL: u32 = 48;
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
                        package_id: id,
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
            package_id: String::new(),
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

/// Install the given system package ids via PackageKit. Authorization is
/// requested through polkit (the COSMIC agent shows the prompt). `on_progress`
/// is called with this step's completion fraction (0.0..=1.0).
async fn pk_update_packages(
    conn: &zbus::Connection,
    package_ids: &[String],
    on_progress: impl Fn(f32),
) -> Result<(), String> {
    let pk = PackageKitProxy::new(conn)
        .await
        .map_err(|e| format!("D-Bus connection failed: {e}"))?;
    let tx = new_transaction(conn, &pk)
        .await
        .map_err(|e| format!("Could not create update transaction ({e})"))?;

    // Let the polkit agent prompt for authorization instead of failing silently.
    let _ = tx.set_hints(&["interactive=true"]).await;

    // Subscribe before issuing the call so no signal is missed.
    let mut finished = tx.receive_finished().await.map_err(|e| e.to_string())?;
    let mut errors = tx.receive_error_code().await.map_err(|e| e.to_string())?;
    let mut progress = tx.receive_item_progress().await.map_err(|e| e.to_string())?;

    let ids: Vec<&str> = package_ids.iter().map(String::as_str).collect();
    tx.update_packages(PK_FLAG_ONLY_TRUSTED, &ids)
        .await
        .map_err(|e| format!("Could not start update ({e})"))?;

    loop {
        tokio::select! {
            Some(_) = finished.next() => break,
            Some(sig) = errors.next() => {
                if let Ok(args) = sig.args() {
                    if *args.code() != PK_ERROR_TRANSACTION_CANCELLED_NONFATAL {
                        return Err(format!("Update failed: {}", args.details()));
                    }
                } else {
                    return Err("Update failed".to_string());
                }
            }
            // PackageKit reports overall progress through the Percentage property;
            // re-read it whenever an item makes progress.
            Some(_) = progress.next() => {
                if let Ok(pct) = tx.percentage().await
                    && pct <= 100
                {
                    on_progress(pct as f32 / 100.0);
                }
            }
            else => break,
        }
    }
    Ok(())
}

/// Update every flatpak ref that has a pending update. The flatpak system helper
/// handles its own polkit authorization for system-wide installs.
async fn flatpak_update() -> Result<(), String> {
    let output = tokio::process::Command::new("flatpak")
        .args(["update", "--noninteractive"])
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("flatpak failed to start: {e}")),
    };

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("flatpak update: {}", stderr.trim()));
    }
    Ok(())
}

/// Install the requested updates in the background, streaming progress events.
///
/// System updates are installed first (weighted to the first half of the bar
/// when flatpaks follow), then flatpaks. Partial failures are collected and
/// reported together so one source failing doesn't hide the other's success.
pub fn install_updates(
    system_ids: Vec<String>,
    flatpak: bool,
) -> futures::channel::mpsc::UnboundedReceiver<InstallEvent> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    tokio::spawn(async move {
        let has_system = !system_ids.is_empty();
        // How much of the bar the system step fills before flatpak takes over.
        let sys_span = if has_system && flatpak { 0.5 } else { 1.0 };

        let mut errors = Vec::new();

        if has_system {
            match zbus::Connection::system().await {
                Ok(conn) => {
                    let tx_p = tx.clone();
                    let result = pk_update_packages(&conn, &system_ids, move |f| {
                        let _ = tx_p.unbounded_send(InstallEvent::Progress(f * sys_span));
                    })
                    .await;
                    if let Err(e) = result {
                        errors.push(e);
                    }
                }
                Err(e) => errors.push(format!("D-Bus connection failed: {e}")),
            }
        }

        if flatpak {
            let _ = tx.unbounded_send(InstallEvent::Progress(sys_span));
            if let Err(e) = flatpak_update().await {
                errors.push(e);
            }
            let _ = tx.unbounded_send(InstallEvent::Progress(1.0));
        }

        let result = if errors.is_empty() {
            Ok(())
        } else {
            Err(errors.join("\n"))
        };
        let _ = tx.unbounded_send(InstallEvent::Done(result));
    });
    rx
}

/// A stream that yields once each time PackageKit's set of updates changes
/// (e.g. after the COSMIC Store installs something). Lets the applet refresh
/// immediately instead of waiting for its periodic timer.
pub fn updates_changed_stream() -> futures::channel::mpsc::UnboundedReceiver<()> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    tokio::spawn(async move {
        let conn = match zbus::Connection::system().await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!("updates-changed watch: D-Bus connection failed: {e}");
                return;
            }
        };
        let pk = match PackageKitProxy::new(&conn).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("updates-changed watch: proxy failed: {e}");
                return;
            }
        };
        let mut signals = match pk.receive_updates_changed().await {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("updates-changed watch: subscribe failed: {e}");
                return;
            }
        };
        while signals.next().await.is_some() {
            if tx.unbounded_send(()).is_err() {
                break;
            }
        }
    });
    rx
}
