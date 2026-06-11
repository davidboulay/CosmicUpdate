// SPDX-License-Identifier: GPL-3.0-only

use std::sync::LazyLock;
use std::time::Duration;

use cosmic::{
    Application, Element, Task, app,
    applet::{cosmic_panel_config::PanelAnchor, menu_button, padded_control},
    cctk::sctk::reexports::calloop,
    cosmic_theme::Spacing,
    iced::{
        Alignment, Length, Subscription, stream,
        futures::{SinkExt, StreamExt, channel::mpsc},
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
        widget::{column, row},
        window,
    },
    theme,
    widget::{Id, autosize, button, container, divider, icon, scrollable, settings, text, toggler},
};

use cosmic::applet::token::subscription::{
    TokenRequest, TokenUpdate, activation_token_subscription,
};
use cosmic::cosmic_config::{self, ConfigGet, ConfigSet};

use crate::backend::{self, UpdateItem, UpdateReport};
use crate::updater;

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("updates-autosize-main"));

/// Bump if the persisted config layout ever changes incompatibly.
const CONFIG_VERSION: u64 = 1;
/// Config key for the "automatically update the applet" toggle.
const AUTO_UPDATE_KEY: &str = "auto-update";

/// Where the applet's own version sits relative to the latest GitHub release.
#[derive(Debug, Clone)]
enum ReleaseStatus {
    /// No check has completed yet.
    Unknown,
    Checking,
    UpToDate,
    /// A newer release exists; holds its tag (e.g. "v0.2.0").
    Available(String),
    Error(String),
}

// Status badges with the colour baked in: a dark-yellow seal with an up-arrow
// when updates are pending, and a green seal with a checkmark when up to date.
const ICON_AVAILABLE_SVG: &[u8] = include_bytes!("../icons/updates-available.svg");
const ICON_UP_TO_DATE_SVG: &[u8] = include_bytes!("../icons/updates-ok.svg");

pub struct Window {
    core: app::Core,
    popup: Option<window::Id>,
    token_tx: Option<calloop::channel::Sender<TokenRequest>>,
    checking: bool,
    report: UpdateReport,
    last_checked: Option<String>,
    /// Set while an in-place install is running; `install_progress` is the
    /// overall fraction (None until the first progress event arrives).
    installing: bool,
    install_progress: Option<f32>,
    install_error: Option<String>,
    /// Persisted settings handle (None if the config backend is unavailable).
    config: Option<cosmic_config::Config>,
    /// Whether to auto-install newer releases of the applet itself.
    auto_update: bool,
    /// True while the settings panel is showing instead of the updates list.
    show_settings: bool,
    /// Latest-release status for the applet's own version.
    release: ReleaseStatus,
    /// True while a self-update download is in progress.
    self_updating: bool,
}

#[derive(Debug, Clone)]
pub enum Message {
    TogglePopup,
    CloseRequested(window::Id),
    /// Run a check. `refresh` downloads new package metadata (polkit prompt);
    /// when false we only read the already-cached state.
    Check {
        refresh: bool,
    },
    Checked(UpdateReport),
    /// Install all pending updates in place (PackageKit + flatpak).
    Install,
    InstallProgress(f32),
    Installed(Result<(), String>),
    OpenStore,
    /// Show/hide the settings panel.
    ToggleSettings,
    /// Check GitHub for a newer release of the applet.
    CheckRelease,
    ReleaseChecked(Result<String, String>),
    SetAutoUpdate(bool),
    /// Download and install the given release tag of the applet, then relaunch.
    SelfUpdate(String),
    /// Ok carries the path of the replaced binary to relaunch.
    SelfUpdated(Result<std::path::PathBuf, String>),
    Token(TokenUpdate),
}

impl Window {
    fn run_check(&mut self, refresh: bool) -> app::Task<Message> {
        if self.checking {
            return Task::none();
        }
        self.checking = true;
        cosmic::task::future(async move {
            let report = backend::check_for_updates(refresh).await;
            cosmic::Action::App(Message::Checked(report))
        })
    }

