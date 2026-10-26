//! Sheets in the browser.
//!
//! Runs the same [`gridcraft_ui_egui::SheetApp`] as the desktop app through eframe's web runner
//! (wgpu: WebGPU where available, WebGL2 otherwise). Build with `trunk build --release` from
//! this directory (output in `dist/web`).
//!
//! Differences from the desktop app: no TCP control channel; Open uses the browser file picker
//! (bytes arrive asynchronously through `Services::inbox`); Save downloads the file; dropped
//! files are read asynchronously.
//!
//! URL query flags: `?webgl` forces WebGL2; `?sample=sales|budget|grades` opens a sample.
//! Framed by OneOS, it talks to the ONE parent origin the frame page declared (`host`): the host sends the
//! workbook to open and stores what Sheets saves as a new version, and the workbook is a live document
//! (`collab`) when the host says so. Preferences (theme, interface size, editing options…) are kept in
//! `localStorage` (`prefs`).
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

#[cfg(target_arch = "wasm32")]
mod collab;
#[cfg(target_arch = "wasm32")]
mod host;
#[cfg(any(target_arch = "wasm32", test))]
mod prefs;
#[cfg(target_arch = "wasm32")]
mod web;

#[cfg(target_arch = "wasm32")]
fn main() {
    web::start();
}

#[cfg(not(target_arch = "wasm32"))]
fn main() {
    eprintln!("gridcraft-web only runs in the browser: build it with `trunk build --release` in apps/gridcraft-web");
}
