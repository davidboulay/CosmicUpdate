// SPDX-License-Identifier: GPL-3.0-only

pub mod backend;
mod window;

pub use window::Window;

pub fn run() -> cosmic::iced::Result {
    cosmic::applet::run::<Window>(())
}