    /// Query GitHub for the latest release tag in the background.
    fn check_release() -> app::Task<Message> {
        cosmic::task::future(async move {
            cosmic::Action::App(Message::ReleaseChecked(updater::latest_release().await))
        })
    }

    /// Download and install the given release tag in the background.
    fn do_self_update(tag: String) -> app::Task<Message> {
        cosmic::task::future(async move {
            cosmic::Action::App(Message::SelfUpdated(updater::self_update(&tag).await))
        })
    }

    /// Launch a command using an activation token so it is focused correctly.
    fn spawn_with_token(&self, exec: &str) {
        if let Some(tx) = self.token_tx.as_ref() {
            let _ = tx.send(TokenRequest {
                app_id: Self::APP_ID.to_string(),
                exec: exec.to_string(),
            });
        } else {
            tracing::error!("activation token channel unavailable");
        }
    }

    /// The coloured status badge, sized for the given pixel size.
    fn status_icon(&self, size: u16) -> cosmic::widget::icon::Icon {
        let bytes: &'static [u8] = if self.report.total() > 0 {
            ICON_AVAILABLE_SVG
        } else {
            ICON_UP_TO_DATE_SVG
        };
        icon::from_svg_bytes(bytes).icon().size(size)
    }

    fn section(&self, title: &str, items: &[UpdateItem]) -> Option<Element<'_, Message>> {
        if items.is_empty() {
            return None;
        }
        let Spacing { space_xxs, .. } = theme::active().cosmic().spacing;

        let mut col = column![text::heading(format!("{title} ({})", items.len()))].spacing(space_xxs);
        for item in items {
            let mut info = column![text::body(item.name.clone())];
            let secondary = if item.summary.is_empty() {
                item.version.clone()
            } else if item.version.is_empty() {
                item.summary.clone()
            } else {
                format!("{} — {}", item.version, item.summary)
            };
            if !secondary.is_empty() {
                info = info.push(text::caption(secondary));
            }
            col = col.push(padded_control(info).padding([space_xxs, 0]));
        }
        Some(col.into())
    }
}

impl cosmic::Application for Window {
    type Message = Message;
    type Executor = cosmic::SingleThreadExecutor;
    type Flags = ();
    const APP_ID: &'static str = "com.github.davidboulay.CosmicAppletUpdates";

    fn init(core: app::Core, _flags: Self::Flags) -> (Self, app::Task<Self::Message>) {
        let config = cosmic_config::Config::new(Self::APP_ID, CONFIG_VERSION).ok();
        let auto_update = config
            .as_ref()
            .and_then(|c| c.get::<bool>(AUTO_UPDATE_KEY).ok())
            .unwrap_or(false);

        let mut window = Self {
            core,
            popup: None,
            token_tx: None,
            checking: false,
            report: UpdateReport::default(),
            last_checked: None,
            installing: false,
            install_progress: None,
            install_error: None,
            config,
            auto_update,
            show_settings: false,
            release: ReleaseStatus::Unknown,
            self_updating: false,
        };
        // Populate counts on startup from cached data (no polkit prompt), and
        // learn whether a newer applet release exists (auto-updating if enabled).
        let task = Task::batch([window.run_check(false), Self::check_release()]);
        (window, task)
    }

    fn core(&self) -> &app::Core {
        &self.core
    }

    fn core_mut(&mut self) -> &mut app::Core {
        &mut self.core
    }

    fn style(&self) -> Option<cosmic::iced::theme::Style> {
        Some(cosmic::applet::style())
    }

