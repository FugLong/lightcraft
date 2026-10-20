//! The byte pipe a tether protocol speaks through.
//!
//! One transaction is a PTP command, an optional data phase out, and the camera's response plus
//! any data phase back. macOS Image Capture, a Linux USB claim, and Windows Portable Devices each
//! implement this. None of them know which brand is on the other end.

use crate::Result;

/// One PTP transaction. `outgoing` is the data phase the initiator sends (a property write).
/// The reply's `data` is the data phase the camera sends. Connect phases do not send data.
pub trait Transport {
    fn transact(&mut self, code: u16, params: &[u32], outgoing: Option<&[u8]>) -> Result<Reply>;

    /// How long the next command may wait, in milliseconds. The default ignores it.
    fn set_budget(&mut self, _millis: u64) {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub code: u16,
    pub params: Vec<u32>,
    pub data: Vec<u8>,
}
