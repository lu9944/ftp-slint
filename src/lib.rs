#![allow(
    clippy::missing_errors_doc,
    clippy::missing_panics_doc,
    clippy::must_use_candidate,
    clippy::module_name_repetitions
)]

pub mod app;
pub mod core;
pub mod ftp;
pub mod store;

pub mod bridge {
    slint::include_modules!();
}