    fn subscription(&self) -> Subscription<Message> {
        // Re-check periodically so the badge stays current while the applet
        // runs. The system already refreshes the package cache on its own
        // (apt-daily.timer), so this only re-reads the cached state — no polkit
        // prompt — and re-queries flatpak.
        fn periodic_check() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(60 * 60); // hourly
            Subscription::run_with("updates-periodic-check", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // The first tick is immediate; skip it since init() already
                    // ran a check on startup.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::Check { refresh: false }).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        // React the moment PackageKit's update set changes — e.g. right after
        // the COSMIC Store installs something, or after our own install — so the
        // badge clears without waiting for the periodic timer.
        fn updates_changed() -> Subscription<Message> {
            Subscription::run_with("packagekit-updates-changed", |_| {
                backend::updates_changed_stream().map(|()| Message::Check { refresh: false })
            })
        }

        // Periodically check whether a newer applet release is out (and auto-
        // update if the user enabled it). Far less frequent than the package
        // check since releases are rare.
        fn periodic_release_check() -> Subscription<Message> {
            const INTERVAL: Duration = Duration::from_secs(6 * 60 * 60); // 6 hours
            Subscription::run_with("updates-release-check", |_| {
                stream::channel(1, |mut output: mpsc::Sender<Message>| async move {
                    let mut timer = tokio::time::interval(INTERVAL);
                    timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                    // First tick is immediate; skip it since init() already checked.
                    timer.tick().await;
                    loop {
                        timer.tick().await;
                        if output.send(Message::CheckRelease).await.is_err() {
                            break;
                        }
                    }
                })
            })
        }

        Subscription::batch([
            activation_token_subscription(0).map(Message::Token),
            periodic_check(),
            updates_changed(),
            periodic_release_check(),
        ])
    }

    fn update(&mut self, message: Self::Message) -> app::Task<Self::Message> {
        match message {
            Message::TogglePopup => {
                if let Some(p) = self.popup.take() {
                    destroy_popup(p)
                } else {
                    let new_id = window::Id::unique();
                    self.popup = Some(new_id);
                    let popup_settings = self.core.applet.get_popup_settings(
                        self.core.main_window_id().unwrap(),
                        new_id,
                        None,
                        None,
                        None,
                    );
                    get_popup(popup_settings)
                }
            }
            Message::CloseRequested(id) => {
                if Some(id) == self.popup {
                    self.popup = None;
                }
                Task::none()
            }
            Message::Check { refresh } => self.run_check(refresh),
            Message::Checked(report) => {
                self.checking = false;
                self.report = report;
                self.last_checked = Some(
                    jiff::Zoned::now()
                        .strftime("%H:%M")
                        .to_string(),
                );
                Task::none()
            }
            Message::Install => {
                if self.installing || self.checking || self.report.total() == 0 {
                    return Task::none();
                }
                self.installing = true;
                self.install_progress = None;
                self.install_error = None;
                let system_ids: Vec<String> = self
                    .report
                    .system
                    .iter()
                    .map(|i| i.package_id.clone())
                    .collect();
                let flatpak = !self.report.flatpak.is_empty();
                cosmic::task::stream(backend::install_updates(system_ids, flatpak).map(|ev| {
                    cosmic::Action::App(match ev {
                        backend::InstallEvent::Progress(p) => Message::InstallProgress(p),
                        backend::InstallEvent::Done(r) => Message::Installed(r),
                    })
                }))
            }
            Message::InstallProgress(p) => {
                self.install_progress = Some(p);
                Task::none()
            }
            Message::Installed(result) => {
                self.installing = false;
                self.install_progress = None;
                self.install_error = result.err();
                // Re-read state so the counts/badge drop immediately. The
                // PackageKit UpdatesChanged signal also fires, but this is
                // instant and also picks up the flatpak side.
                self.run_check(false)
            }
            Message::OpenStore => {
                self.spawn_with_token("cosmic-store");
                Task::none()
            }
            Message::ToggleSettings => {
                self.show_settings = !self.show_settings;
                // Refresh the release status when opening the panel.
                if self.show_settings && !matches!(self.release, ReleaseStatus::Checking) {
                    self.release = ReleaseStatus::Checking;
                    return Self::check_release();
                }
                Task::none()
            }
            Message::CheckRelease => {
                if matches!(self.release, ReleaseStatus::Checking) || self.self_updating {
                    return Task::none();
                }
                self.release = ReleaseStatus::Checking;
                Self::check_release()
            }
            Message::ReleaseChecked(Ok(tag)) => {
                if updater::is_newer(&tag, updater::CURRENT_VERSION) {
                    self.release = ReleaseStatus::Available(tag.clone());
                    // Auto-install the new version if the user opted in.
                    if self.auto_update && !self.self_updating {
                        self.self_updating = true;
                        return Self::do_self_update(tag);
                    }
                } else {
                    self.release = ReleaseStatus::UpToDate;
                }
                Task::none()
            }
            Message::ReleaseChecked(Err(e)) => {
                self.release = ReleaseStatus::Error(e);
                Task::none()
            }
            Message::SetAutoUpdate(on) => {
                self.auto_update = on;
                if let Some(cfg) = &self.config
                    && let Err(e) = cfg.set(AUTO_UPDATE_KEY, on)
                {
                    tracing::warn!("could not persist auto-update setting: {e}");
                }
                // If switching on while an update is already pending, apply it now.
                if on
                    && !self.self_updating
                    && let ReleaseStatus::Available(tag) = &self.release
                {
                    let tag = tag.clone();
                    self.self_updating = true;
                    return Self::do_self_update(tag);
                }
                Task::none()
            }
            Message::SelfUpdate(tag) => {
                if self.self_updating {
                    return Task::none();
                }
                self.self_updating = true;
                Self::do_self_update(tag)
            }
            Message::SelfUpdated(Ok(exe)) => {
                // The binary has been replaced; exec into the new version. This
                // only returns if the exec itself fails.
                let err = updater::relaunch(&exe);
                tracing::error!("relaunch after self-update failed: {err}");
                self.self_updating = false;
                self.release =
                    ReleaseStatus::Error(format!("Updated, but relaunch failed: {err}"));
                Task::none()
            }
            Message::SelfUpdated(Err(e)) => {
                self.self_updating = false;
                self.release = ReleaseStatus::Error(e);
                Task::none()
            }
            Message::Token(u) => {
                match u {
                    TokenUpdate::Init(tx) => self.token_tx = Some(tx),
                    TokenUpdate::Finished => self.token_tx = None,
                    TokenUpdate::ActivationToken { token, exec, .. } => {
                        let mut cmd = std::process::Command::new("sh");
                        cmd.arg("-c").arg(&exec);
                        if let Some(token) = token {
                            cmd.env("XDG_ACTIVATION_TOKEN", &token);
                            cmd.env("DESKTOP_STARTUP_ID", &token);
                        }
                        tokio::spawn(cosmic::process::spawn(cmd));
                    }
                }
                Task::none()
            }
        }
    }

    fn view(&self) -> Element<'_, Message> {
        let horizontal = matches!(
            self.core.applet.anchor,
            PanelAnchor::Top | PanelAnchor::Bottom
        );

        let total = self.report.total();
        let suggested = self.core.applet.suggested_size(true);
        let icon = self.status_icon(suggested.0);

        let content: Element<'_, Message> = if total > 0 {
            let count = self.core.applet.text(total.to_string());
            if horizontal {
                row![icon, count]
                    .spacing(2)
                    .align_y(Alignment::Center)
                    .into()
            } else {
                column![icon, count]
                    .spacing(2)
                    .align_x(Alignment::Center)
                    .into()
            }
        } else {
            icon.into()
        };

