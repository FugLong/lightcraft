//! An in-memory A7 III. Sessions and the engine tests drive it; nothing here touches USB.

use crate::info::{self, device_info};
use crate::props::{self, Prop};
use crate::sony::{
    CAPTURED_OBJECT, LIVE_VIEW_OBJECT, PROP_FNUMBER, PROP_ISO, PROP_LIVE_VIEW, PROP_OBJECT_IN_MEMORY, PROP_SAVE_DEST, PROP_SHUTTER, Reply, Transport,
};
use crate::{Error, Result};

const OPEN_SESSION: u16 = 0x1002;
const GET_DEVICE_INFO: u16 = 0x1001;
const CLOSE_SESSION: u16 = 0x1003;
const GET_OBJECT_INFO: u16 = 0x1008;
const GET_OBJECT: u16 = 0x1009;
const SDIO_CONNECT: u16 = 0x9201;
const SDIO_GET_EXT_DEVICE_INFO: u16 = 0x9202;
const SDIO_SET_EXT_DEVICE_PROP_VALUE: u16 = 0x9205;
const SDIO_CONTROL_DEVICE: u16 = 0x9207;
const SDIO_GET_ALL: u16 = 0x9209;
const OK: u16 = 0x2001;

/// A 1×1 JPEG, so a live-view frame can be decoded without a camera.
pub const TINY_JPEG: &[u8] = &[
    0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43,
    0x00, 0x08, 0x06, 0x06, 0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D, 0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12,
    0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D, 0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28, 0x37, 0x29,
    0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32, 0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01,
    0x00, 0x01, 0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x1F, 0x00, 0x00, 0x01, 0x05, 0x01, 0x01, 0x01, 0x01, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0A, 0x0B, 0xFF, 0xC4, 0x00, 0xB5, 0x10, 0x00, 0x02, 0x01, 0x03,
    0x03, 0x02, 0x04, 0x03, 0x05, 0x05, 0x04, 0x04, 0x00, 0x00, 0x01, 0x7D, 0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06,
    0x13, 0x51, 0x61, 0x07, 0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xA1, 0x08, 0x23, 0x42, 0xB1, 0xC1, 0x15, 0x52, 0xD1, 0xF0, 0x24, 0x33, 0x62, 0x72,
    0x82, 0x09, 0x0A, 0x16, 0x17, 0x18, 0x19, 0x1A, 0x25, 0x26, 0x27, 0x28, 0x29, 0x2A, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3A, 0x43, 0x44, 0x45,
    0x46, 0x47, 0x48, 0x49, 0x4A, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5A, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69, 0x6A, 0x73, 0x74, 0x75,
    0x76, 0x77, 0x78, 0x79, 0x7A, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89, 0x8A, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9A, 0xA2, 0xA3,
    0xA4, 0xA5, 0xA6, 0xA7, 0xA8, 0xA9, 0xAA, 0xB2, 0xB3, 0xB4, 0xB5, 0xB6, 0xB7, 0xB8, 0xB9, 0xBA, 0xC2, 0xC3, 0xC4, 0xC5, 0xC6, 0xC7, 0xC8, 0xC9,
    0xCA, 0xD2, 0xD3, 0xD4, 0xD5, 0xD6, 0xD7, 0xD8, 0xD9, 0xDA, 0xE1, 0xE2, 0xE3, 0xE4, 0xE5, 0xE6, 0xE7, 0xE8, 0xE9, 0xEA, 0xF1, 0xF2, 0xF3, 0xF4,
    0xF5, 0xF6, 0xF7, 0xF8, 0xF9, 0xFA, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00, 0x3F, 0x00, 0xFB, 0xFF, 0xD9,
];

pub struct Sim {
    props: Vec<Prop>,
    handshake: Vec<u8>,
    pub shots: u32,
    /// Set when a property write is refused (the save-destination warning).
    pub refuse_writes: bool,
    /// Replaces the captured file bytes. The name stays `DSC00001.ARW`.
    pub capture_payload: Option<Vec<u8>>,
}

impl Sim {
    pub fn a7iii() -> Self {
        Self {
            props: vec![
                Prop::u16(PROP_FNUMBER, 800, &[280, 400, 560, 800], true),
                Prop::u32(PROP_SHUTTER, 0x0001_007D, &[0x0001_003C, 0x0001_007D, 0x0001_00FA], true),
                Prop::u32(PROP_ISO, 100, &[100, 200, 400], true),
                Prop::u16(PROP_SAVE_DEST, 1, &[1, 0x10], true),
                Prop::u16(PROP_LIVE_VIEW, 1, &[0, 1], false),
                Prop::u32(PROP_OBJECT_IN_MEMORY, 0, &[], false),
            ],
            handshake: vec![0, 0, 0, 0, 0, 0, 0, 0],
            shots: 0,
            refuse_writes: false,
            capture_payload: None,
        }
    }

    fn prop_mut(&mut self, code: u16) -> Option<&mut Prop> {
        self.props.iter_mut().find(|p| p.code == code)
    }

