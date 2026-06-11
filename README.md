# COSMIC Updates Applet

A panel applet for the **COSMIC** desktop (Pop!_OS) that shows how many app and
system updates are pending and lets you check for new ones with a button. It can
be added to the COSMIC panel or dock like any built-in applet.

## Screenshots

| Updates available | Up to date |
|:---:|:---:|
| ![Updates available](screenshots/updates-available.png) | ![Up to date](screenshots/up-to-date.png) |

## What it does

- **Panel button** shows a badge with the total number of pending updates, or a
  ✓ icon when everything is up to date.
- **Popup** (click the button) lists the pending updates, split into:
  - **System** packages — queried via **PackageKit** over D-Bus, the same
    backend the COSMIC Store uses (apt on Pop!_OS).
  - **Flatpak** apps — queried via the `flatpak` CLI.
- **"Check for updates"** button refreshes the package metadata and re-scans.
  - Flatpak always queries its remotes live.
  - System refresh goes through PackageKit; if your polkit policy requires it,
    the desktop's authentication agent will prompt. Per-repo warnings (e.g. a
    missing GPG key) are logged, not shown, so they don't hide real updates.
- **"Install N updates"** applies everything *in place* — no need to open the
  Store. System packages go through PackageKit's `UpdatePackages` (the same
  transaction the COSMIC Store uses; the polkit agent prompts for
  authorization), flatpaks through `flatpak update`. A progress bar tracks the
  install and the badge clears as soon as it finishes.
- **"Open in COSMIC Store"** is still there if you'd rather review updates in
  the full Store UI.
- **Settings** (the ⚙ button in the popup header) shows the applet's own
  version and lets it keep itself up to date:
  - **"Check for new version"** queries the latest [GitHub release](https://github.com/davidboulay/CosmicUpdate/releases)
    and tells you whether a newer version is out. (This is distinct from the
    main popup's **"Check for updates"**, which scans for system & app updates.)
  - **"Automatically update the applet"** — when enabled, a newer release is
    downloaded (the prebuilt binary, same as `install.sh`), installed over the
    running binary, and the applet relaunches into it. The check runs on startup
    and every few hours. The setting is persisted via `cosmic-config`.

The applet also listens for PackageKit's `UpdatesChanged` signal, so when
updates are installed elsewhere (e.g. the COSMIC Store) the badge refreshes
**immediately** rather than waiting for the next periodic re-scan.

On startup the applet reads the *cached* update state (no prompt) so the badge
populates immediately.

## Install

One command — no checkout required:

```sh
curl -fsSL https://raw.githubusercontent.com/davidboulay/CosmicUpdate/main/install.sh | bash
```

The installer downloads a **prebuilt binary** from the latest [release](https://github.com/davidboulay/CosmicUpdate/releases)
(x86_64, no Rust needed). On other architectures, or if no release is
available, it automatically **builds from source** instead (requires a Rust
toolchain — edition 2024 / Rust ≥ 1.85).

It installs to `~/.local`:

- binary → `~/.local/bin/cosmic-applet-updates`
- desktop entry → `~/.local/share/applications/com.github.davidboulay.CosmicAppletUpdates.desktop`

Install system-wide with `PREFIX=/usr/local sudo -E bash install.sh`, or from a
checkout with `./install.sh`.

### Add it to the panel

**Settings → Desktop → Panel** (or **Dock**) **→ Add Applet → Updates**.

If it doesn't show up right away, restart the panel:

```sh
cosmic-panel --replace &
```

…or log out and back in.

## Verify the backend without the GUI

```sh
cargo run --example check            # read cached state (no prompt)
cargo run --example check -- refresh # refresh metadata first
```

## Project layout

| File | Purpose |
|------|---------|
| `src/main.rs` | binary entry point |
| `src/lib.rs` | `run()` — launches the applet |
| `src/window.rs` | the applet UI (panel button + popup) |
| `src/backend.rs` | update discovery, in-place install, and the `UpdatesChanged` watch (PackageKit + flatpak) |
| `src/updater.rs` | applet self-update: GitHub release check + download/replace/relaunch |
| `examples/check.rs` | headless backend smoke test |
| `data/*.desktop` | COSMIC applet registration |

## Notes

- `libcosmic` is pinned to the revision matching the COSMIC release shipped with
  Pop!_OS 24.04 LTS. Bump the `rev` in `Cargo.toml` if you track a newer COSMIC.
- The app ID is `com.github.davidboulay.CosmicAppletUpdates`; the desktop file
  name must match it.