        // Match stock applets: give the button a fixed cross-axis size and
        // centre the content, so the hover highlight covers the full panel
        // height (rather than just the icon). Use the regular — not the larger
        // "shrinkable" — padding on the long axis to keep the sides compact.
        let (_pad_shrinkable, pad_regular) = self.core.applet.suggested_padding(true);
        let button = if horizontal {
            button::custom(container(content).center_y(Length::Fill))
                .height(Length::Fixed((suggested.1 + 2 * pad_regular) as f32))
                .padding([0, pad_regular])
        } else {
            button::custom(container(content).center_x(Length::Fill))
                .width(Length::Fixed((suggested.0 + 2 * pad_regular) as f32))
                .padding([pad_regular, 0])
        }
        .on_press_down(Message::TogglePopup)
        .class(cosmic::theme::Button::AppletIcon);

        autosize::autosize(button, AUTOSIZE_MAIN_ID.clone()).into()
    }

    fn view_window(&self, _id: window::Id) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        if self.show_settings {
            return self.settings_view();
        }

        let total = self.report.total();

        // Header / summary line.
        let summary = if self.checking {
            text::body("Checking for updates…")
        } else if total == 0 {
            text::body("Your system is up to date")
        } else {
            text::body(format!(
                "{total} update{} available",
                if total == 1 { "" } else { "s" }
            ))
        };

        let header = padded_control(
            row![
                self.status_icon(28),
                column![text::title4("Updates"), summary]
                    .spacing(2)
                    .width(Length::Fill),
                button::icon(icon::from_name("emblem-system-symbolic").symbolic(true))
                    .on_press(Message::ToggleSettings),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center),
        );

        // Check button.
        let check_label = if self.checking {
            "Checking…"
        } else {
            "Check for updates"
        };
        let check_button = button::standard(check_label)
            .leading_icon(icon::from_name("view-refresh-symbolic").symbolic(true))
            .on_press_maybe(
                (!self.checking && !self.installing).then_some(Message::Check { refresh: true }),
            )
            .width(Length::Fill);

        let mut content = column![header, padded_control(check_button)].spacing(space_xxs);

        // Errors, if any.
        let mut error_lines: Vec<String> = self.report.errors.clone();
        if let Some(e) = &self.install_error {
            error_lines.push(e.clone());
        }
        for err in &error_lines {
            content = content.push(
                padded_control(
                    text::caption(err.clone()).class(cosmic::theme::Text::Color(
                        theme::active().cosmic().destructive_color().into(),
                    )),
                )
                .padding([space_xxs, space_m]),
            );
        }

        // Update sections.
        let mut sections = column![].spacing(space_s);
        let mut any = false;
        if let Some(s) = self.section("System", &self.report.system) {
            sections = sections.push(s);
            any = true;
        }
        if let Some(s) = self.section("Flatpak", &self.report.flatpak) {
            sections = sections.push(s);
            any = true;
        }

        if any {
            content = content.push(padded_control(divider::horizontal::default()));
            content = content.push(
                container(scrollable(padded_control(sections)).height(Length::Shrink))
                    .max_height(320.0),
            );
            content = content.push(padded_control(divider::horizontal::default()));

            if self.installing {
                // Show progress in place of the action buttons while installing.
                let bar: Element<'_, Message> = match self.install_progress {
                    Some(p) => cosmic::widget::progress_bar::determinate_linear(p)
                        .width(Length::Fill)
                        .into(),
                    None => cosmic::widget::progress_bar::indeterminate_linear()
                        .width(Length::Fill)
                        .into(),
                };
                let label = match self.install_progress {
                    Some(p) => format!("Installing updates… {:.0}%", p * 100.0),
                    None => "Installing updates…".to_string(),
                };
                content = content.push(padded_control(
                    column![text::body(label), bar].spacing(space_xxs),
                ));
            } else {
                // Primary action: install everything in place.
                let install_button = button::suggested(format!(
                    "Install {total} update{}",
                    if total == 1 { "" } else { "s" }
                ))
                .leading_icon(
                    icon::from_name("system-software-install-symbolic").symbolic(true),
                )
                .on_press(Message::Install)
                .width(Length::Fill);
                content = content.push(padded_control(install_button));

                // Secondary: open the full COSMIC Store to browse/review.
                content = content.push(
                    menu_button(row![
                        icon::from_name("system-software-install-symbolic")
                            .symbolic(true)
                            .size(16),
                        text::body("Open in COSMIC Store"),
                    ]
                    .spacing(space_s)
                    .align_y(Alignment::Center))
                    .on_press(Message::OpenStore),
                );
            }
        }

        if let Some(checked) = &self.last_checked {
            content = content.push(
                padded_control(text::caption(format!("Last checked at {checked}")))
                    .padding([space_xxs, space_m]),
            );
        }

        self.core
            .applet
            .popup_container(
                container(content.spacing(space_xxs).padding([space_s, 0]))
                    .width(Length::Fixed(360.0)),
            )
            .into()
    }

    fn on_close_requested(&self, id: window::Id) -> Option<Message> {
        Some(Message::CloseRequested(id))
    }
}

