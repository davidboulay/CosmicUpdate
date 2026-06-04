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
        futures::{SinkExt, channel::mpsc},
        platform_specific::shell::wayland::commands::popup::{destroy_popup, get_popup},
        widget::{column, row},
        window,
    },
    theme,
    widget::{Id, autosize, button, container, divider, icon, scrollable, text},
};

use cosmic::applet::token::subscription::{
    TokenRequest, TokenUpdate, activation_token_subscription,
};

use crate::backend::{self, UpdateItem, UpdateReport};

static AUTOSIZE_MAIN_ID: LazyLock<Id> = LazyLock::new(|| Id::new("updates-autosize-main"));

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
    OpenStore,
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
        let mut window = Self {
            core,
            popup: None,
            token_tx: None,
            checking: false,
            report: UpdateReport::default(),
            last_checked: None,
        };
        // Populate counts on startup from cached data (no polkit prompt).
        let task = window.run_check(false);
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

        Subscription::batch([
            activation_token_subscription(0).map(Message::Token),
            periodic_check(),
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
            Message::OpenStore => {
                self.spawn_with_token("cosmic-store");
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
                column![text::title4("Updates"), summary].spacing(2),
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
            .on_press_maybe((!self.checking).then_some(Message::Check { refresh: true }))
            .width(Length::Fill);

        let mut content = column![header, padded_control(check_button)].spacing(space_xxs);

        // Errors, if any.
        for err in &self.report.errors {
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
            content = content.push(
                menu_button(row![
                    icon::from_name("system-software-install-symbolic")
                        .symbolic(true)
                        .size(16),
                    text::body("Install updates in COSMIC Store"),
                ]
                .spacing(space_s)
                .align_y(Alignment::Center))
                .on_press(Message::OpenStore),
            );
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
