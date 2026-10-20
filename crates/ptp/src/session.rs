//! What every tether brand offers the app, whatever protocol it speaks.
//!
//! The loupe and the commands talk to this. A new brand implements it. A new operating system
//! implements [`crate::Transport`] and does not appear here.

use crate::Result;

/// A still the camera handed over. The app imports `bytes` under `name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub name: String,
    pub bytes: Vec<u8>,
}

/// The four dials the tether bar shows. A brand that has no white balance reports none.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dial {
    Aperture,
    Shutter,
    Iso,
    WhiteBalance,
}

impl Dial {
    pub fn label(self) -> &'static str {
        match self {
            Dial::Aperture => "aperture",
            Dial::Shutter => "shutter",
            Dial::Iso => "ISO",
            Dial::WhiteBalance => "white balance",
        }
    }
}

/// A connected camera. Object-safe so the engine can hold whichever brand claimed the body.
pub trait CameraSession {
    fn model(&self) -> &str;
    fn text(&self, dial: Dial) -> Option<String>;
    fn has_white_balance(&self) -> bool;
    fn exposure_comp(&self) -> Option<String>;
    fn choices(&self, dial: Dial) -> Vec<(u64, String)>;
    fn magnified(&self) -> bool;
    fn hint(&self) -> &str;
    fn set_budget(&mut self, millis: u64);
    fn refresh(&mut self) -> Result<()>;
    fn set_dial(&mut self, dial: Dial, step: i32, absolute: Option<u64>) -> Result<u64>;
    fn magnify(&mut self, on: bool) -> Result<()>;
    fn nudge(&mut self, direction: &str) -> Result<()>;
    fn autofocus(&mut self) -> Result<()>;
    fn capture(&mut self) -> Result<Captured>;
    fn take_new_file(&mut self) -> Result<Option<Captured>>;
    fn live_jpeg(&mut self) -> Result<Option<Vec<u8>>>;
    fn close(&mut self) -> Result<()>;
}
