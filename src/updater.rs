// SPDX-License-Identifier: GPL-3.0-only
//
// Self-update: check GitHub for newer releases of the applet and, when asked,
// download the prebuilt binary and relaunch into it.
//
// We shell out to `curl` (the same tool the installer relies on) so we don't
// pull in an HTTP/TLS stack just for an occasional version check.

use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;

/// GitHub `owner/repo` the releases are published under (matches `install.sh`).
const REPO: &str = "davidboulay/CosmicUpdate";
/// Release asset name for the binary (matches `install.sh`).
const BIN: &str = "cosmic-applet-updates";

/// The version this build was compiled as.
pub const CURRENT_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Ask GitHub for the latest release tag (e.g. "v0.2.0" or "0.2.0").
pub async fn latest_release() -> Result<String, String> {
    let url = format!("https://api.github.com/repos/{REPO}/releases/latest");
    let output = tokio::process::Command::new("curl")
        .args([
            "-fsSL",
            "-H",
            "Accept: application/vnd.github+json",
            "-A",
            "cosmic-applet-updates",
            &url,
        ])
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "curl is required to check for updates".to_string()
            } else {
                format!("curl failed to start: {e}")
            }
        })?;

    if !output.status.success() {
        return Err("Could not reach GitHub".to_string());
    }

    let body = String::from_utf8_lossy(&output.stdout);
    parse_tag_name(&body).ok_or_else(|| "No release found".to_string())
}

/// Extract the `tag_name` string from the releases JSON without a JSON parser.
fn parse_tag_name(json: &str) -> Option<String> {
    let key = json.find("\"tag_name\"")?;
    let after_key = &json[key + "\"tag_name\"".len()..];
    let colon = after_key.find(':')?;
    let after_colon = &after_key[colon + 1..];
    let open = after_colon.find('"')?;
    let value = &after_colon[open + 1..];
    let close = value.find('"')?;
    let tag = value[..close].trim().to_string();
    if tag.is_empty() { None } else { Some(tag) }
}

/// `1.2.3` (with optional leading `v` and trailing pre-release) → (1, 2, 3).
fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    // Drop any "-rc1"/"+build" suffix on the patch component.
    let core = v.split(['-', '+']).next().unwrap_or(v);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next().unwrap_or("0").parse().ok()?;
    let patch = parts.next().unwrap_or("0").parse().ok()?;
    Some((major, minor, patch))
}

/// Whether `latest` is a strictly newer version than `current`. Falls back to a
/// string comparison if either tag isn't parseable as semver.
pub fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_semver(latest), parse_semver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => latest.trim_start_matches('v') != current.trim_start_matches('v'),
    }
}

/// Download the prebuilt binary for `tag` and atomically replace this
/// executable. Returns the path that was replaced; pass it to [`relaunch`] to
/// run the new code.
///
/// The path is captured here *before* the swap on purpose: once the running
/// binary's file is unlinked, `std::env::current_exe()` starts returning a path
/// with a `" (deleted)"` suffix, which is useless for re-exec.
pub async fn self_update(tag: &str) -> Result<std::path::PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("cannot locate self: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "executable has no parent directory".to_string())?;

    // Download into the same directory so the final rename is atomic (same fs).
    let tmp = dir.join(format!(".{BIN}.update"));
    let url = format!("https://github.com/{REPO}/releases/download/{tag}/{BIN}");

    let output = tokio::process::Command::new("curl")
        .args(["-fsSL", &url, "-o"])
        .arg(&tmp)
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                "curl is required to download updates".to_string()
            } else {
                format!("curl failed to start: {e}")
            }
        })?;

    if !output.status.success() {
        let _ = std::fs::remove_file(&tmp);
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(format!("download failed: {}", stderr.trim()));
    }

    // Guard against a truncated/HTML error body being installed as the binary.
    match std::fs::metadata(&tmp) {
        Ok(m) if m.len() >= 4096 => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&tmp);
            return Err("downloaded file looks incomplete".to_string());
        }
        Err(e) => return Err(format!("download missing: {e}")),
    }

    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| format!("could not set permissions: {e}"))?;

    std::fs::rename(&tmp, &exe).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        format!("could not replace binary (is {} writable?): {e}", exe.display())
    })?;

    Ok(exe)
}

/// Replace the current process with a fresh exec of the (now-updated) binary at
/// `exe`, inheriting the same arguments and environment so it re-attaches to the
/// same panel slot. Only returns (an `Err`) if the exec itself fails.
///
/// `exe` must be the path captured *before* the binary was swapped — see
/// [`self_update`]; `std::env::current_exe()` is unreliable after the swap.
pub fn relaunch(exe: &std::path::Path) -> std::io::Error {
    // `exec` only returns on failure.
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_tag_name() {
        let json = r#"{"html_url":"https://x/tag/v9.9.9","tag_name":"v0.2.0","name":"r"}"#;
        assert_eq!(parse_tag_name(json).as_deref(), Some("v0.2.0"));
    }

    #[test]
    fn extracts_tag_name_with_spaces() {
        assert_eq!(
            parse_tag_name(r#"{ "tag_name" : "1.4.0" }"#).as_deref(),
            Some("1.4.0")
        );
    }

    #[test]
    fn no_tag_name() {
        assert_eq!(parse_tag_name(r#"{"message":"Not Found"}"#), None);
    }

    #[test]
    fn version_ordering() {
        assert!(is_newer("v0.2.0", "0.1.0"));
        assert!(is_newer("0.1.1", "v0.1.0"));
        assert!(is_newer("1.0.0", "0.9.9"));
        assert!(!is_newer("v0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // Pre-release suffix on the patch component is ignored for the core compare.
        assert!(!is_newer("v0.1.0-rc1", "0.1.0"));
    }

    #[test]
    fn unparseable_falls_back_to_string_inequality() {
        assert!(is_newer("weird", "0.1.0"));
        assert!(!is_newer("nightly", "nightly"));
    }
}