impl Window {
    /// The settings panel: version, applet self-update check, and the
    /// auto-update toggle.
    fn settings_view(&self) -> Element<'_, Message> {
        let Spacing {
            space_xxs,
            space_s,
            space_m,
            ..
        } = theme::active().cosmic().spacing;

        // Header with a back button.
        let header = padded_control(
            row![
                button::icon(icon::from_name("go-previous-symbolic").symbolic(true))
                    .on_press(Message::ToggleSettings),
                text::title4("Settings"),
            ]
            .spacing(space_s)
            .align_y(Alignment::Center),
        );

        // Current applet version.
        let version_row = settings::item("Version", text::body(updater::CURRENT_VERSION));

        // Manual "check GitHub" button (disabled mid-check / mid-update).
        // Labelled distinctly from the main popup's package-update check so the
        // two aren't mistaken for each other.
        let busy = matches!(self.release, ReleaseStatus::Checking) || self.self_updating;
        let check_button = button::standard("Check for new version")
            .leading_icon(icon::from_name("software-update-available-symbolic").symbolic(true))
            .on_press_maybe((!busy).then_some(Message::CheckRelease))
            .width(Length::Fill);

        // Status line + (when an update is available) an "Update now" action.
        let (status, update_now): (String, Option<Element<'_, Message>>) = match &self.release {
            ReleaseStatus::Unknown => ("Not checked yet".to_string(), None),
            ReleaseStatus::Checking => ("Checking GitHub…".to_string(), None),
            ReleaseStatus::UpToDate => {
                (format!("Up to date (v{})", updater::CURRENT_VERSION), None)
            }
            ReleaseStatus::Available(tag) => (
                format!("{tag} is available"),
                (!self.self_updating).then(|| {
                    button::suggested("Update now")
                        .on_press(Message::SelfUpdate(tag.clone()))
                        .into()
                }),
            ),
            ReleaseStatus::Error(e) => (format!("Check failed: {e}"), None),
        };

