//! Which protocol claims a camera, and how a new one is added.
//!
//! The engine asks [`claims`] before it opens a device, then [`open`] with a [`crate::Transport`].
//! Adding a brand means a claim here and a [`crate::CameraSession`] somewhere else. Adding an
//! operating system means a new transport. Neither change belongs in the other.

use crate::link::Transport;
use crate::session::CameraSession;
use crate::sony::{self, SonySession};
use crate::{Error, Result};

/// True when a listed camera should be opened with a protocol this build knows.
pub fn claims(vendor: u16, name: &str) -> bool {
    sony::claims(vendor, name)
}

/// What to say when the list stayed empty.
pub fn waiting_hint() -> &'static str {
    "no camera in PC Remote. On the camera set USB Connection to PC Remote."
}

/// What to say when a camera was seen and no protocol claimed it.
pub fn refused(name: &str) -> String {
    format!("{name} isn't a camera this tether can drive")
}

/// Open the protocol that claimed `vendor` / `name`. The transport is the operating system's pipe.
pub fn open<T: Transport + 'static>(vendor: u16, name: &str, io: T) -> Result<Box<dyn CameraSession>> {
    if sony::claims(vendor, name) {
        return Ok(Box::new(SonySession::open(io)?));
    }
    Err(Error::new(refused(name)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::Dial;
    use crate::sim::Sim;

    const SONY_VENDOR: u16 = 0x054C;

    #[test]
    fn sony_is_claimed_and_another_vendor_is_not() {
        assert!(claims(SONY_VENDOR, "camera"));
        assert!(claims(0, "ILCE-7M3"));
        assert!(claims(0, "ZV-E10"));
        assert!(!claims(0x04A9, "Canon EOS R6"));
        assert!(refused("Canon EOS R6").contains("Canon EOS R6"));
    }

    #[test]
    fn the_registry_opens_a_sony_session() {
        let mut cam = open(SONY_VENDOR, "ILCE-7M3", Sim::a7iii()).unwrap();
        assert_eq!(cam.model(), "ILCE-7M3");
        cam.set_dial(Dial::Aperture, -1, None).unwrap();
        assert_eq!(cam.text(Dial::Aperture).as_deref(), Some("f/5.6"));
    }
}
