//! Picture Transfer Protocol containers, and the tether protocols that speak them.
//!
//! Containers follow PIMA 15740 (the USB still-image class). [`Transport`] is the operating
//! system's pipe. [`CameraSession`] is what a brand offers the app. Sony Camera Control PTP 2
//! (the A7 III's PC Remote profile) is the first brand. PTP 3 is a different profile and is not
//! implemented here. This crate does not open USB devices.
#![forbid(unsafe_code)]
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

mod brand;
mod info;
mod link;
mod packet;
mod props;
mod read;
mod session;
mod sim;
mod sony;

pub use brand::{claims, open, refused, waiting_hint};
pub use info::{DeviceInfo, device_info, object_filename, parse_device_info};
pub use link::{Reply, Transport};
pub use packet::{Packet, decode, encode_command};
pub use props::{Prop, encode_props, encode_value, parse_props, step_enum};
pub use session::{CameraSession, Captured, Dial};
pub use sim::{Sim, TINY_JPEG};
pub use sony::{CAPTURED_OBJECT, LIVE_VIEW_OBJECT, SonySession, control_click, format_aperture, format_iso, format_shutter, jpeg_payload};

/// A protocol failure the caller can show. Camera bytes are treated as hostile: a short buffer,
/// a length that does not match, or a count that would run off the end is this error, never a panic.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error {
    message: String,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into() }
    }

    pub fn message(&self) -> &str {
        &self.message
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

pub type Result<T> = std::result::Result<T, Error>;