        let status_class = if matches!(self.release, ReleaseStatus::Error(_)) {
            cosmic::theme::Text::Color(theme::active().cosmic().destructive_color().into())
        } else {
            cosmic::theme::Text::Default
        };
        let mut status_col = column![text::caption(status).class(status_class)].spacing(space_xxs);
        if self.self_updating {
            status_col = status_col.push(text::caption("Downloading and installing…"));
        }
        if let Some(action) = update_now {
            status_col = status_col.push(action);
        }

        // Auto-update toggle.
        let auto_row = settings::item(
            "Automatically update the applet",
            toggler(self.auto_update).on_toggle(Message::SetAutoUpdate),
        );

        // Spell out that this panel is about the applet itself, not the system
        // and app updates the main view tracks.
        let subtitle = padded_control(text::caption(
            "Updates for the applet itself — separate from the system & app updates it lists.",
        ))
        .padding([0, space_m]);

        let content = column![
            header,
            subtitle,
            padded_control(version_row),
            padded_control(check_button),
            padded_control(status_col).padding([space_xxs, space_m]),
            padded_control(divider::horizontal::default()),
            padded_control(auto_row),
        ]
        .spacing(space_xxs);

        self.core
            .applet
            .popup_container(
                container(content.spacing(space_xxs).padding([space_s, 0]))
                    .width(Length::Fixed(360.0)),
            )
            .into()
    }
}