    fn ok(data: Vec<u8>) -> Reply {
        Reply { code: OK, params: Vec::new(), data }
    }
}

impl Transport for Sim {
    fn transact(&mut self, code: u16, params: &[u32], outgoing: Option<&[u8]>) -> Result<Reply> {
        match code {
            OPEN_SESSION | CLOSE_SESSION | SDIO_GET_EXT_DEVICE_INFO => Ok(Self::ok(Vec::new())),
            GET_DEVICE_INFO => Ok(Self::ok(device_info("Sony", "ILCE-7M3"))),
            SDIO_CONNECT => match params.first().copied().unwrap_or(0) {
                1 => Ok(Self::ok(self.handshake.clone())),
                2 | 3 => {
                    if outgoing.is_some() {
                        return Err(Error::new("connect does not take outgoing data"));
                    }
                    Ok(Self::ok(self.handshake.clone()))
                }
                _ => Err(Error::new("unknown connect phase")),
            },
            SDIO_GET_ALL => Ok(Self::ok(props::encode_props(&self.props)?)),
            SDIO_SET_EXT_DEVICE_PROP_VALUE => {
                if self.refuse_writes {
                    return Ok(Reply { code: 0x2002, params: Vec::new(), data: Vec::new() });
                }
                let code = params.first().copied().unwrap_or(0) as u16;
                let button = outgoing.and_then(|b| b.get(..2)).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
                // Shutter and focus are button properties (1 up, 2 down), not SDIO_ControlDevice.
                if code == 0xD2C2 && button == 2 {
                    let shots = self.shots.saturating_add(1);
                    self.shots = shots;
                    if let Some(p) = self.prop_mut(PROP_OBJECT_IN_MEMORY) {
                        p.current = 0x8000 | u64::from(shots);
                    }
                    return Ok(Self::ok(Vec::new()));
                }
                if (0xD2C1..=0xD2D4).contains(&code) {
                    return Ok(Self::ok(Vec::new()));
                }
                let prop = self.prop_mut(code).ok_or_else(|| Error::new("unknown property"))?;
                let datatype = prop.datatype;
                let value = match datatype {
                    props::TY_U16 => outgoing.and_then(|b| b.get(..2)).map(|b| u64::from(u16::from_le_bytes([b[0], b[1]]))),
                    props::TY_U32 => outgoing.and_then(|b| b.get(..4)).map(|b| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
                    _ => None,
                };
                let Some(value) = value else { return Err(Error::new("property value was cut short")) };
                prop.current = value;
                Ok(Self::ok(Vec::new()))
            }
            // The value travels in the data phase. Down is 2. A bare second parameter is not a press.
            SDIO_CONTROL_DEVICE => {
                let ctrl = params.first().copied().unwrap_or(0);
                let button = outgoing.and_then(|b| b.get(..2)).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0);
                if button == 2 && (ctrl == 0xD2C2 || ctrl == 0xD2C7) {
                    let shots = self.shots.saturating_add(1);
                    self.shots = shots;
                    if let Some(p) = self.prop_mut(PROP_OBJECT_IN_MEMORY) {
                        p.current = 0x8000 | u64::from(shots);
                    }
                }
                if let Some(prop) = self.prop_mut(ctrl as u16) {
                    let value = match prop.datatype {
                        props::TY_U16 | props::TY_I16 => outgoing.and_then(|b| b.get(..2)).map(|b| u64::from(u16::from_le_bytes([b[0], b[1]]))),
                        props::TY_U32 | props::TY_I32 => outgoing.and_then(|b| b.get(..4)).map(|b| u64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))),
                        _ => None,
                    };
                    if let Some(value) = value {
                        prop.current = value;
                    }
                }
                Ok(Self::ok(Vec::new()))
            }
            GET_OBJECT_INFO => {
                let handle = params.first().copied().unwrap_or(0);
                let (name, n) = if handle == LIVE_VIEW_OBJECT {
                    ("live.jpg", TINY_JPEG.len() as u32)
                } else if handle == CAPTURED_OBJECT {
                    ("DSC00001.ARW", 16u32)
                } else {
                    return Err(Error::new("unknown object"));
                };
                Ok(Self::ok(info::object_info(name, n)))
            }
            GET_OBJECT => {
                let handle = params.first().copied().unwrap_or(0);
                if handle == LIVE_VIEW_OBJECT {
                    let mut wrapped = vec![0, 0, 0, 0];
                    wrapped.extend_from_slice(TINY_JPEG);
                    return Ok(Self::ok(wrapped));
                }
                if handle == CAPTURED_OBJECT {
                    if let Some(bytes) = self.capture_payload.clone() {
                        return Ok(Self::ok(bytes));
                    }
                    let mut bytes = b"II*\0".to_vec();
                    bytes.extend_from_slice(format!("ARW{shots}", shots = self.shots).as_bytes());
                    return Ok(Self::ok(bytes));
                }
                Err(Error::new("unknown object"))
            }
            _ => Err(Error::new(format!("sim has no operation {code:#06x}"))),
        }
    }
}
