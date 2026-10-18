//! A Sony Camera Control PTP 2 session: connect, exposure, live view, shutter, file download.
//!
//! Operation codes and the connect / live-view / shutter sequence are Sony's Camera Control PTP 2
//! (Camera Remote Command), the profile the A7 III implements. PTP 3 is not this module.

use crate::info::{self, DeviceInfo};
use crate::props::{self, Prop, step_enum};
use crate::{Error, Result};

pub const LIVE_VIEW_OBJECT: u32 = 0xFFFF_C002;
pub const CAPTURED_OBJECT: u32 = 0xFFFF_C001;

const GET_DEVICE_INFO: u16 = 0x1001;
const OPEN_SESSION: u16 = 0x1002;
const CLOSE_SESSION: u16 = 0x1003;
const GET_OBJECT_INFO: u16 = 0x1008;
const GET_OBJECT: u16 = 0x1009;
const SDIO_CONNECT: u16 = 0x9201;
const SDIO_GET_EXT_DEVICE_INFO: u16 = 0x9202;
const SDIO_SET_EXT_DEVICE_PROP_VALUE: u16 = 0x9205;
const SDIO_CONTROL_DEVICE: u16 = 0x9207;
const SDIO_GET_ALL_EXT_DEVICE_PROP_INFO: u16 = 0x9209;

const OK: u16 = 0x2001;
const BUSY: u16 = 0x2019;
const SESSION_ALREADY_OPEN: u16 = 0x201E;

pub const PROP_WHITE_BALANCE: u16 = 0x5005;
pub const PROP_COLOR_TEMP: u16 = 0xD20F;
pub const PROP_FNUMBER: u16 = 0x5007;
pub const PROP_EXPOSURE_COMP: u16 = 0x5010;
pub const PROP_SHUTTER: u16 = 0xD20D;
pub const PROP_ISO: u16 = 0xD21E;
pub const PROP_OBJECT_IN_MEMORY: u16 = 0xD215;
pub const PROP_LIVE_VIEW: u16 = 0xD221;
pub const PROP_SAVE_DEST: u16 = 0xD222;

const CTRL_S1: u32 = 0xD2C1;
const CTRL_S2: u32 = 0xD2C2;
const CTRL_MAGNIFY: u32 = 0xD2CB;
const CTRL_MAGNIFY_CANCEL: u32 = 0xD2CC;
const CTRL_UP: u32 = 0xD2CD;
const CTRL_DOWN: u32 = 0xD2CE;
const CTRL_LEFT: u32 = 0xD2CF;
const CTRL_RIGHT: u32 = 0xD2D0;
const PROP_FOCUS_MODE: u16 = 0x500A;
const PROP_FOCUS_INDICATION: u16 = 0xD213;
/// Idle focus indication on this body is 1. The next value it lists is the lock.
const FOCUS_LOCKED: u64 = 2;
const FOCUS_MANUAL: u64 = 1;
const BUTTON_UP: u32 = 1;
const BUTTON_DOWN: u32 = 2;
const HOST_DEST: u64 = 0x10;
const BOTH_DEST: u64 = 0x11;
const INITIATOR_VERSION: u32 = 0x00C8;
const READY_BIT: u64 = 0x8000;

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dial {
    Aperture,
    Shutter,
    Iso,
    WhiteBalance,
}

impl Dial {
    pub fn code(self) -> u16 {
        match self {
            Dial::Aperture => PROP_FNUMBER,
            Dial::Shutter => PROP_SHUTTER,
            Dial::Iso => PROP_ISO,
            Dial::WhiteBalance => PROP_WHITE_BALANCE,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Dial::Aperture => "aperture",
            Dial::Shutter => "shutter",
            Dial::Iso => "ISO",
            Dial::WhiteBalance => "white balance",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    pub name: String,
    pub bytes: Vec<u8>,
}

pub struct SonySession<T> {
    io: T,
    info: DeviceInfo,
    props: Vec<Prop>,
    last_object: u64,
    pub magnified: bool,
    /// Shown when the body would not move stills onto the computer.
    pub hint: String,
    /// One exposure click moved several stops. Further clicks are refused.
    runaway: bool,
}

impl<T: Transport> SonySession<T> {
    pub fn open(mut io: T) -> Result<Self> {
        let opened = call(&mut io, OPEN_SESSION, &[1], None)?;
        let info_reply = call(&mut io, GET_DEVICE_INFO, &[], None)?;
        let info = info::parse_device_info(&info_reply.data)?;
        if !is_sony(&info) {
            let _ = call(&mut io, CLOSE_SESSION, &[], None);
            return Err(Error::new(format!("{} {} is not a Sony camera. This tether speaks Sony PTP 2 (PC Remote).", info.manufacturer, info.model)));
        }
        // A camera that still says PC already finished the handshake. Repeating it stalls an A7 III.
        let props = if opened.code == SESSION_ALREADY_OPEN {
            match call(&mut io, SDIO_GET_ALL_EXT_DEVICE_PROP_INFO, &[], None).and_then(|reply| props::parse_props(&reply.data)) {
                Ok(props) => props,
                Err(_) => reopen(&mut io)?,
            }
        } else {
            match handshake(&mut io) {
                Ok(props) => props,
                Err(e) => {
                    let _ = call(&mut io, CLOSE_SESSION, &[], None);
                    return Err(e);
                }
            }
        };
        let last_object = props.iter().find(|p| p.code == PROP_OBJECT_IN_MEMORY).map(|p| p.current).unwrap_or(0);
        let mut session = Self { io, info, props, last_object, magnified: false, hint: String::new(), runaway: false };
        session.aim_at_computer()?;
        session.note_locked_dials();
        Ok(session)
    }

    pub fn close(&mut self) -> Result<()> {
        self.io.set_budget(2_000);
        call(&mut self.io, CLOSE_SESSION, &[], None).map(|_| ())
    }

    pub fn set_budget(&mut self, millis: u64) {
        self.io.set_budget(millis);
    }

    pub fn model(&self) -> &str {
        &self.info.model
    }

    pub fn manufacturer(&self) -> &str {
        &self.info.manufacturer
    }

    pub fn prop(&self, code: u16) -> Option<&Prop> {
        self.props.iter().find(|p| p.code == code)
    }

    pub fn text(&self, dial: Dial) -> Option<String> {
        let prop = self.prop(dial.code())?;
        Some(match dial {
            Dial::Aperture => format_aperture(prop.current),
            Dial::Shutter => format_shutter(prop.current),
            Dial::Iso => format_iso(prop.current),
            Dial::WhiteBalance => {
                if prop.current == 0x8012
                    && let Some(k) = self.prop(PROP_COLOR_TEMP).map(|p| p.current)
                    && (2_000..=12_000).contains(&k)
                {
                    format!("{k}K")
                } else {
                    format_white_balance(prop.current)
                }
            }
        })
    }

    pub fn has_white_balance(&self) -> bool {
        self.prop(PROP_WHITE_BALANCE).is_some()
    }

    /// Exposure compensation, in the camera's thousandths-of-a-stop form, as `+0.0`.
    pub fn exposure_comp(&self) -> Option<String> {
        self.prop(PROP_EXPOSURE_COMP).map(|p| format_exposure_comp(p.current))
    }

    pub fn refresh(&mut self) -> Result<()> {
        let reply = call(&mut self.io, SDIO_GET_ALL_EXT_DEVICE_PROP_INFO, &[], None)?;
        self.props = props::parse_props(&reply.data)?;
        Ok(())
    }

    /// Step a dial (`step` is -1 or +1) or write an absolute property value the body listed.
    ///
    /// White balance has the absolute bit set and takes `SDIO_SetExtDevicePropValue` with the new
    /// value. Aperture, shutter and ISO do not: that opcode is ignored, and `SDIO_ControlDevice`
    /// does move them, but the data is one click (`+1` / `-1`), not the speed itself. The speed's
    /// low half is a repeat count (1/40 is 40), which runs the shutter to the end and keeps going.
    pub fn set_dial(&mut self, dial: Dial, step: i32, absolute: Option<u64>) -> Result<u64> {
        if self.runaway && absolute.is_none() && self.prop(dial.code()).is_some_and(|p| p.flags & 0x01 == 0) {
            return Err(Error::new("exposure clicks are stopped because the last one jumped several stops"));
        }
        let prop = self.prop(dial.code()).cloned().ok_or_else(|| Error::new(format!("this camera has no {}", dial.label())))?;
        if let Some(target) = absolute
            && prop.flags & 0x01 == 0
        {
            // Exposure ignores an absolute write. Walk there one click at a time.
            // The target word itself is a repeat count and must not be sent.
            return self.walk_to(dial, &prop, target);
        }
        let value = match absolute {
            Some(v) => v,
            None => next_dial_value(&prop, dial, step).ok_or_else(|| Error::new(format!("{} is already at the end of its range", dial.label())))?,
        };
        if !prop.enums.is_empty() && !prop.enums.contains(&value) {
            return Err(Error::new(format!("this camera does not offer that {}", dial.label())));
        }
        if prop.flags & 0x01 == 0 && absolute.is_none() {
            // One click. The body often publishes the new stop a moment later; reading once
            // and calling that a refusal is how a real change was reported as a failure.
            // A second write is not sent. The speed word is a repeat count on this camera.
            self.step_exposure(&prop, step)?;
            let now = self.published(prop.code, prop.current)?;
            if now != prop.current
                && let Some(delta) = stops_apart(&prop, dial, now)
                && delta.unsigned_abs() > 4
            {
                self.runaway = true;
                return Err(Error::new(format!("stopped. {} jumped {delta} stops on one click", dial.label())));
            }
            return Ok(now);
        }
        let bytes = props::encode_value(prop.datatype, value)?;
        self.one_write(SDIO_SET_EXT_DEVICE_PROP_VALUE, u32::from(prop.code), &bytes)?;
        let now = self.published(prop.code, prop.current)?;
        if now == value || now != prop.current {
            return Ok(now);
        }
        Err(self.kept(dial))
    }

    pub fn magnify(&mut self, on: bool) -> Result<()> {
        let code = if on { CTRL_MAGNIFY } else { CTRL_MAGNIFY_CANCEL };
        self.press(code)?;
        self.magnified = on;
        Ok(())
    }

    /// Move the focus-magnifier window. These are the remote-key buttons this body lists.
    pub fn nudge(&mut self, direction: &str) -> Result<()> {
        let code = match direction {
            "up" => CTRL_UP,
            "down" => CTRL_DOWN,
            "left" => CTRL_LEFT,
            "right" => CTRL_RIGHT,
            _ => return Err(Error::new("magnifier move is up, down, left, or right")),
        };
        self.press(code)
    }

    /// Values the dial can land on, in menu order, with the labels the bar shows.
    pub fn choices(&self, dial: Dial) -> Vec<(u64, String)> {
        let Some(prop) = self.prop(dial.code()) else { return Vec::new() };
        let mut values = ladder(prop, dial);
        if !values.contains(&prop.current) {
            values.insert(0, prop.current);
        }
        values.into_iter().map(|v| (v, self.label(dial, v))).collect()
    }

    fn label(&self, dial: Dial, value: u64) -> String {
        match dial {
            Dial::Aperture => format_aperture(value),
            Dial::Shutter => format_shutter(value),
            Dial::Iso => format_iso(value),
            Dial::WhiteBalance => {
                if value == 0x8012
                    && let Some(k) = self.prop(PROP_COLOR_TEMP).map(|p| p.current)
                    && (2_000..=12_000).contains(&k)
                    && self.prop(PROP_WHITE_BALANCE).is_some_and(|p| p.current == value)
                {
                    format!("{k}K")
                } else {
                    format_white_balance(value)
                }
            }
        }
    }

    /// Half-press the shutter so the body autofocuses, then release it.
    pub fn autofocus(&mut self) -> Result<()> {
        self.press(CTRL_S1)
    }

    pub fn capture(&mut self) -> Result<Captured> {
        // Hold S1 (autofocus) across the S2 press. Releasing S1 first cancels the focus.
        // Up is sent even when down fails, so a half-press cannot stay held.
        // An AF-S body will not open the shutter until focus locks. Letting S2 go after a
        // few dozen milliseconds is what primed the camera and then cancelled the shot.
        let manual = self.prop(PROP_FOCUS_MODE).is_some_and(|p| p.current == FOCUS_MANUAL);
        let mut first_err = None;
        let mut focused = manual;
        if let Err(e) = self.control(CTRL_S1, BUTTON_DOWN) {
            first_err = Some(e);
        } else if manual {
            pause(80);
        } else {
            for _ in 0..8 {
                pause(150);
                if self.refresh().is_err() {
                    break;
                }
                if self.prop(PROP_FOCUS_INDICATION).is_some_and(|p| p.current == FOCUS_LOCKED) {
                    focused = true;
                    break;
                }
            }
        }
        if first_err.is_none() {
            if let Err(e) = self.control(CTRL_S2, BUTTON_DOWN) {
                first_err = Some(e);
            } else {
                for _ in 0..8 {
                    pause(60);
                    if self.refresh().is_ok() {
                        let now = self.prop(PROP_OBJECT_IN_MEMORY).map(|p| p.current).unwrap_or(0);
                        if now != 0 && now != self.last_object && now & READY_BIT != 0 {
                            break;
                        }
                    }
                }
            }
        }
        if let Err(e) = self.control(CTRL_S2, BUTTON_UP)
            && first_err.is_none()
        {
            first_err = Some(e);
        }
        if let Err(e) = self.control(CTRL_S1, BUTTON_UP)
            && first_err.is_none()
        {
            first_err = Some(e);
        }
        if let Some(e) = first_err {
            return Err(e);
        }
        for _ in 0..60 {
            self.refresh()?;
            if let Some(file) = self.take_new_file()? {
                return Ok(file);
            }
            pause(50);
        }
        if focused {
            Err(Error::new("the camera didn't return a file"))
        } else {
            Err(Error::new("the shutter didn't release. Autofocus didn't lock — set the focus mode to MF, or aim at an edge the camera can focus."))
        }
    }

    /// A shot pressed on the body: a new object-in-memory value with the ready bit set.
    pub fn take_new_file(&mut self) -> Result<Option<Captured>> {
        let now = self.prop(PROP_OBJECT_IN_MEMORY).map(|p| p.current).unwrap_or(0);
        if now == 0 || now == self.last_object || now & READY_BIT == 0 {
            return Ok(None);
        }
        match self.read_object(CAPTURED_OBJECT, "capture.arw") {
            Ok(file) => {
                self.last_object = now;
                Ok(Some(file))
            }
            // The ready bit can be set before the file is readable. That is not a failed shot.
            Err(e) if e.message().contains("0x2009") || e.message().contains("0x200f") => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub fn live_jpeg(&mut self) -> Result<Option<Vec<u8>>> {
        let on = self.prop(PROP_LIVE_VIEW).is_some_and(|p| p.enabled && p.current != 0);
        if !on {
            return Ok(None);
        }
        let file = self.read_object(LIVE_VIEW_OBJECT, "live.jpg")?;
        Ok(Some(file.bytes))
    }

    fn aim_at_computer(&mut self) -> Result<()> {
        let Some(prop) = self.prop(PROP_SAVE_DEST).cloned() else {
            self.hint = "Set the still-image save destination to the computer on the camera.".into();
            return Ok(());
        };
        if prop.current == HOST_DEST || prop.current == BOTH_DEST {
            return Ok(());
        }
        let Some(want) = prop.enums.iter().copied().find(|v| *v == HOST_DEST).or_else(|| prop.enums.iter().copied().find(|v| *v == BOTH_DEST)) else {
            self.hint = "Set the still-image save destination to the computer on the camera.".into();
            return Ok(());
        };
        if !prop.writable && prop.availability != 1 {
            self.hint = "Set the still-image save destination to the computer on the camera.".into();
            return Ok(());
        }
        let bytes = match props::encode_value(prop.datatype, want) {
            Ok(bytes) => bytes,
            Err(_) => {
                self.hint = "The camera refused to save stills to the computer. Set that on the camera.".into();
                return Ok(());
            }
        };
        // The absolute setter. The step setter is for exposure stops, and a bad one stalls the body.
        if self.one_write(SDIO_SET_EXT_DEVICE_PROP_VALUE, u32::from(prop.code), &bytes).is_err() {
            self.hint = "The camera refused to save stills to the computer. Set that on the camera.".into();
            return Ok(());
        }
        self.refresh()?;
        Ok(())
    }

    fn note_locked_dials(&mut self) {
        // Manual (1) already lets the host step exposure. Availability 2 in another mode means
        // the body is choosing the stop. There is no aperture-ring lock to report.
        let mode = self.prop(0x500E).map(|p| p.current);
        if matches!(mode, None | Some(1)) {
            return;
        }
        let locked = [Dial::Aperture, Dial::Shutter, Dial::Iso].iter().any(|dial| self.prop(dial.code()).is_some_and(|p| p.availability == 2));
        if !locked {
            return;
        }
        let Some(name) = mode.and_then(exposure_mode_name) else { return };
        let line = format!("The mode dial is on {name}, so the camera is choosing aperture, shutter, and ISO.");
        if self.hint.is_empty() {
            self.hint = line;
        } else {
            self.hint = format!("{}\n{line}", self.hint);
        }
    }

    /// Re-read until `code` moves, or a short wait is up. The A7 III applies a click and only
    /// then updates the property list.
    fn published(&mut self, code: u16, before: u64) -> Result<u64> {
        for attempt in 0..10 {
            self.refresh()?;
            let now = self.prop(code).map(|p| p.current).unwrap_or(before);
            if now != before {
                return Ok(now);
            }
            if attempt + 1 < 10 {
                pause(150);
            }
        }
        Ok(before)
    }

    fn kept(&self, dial: Dial) -> Error {
        let mode = self.prop(0x500E).map(|p| p.current);
        match mode.and_then(exposure_mode_name) {
            Some(name) if mode != Some(1) => Error::new(format!("the camera kept {}. The mode dial is on {name}.", dial.label())),
            _ => Error::new(format!("the camera kept {}", dial.label())),
        }
    }

    /// Reach `target` by single clicks. One command whose payload is the stop count only
    /// moves one stop on this body, and clicks sent with no gap are dropped after the first.
    /// A short pause between clicks lets the next one in, without reading the whole property
    /// list every time — that read was the one-second-per-stop delay. The payload stays ±1.
    /// The setting itself must not be sent: 1/40's low half is 40, and that count ran the
    /// shutter into the stop.
    fn walk_to(&mut self, dial: Dial, prop: &Prop, target: u64) -> Result<u64> {
        if self.runaway {
            return Err(Error::new("exposure clicks are stopped because the last one jumped several stops"));
        }
        let list = ladder(prop, dial);
        if list.iter().all(|v| *v != target) {
            return Err(Error::new(format!("this camera does not offer that {}", dial.label())));
        }
        let mut current = prop.current;
        if current == target {
            return Ok(current);
        }
        if let Ok(bytes) = props::encode_value(prop.datatype, target) {
            let _ = self.one_write(SDIO_SET_EXT_DEVICE_PROP_VALUE, u32::from(prop.code), &bytes);
            if self.refresh().is_ok()
                && let Some(now) = self.prop(prop.code).map(|p| p.current)
            {
                if now == target {
                    return Ok(now);
                }
                current = now;
            }
        }
        // Eight clicks, then look. A tighter burst is what the body drops. If this gap is
        // still too short, the next pass waits longer instead of firing the rest into the void.
        let mut gap = 180u64;
        for round in 0..8 {
            let delta = toward(&list, current, target).ok_or_else(|| Error::new(format!("this camera does not offer that {}", dial.label())))?;
            if delta == 0 {
                return Ok(current);
            }
            let step = if delta > 0 { 1 } else { -1 };
            let want = delta.unsigned_abs().min(8);
            for i in 0..want {
                self.step_exposure(prop, step)?;
                if i + 1 < want {
                    pause(gap);
                }
            }
            let now = self.catch_up(prop.code, current)?;
            let moved = toward(&list, current, now).map(|d| d.unsigned_abs()).unwrap_or(0);
            if moved > want {
                self.runaway = true;
                return Err(Error::new(format!("stopped. {} jumped {moved} stops after {want} clicks", dial.label())));
            }
            if now == target {
                return Ok(now);
            }
            if moved == 0 {
                return Err(self.kept(dial));
            }
            current = now;
            if moved + 1 < want {
                gap = 360;
                if round >= 2 {
                    return self.step_until(dial, prop, &list, current, target);
                }
            }
        }
        Err(Error::new(format!("{} stopped short of that setting", dial.label())))
    }

    /// One click, then the next once that click is visible. Used when a timed gap was too short
    /// and the body kept only the first click of a run.
    fn step_until(&mut self, dial: Dial, prop: &Prop, list: &[u64], mut current: u64, target: u64) -> Result<u64> {
        for _ in 0..48 {
            let delta = toward(list, current, target).ok_or_else(|| Error::new(format!("this camera does not offer that {}", dial.label())))?;
            if delta == 0 {
                return Ok(current);
            }
            let step = if delta > 0 { 1 } else { -1 };
            self.step_exposure(prop, step)?;
            let now = self.published(prop.code, current)?;
            if now == current {
                return Err(self.kept(dial));
            }
            if let Some(moved) = toward(list, current, now)
                && moved.unsigned_abs() > 4
            {
                self.runaway = true;
                return Err(Error::new(format!("stopped. {} jumped {moved} stops on one click", dial.label())));
            }
            current = now;
            if current == target {
                return Ok(current);
            }
        }
        Err(Error::new(format!("{} stopped short of that setting", dial.label())))
    }

    /// Read until the value stops changing. An unchanged list is not treated as finished
    /// after a couple of reads — the body publishes the new stop a moment after the click.
    fn catch_up(&mut self, code: u16, before: u64) -> Result<u64> {
        let mut last = before;
        let mut same = 0u8;
        for attempt in 0..8 {
            self.refresh()?;
            let now = self.prop(code).map(|p| p.current).unwrap_or(last);
            if now == last {
                same = same.saturating_add(1);
                if now != before && same >= 2 {
                    return Ok(now);
                }
            } else {
                same = 0;
                last = now;
            }
            if attempt + 1 < 8 {
                pause(40);
            }
        }
        Ok(last)
    }

    /// One exposure click. The payload is +1 or −1, never a stop count and never the setting.
    fn step_exposure(&mut self, prop: &Prop, step: i32) -> Result<()> {
        if self.runaway {
            return Err(Error::new("exposure clicks are stopped because the last one jumped several stops"));
        }
        if step != 1 && step != -1 {
            return Err(Error::new("refusing an exposure move that is not one click"));
        }
        let bytes = exposure_step_bytes(step);
        if !control_click(&bytes) {
            return Err(Error::new("refusing an exposure move that is not one click"));
        }
        self.io.set_budget(4_000);
        let reply = self.io.transact(SDIO_CONTROL_DEVICE, &[u32::from(prop.code)], Some(&bytes));
        self.io.set_budget(8_000);
        let reply = reply?;
        if reply.code == OK {
            Ok(())
        } else {
            Err(Error::new(format!("{} was refused ({:#06x})", op_name(SDIO_CONTROL_DEVICE), reply.code)))
        }
    }

    fn one_write(&mut self, op: u16, code: u32, bytes: &[u8]) -> Result<()> {
        let timed = op == SDIO_CONTROL_DEVICE;
        if timed {
            self.io.set_budget(4_000);
        }
        let result = call(&mut self.io, op, &[code], Some(bytes));
        if timed {
            self.io.set_budget(8_000);
        }
        result.map(|_| ())
    }

    /// Button down is 2 and up is 1, as two bytes of data. One attempt: a retry would press again.
    fn control(&mut self, code: u32, value: u32) -> Result<()> {
        if value != BUTTON_UP && value != BUTTON_DOWN {
            return Err(Error::new(format!("{}: refusing a value that is not up or down", button_name(code))));
        }
        let bytes = (value as u16).to_le_bytes();
        if !control_click(&bytes) {
            return Err(Error::new(format!("{}: refusing a value that is not one click", button_name(code))));
        }
        self.io.set_budget(4_000);
        let reply = self.io.transact(SDIO_CONTROL_DEVICE, &[code], Some(&bytes));
        self.io.set_budget(8_000);
        match reply {
            Ok(reply) if reply.code == OK => Ok(()),
            Ok(reply) => Err(Error::new(format!("{}: refused ({:#06x})", button_name(code), reply.code))),
            Err(e) => Err(Error::new(format!("{}: {}", button_name(code), e.message()))),
        }
    }

    fn press(&mut self, code: u32) -> Result<()> {
        let down = self.control(code, BUTTON_DOWN);
        pause(80);
        let up = self.control(code, BUTTON_UP);
        down?;
        up
    }

    fn read_object(&mut self, handle: u32, fallback: &str) -> Result<Captured> {
        let name = match call(&mut self.io, GET_OBJECT_INFO, &[handle], None) {
            Ok(info) => {
                let parsed = info::object_filename(&info.data).unwrap_or_else(|_| fallback.to_string());
                if parsed.is_empty() { fallback.to_string() } else { parsed }
            }
            // Live view is a JPEG object. Some bodies refuse its object-info dataset and still
            // return the bytes from GetObject.
            Err(_) if handle == LIVE_VIEW_OBJECT => fallback.to_string(),
            Err(e) => return Err(e),
        };
        let obj = call(&mut self.io, GET_OBJECT, &[handle], None)?;
        let bytes = if handle == LIVE_VIEW_OBJECT { jpeg_payload(&obj.data)? } else { file_bytes(&obj.data) };
        Ok(Captured { name, bytes })
    }
}

fn call<T: Transport>(io: &mut T, code: u16, params: &[u32], outgoing: Option<&[u8]>) -> Result<Reply> {
    let mut last = Error::new("camera stayed busy");
    for attempt in 0..3 {
        let reply = io.transact(code, params, outgoing)?;
        if reply.code == BUSY && attempt < 2 {
            last = Error::new(format!("{} stayed busy", op_name(code)));
            continue;
        }
        if reply.code == OK || (code == OPEN_SESSION && reply.code == SESSION_ALREADY_OPEN) {
            return Ok(reply);
        }
        return Err(Error::new(format!("{} was refused ({:#06x})", op_name(code), reply.code)));
    }
    Err(last)
}

fn is_sony(info: &DeviceInfo) -> bool {
    let maker = info.manufacturer.to_ascii_lowercase();
    let model = info.model.to_ascii_lowercase();
    maker.contains("sony") || model.starts_with("ilce") || model.starts_with("ilme") || model.starts_with("dsc-") || model.starts_with("zv-")
}

fn handshake<T: Transport>(io: &mut T) -> Result<Vec<Prop>> {
    // Each connect phase returns 8 zero bytes from the camera. Sending them back stalls an A7 III.
    call(io, SDIO_CONNECT, &[1, 0, 0], None)?;
    call(io, SDIO_CONNECT, &[2, 0, 0], None)?;
    call(io, SDIO_GET_EXT_DEVICE_INFO, &[INITIATOR_VERSION], None)?;
    call(io, SDIO_CONNECT, &[3, 0, 0], None)?;
    let reply = call(io, SDIO_GET_ALL_EXT_DEVICE_PROP_INFO, &[], None)?;
    props::parse_props(&reply.data)
}

fn reopen<T: Transport>(io: &mut T) -> Result<Vec<Prop>> {
    let _ = call(io, CLOSE_SESSION, &[], None);
    call(io, OPEN_SESSION, &[1], None)?;
    match handshake(io) {
        Ok(props) => Ok(props),
        Err(e) => {
            let _ = call(io, CLOSE_SESSION, &[], None);
            Err(e)
        }
    }
}

fn exposure_mode_name(mode: u64) -> Option<&'static str> {
    match mode {
        1 => Some("M"),
        2 => Some("P"),
        3 => Some("A"),
        4 => Some("S"),
        0x8000 => Some("Auto"),
        _ => None,
    }
}

fn button_name(code: u32) -> &'static str {
    match code {
        CTRL_S1 => "autofocus",
        CTRL_S2 => "the shutter",
        CTRL_MAGNIFY | CTRL_MAGNIFY_CANCEL => "the magnifier",
        CTRL_UP | CTRL_DOWN | CTRL_LEFT | CTRL_RIGHT => "the magnifier position",
        _ => "a camera button",
    }
}

fn pause(ms: u64) {
    #[cfg(not(target_arch = "wasm32"))]
    {
        // Tests poll the same way the body does, without waiting out the camera's lag.
        let ms = if cfg!(test) { 0 } else { ms };
        std::thread::sleep(std::time::Duration::from_millis(ms));
    }
    #[cfg(target_arch = "wasm32")]
    let _ = ms;
}

/// Third-stop f-numbers, as the camera stores them (f/4 is 400). Wider apertures come first.
const APERTURES: &[u64] = &[
    100, 110, 120, 140, 160, 180, 200, 220, 250, 280, 320, 350, 400, 450, 500, 560, 630, 710, 800, 900, 1000, 1100, 1300, 1400, 1600, 1800, 2000,
    2200,
];

/// Slow exposures first, so a positive step is a faster shutter.
const SHUTTERS: &[u64] = &[
    shut_s(30),
    shut_s(25),
    shut_s(20),
    shut_s(15),
    shut_s(13),
    shut_s(10),
    shut_s(8),
    shut_s(6),
    shut_s(5),
    shut_s(4),
    shut_s(3),
    shut_s(2),
    shut_s(1),
    shut_f(2),
    shut_f(3),
    shut_f(4),
    shut_f(5),
    shut_f(6),
    shut_f(8),
    shut_f(10),
    shut_f(13),
    shut_f(15),
    shut_f(20),
    shut_f(25),
    shut_f(30),
    shut_f(40),
    shut_f(50),
    shut_f(60),
    shut_f(80),
    shut_f(100),
    shut_f(125),
    shut_f(160),
    shut_f(200),
    shut_f(250),
    shut_f(320),
    shut_f(400),
    shut_f(500),
    shut_f(640),
    shut_f(800),
    shut_f(1000),
    shut_f(1250),
    shut_f(1600),
    shut_f(2000),
    shut_f(2500),
    shut_f(3200),
    shut_f(4000),
    shut_f(5000),
    shut_f(6400),
    shut_f(8000),
];

const fn shut_s(seconds: u64) -> u64 {
    (seconds << 16) | 1
}

const fn shut_f(denom: u64) -> u64 {
    (1 << 16) | denom
}

/// A camera control is button up (1), button down (2), or one exposure click (±1).
/// A stop count, a shutter word, an f-number, or an ISO is refused. One command of 40
/// (the low half of 1/40) ran the shutter into the stop.
pub fn control_click(bytes: &[u8]) -> bool {
    let Ok(pair) = <[u8; 2]>::try_from(bytes) else { return false };
    matches!(i16::from_le_bytes(pair), -1 | 1 | 2)
}

/// One click, as a signed 16-bit step.
fn exposure_step_bytes(step: i32) -> [u8; 2] {
    let delta: i16 = if step < 0 { -1 } else { 1 };
    delta.to_le_bytes()
}

/// Absolute properties (bit 0) take `SDIO_SetExtDevicePropValue`. Exposure stops take one
/// `SDIO_ControlDevice` click instead.
#[cfg(test)]
fn setter_for(flags: u8) -> u16 {
    if flags & 0x01 != 0 { SDIO_SET_EXT_DEVICE_PROP_VALUE } else { SDIO_CONTROL_DEVICE }
}

fn dummy_range(prop: &props::Prop) -> bool {
    matches!((prop.minimum, prop.maximum, prop.step), (Some(0), Some(max), Some(1)) if max >= 0xFFFF)
}

fn ladder(prop: &props::Prop, dial: Dial) -> Vec<u64> {
    if !prop.enums.is_empty() {
        if dial == Dial::Iso {
            return iso_bank(prop);
        }
        return prop.enums.clone();
    }
    match dial {
        Dial::Aperture => APERTURES.to_vec(),
        Dial::Shutter => SHUTTERS.to_vec(),
        Dial::Iso | Dial::WhiteBalance => Vec::new(),
    }
}

/// Positive when `to` is later in the list than `from`.
fn toward(list: &[u64], from: u64, to: u64) -> Option<i32> {
    let to_i = list.iter().position(|v| *v == to)?;
    let from_i = list.iter().position(|v| *v == from).or_else(|| list.iter().enumerate().min_by_key(|(_, v)| v.abs_diff(from)).map(|(i, _)| i))?;
    Some(to_i as i32 - from_i as i32)
}

fn next_dial_value(prop: &props::Prop, dial: Dial, step: i32) -> Option<u64> {
    if !prop.enums.is_empty() {
        return step_enum(prop, step);
    }
    if dummy_range(prop) {
        return match dial {
            Dial::Aperture => step_indexed(APERTURES, prop.current, step),
            Dial::Shutter => step_shutter(prop.current, step),
            _ => None,
        };
    }
    step_enum(prop, step)
}

fn stops_apart(prop: &props::Prop, dial: Dial, now: u64) -> Option<i32> {
    let list: &[u64] = if !prop.enums.is_empty() {
        &prop.enums
    } else {
        match dial {
            Dial::Shutter => SHUTTERS,
            Dial::Aperture => APERTURES,
            Dial::Iso | Dial::WhiteBalance => return None,
        }
    };
    let from = list_index(list, prop.current)?;
    let to = list_index(list, now)?;
    Some(to as i32 - from as i32)
}

fn list_index(list: &[u64], value: u64) -> Option<usize> {
    if list.is_empty() {
        return None;
    }
    list.iter().position(|v| *v == value).or_else(|| list.iter().enumerate().min_by_key(|(_, v)| v.abs_diff(value)).map(|(i, _)| i))
}

fn step_indexed(values: &[u64], current: u64, step: i32) -> Option<u64> {
    let idx = values.iter().position(|v| *v == current).or_else(|| values.iter().enumerate().min_by_key(|(_, v)| v.abs_diff(current)).map(|(i, _)| i))?;
    let next = if step < 0 { idx.checked_sub(1)? } else { idx.checked_add(1)? };
    values.get(next).copied()
}

fn shutter_seconds(v: u64) -> f64 {
    let hi = (v >> 16) as f64;
    let lo = (v & 0xffff) as f64;
    if lo == 0.0 { f64::MAX } else { hi / lo }
}

fn step_shutter(current: u64, step: i32) -> Option<u64> {
    let idx = SHUTTERS.iter().position(|v| *v == current).or_else(|| {
        let secs = shutter_seconds(current);
        SHUTTERS
            .iter()
            .enumerate()
            .min_by(|(_, a), (_, b)| {
                let da = (shutter_seconds(**a) - secs).abs();
                let db = (shutter_seconds(**b) - secs).abs();
                da.partial_cmp(&db).unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
    })?;
    let next = if step < 0 { idx.checked_sub(1)? } else { idx.checked_add(1)? };
    SHUTTERS.get(next).copied()
}

fn op_name(code: u16) -> &'static str {
    match code {
        OPEN_SESSION => "opening the session",
        CLOSE_SESSION => "closing the session",
        GET_DEVICE_INFO => "reading the camera",
        GET_OBJECT_INFO => "reading the file info",
        GET_OBJECT => "reading the file",
        SDIO_CONNECT => "connecting",
        SDIO_GET_EXT_DEVICE_INFO => "reading the protocol",
        SDIO_SET_EXT_DEVICE_PROP_VALUE => "changing a setting",
        SDIO_CONTROL_DEVICE => "the shutter",
        SDIO_GET_ALL_EXT_DEVICE_PROP_INFO => "reading the settings",
        _ => "a camera command",
    }
}

pub fn format_aperture(v: u64) -> String {
    if v == 0 {
        return "—".into();
    }
    let f = (v as f64) / 100.0;
    if (f - f.round()).abs() < 0.05 { format!("f/{f:.0}") } else { format!("f/{f:.1}") }
}

pub fn format_shutter(v: u64) -> String {
    let hi = u32::try_from(v >> 16).unwrap_or(0);
    let lo = u32::try_from(v & 0xffff).unwrap_or(0);
    if hi == 1 && lo > 1 {
        format!("1/{lo}")
    } else if lo == 1 && hi > 0 {
        format!("{hi}s")
    } else if hi > 0 && lo > 0 {
        format!("{hi}/{lo}")
    } else if v == 0 {
        "—".into()
    } else {
        v.to_string()
    }
}

/// The ISO list is the real stops, then the same stops again with a high byte set.
/// That byte is a mode (a second copy, then a full-stop copy), not a bigger ISO.
/// 0x01000190 is 400 in that second list, which was showing up as 16777616.
/// `0x5010` is a signed 16-bit count of thousandths of a stop. `0` is even, `1000` is +1.0.
pub fn format_exposure_comp(raw: u64) -> String {
    let bits = u16::try_from(raw & 0xffff).unwrap_or(0);
    let thousandths = i16::from_le_bytes(bits.to_le_bytes());
    let ev = f32::from(thousandths) / 1000.0;
    format!("{ev:+.1}")
}

pub fn format_iso(v: u64) -> String {
    if v == 0 {
        return "—".into();
    }
    let bank = v >> 24;
    let base = v & 0x00FF_FFFF;
    let number = if base == 0x00FF_FFFF {
        "Auto".to_string()
    } else if (1..=409_600).contains(&base) {
        base.to_string()
    } else {
        return v.to_string();
    };
    match bank {
        0 => number,
        1 => format!("NR {number}"),
        2 => format!("NR+ {number}"),
        _ => v.to_string(),
    }
}

/// ISO entries that belong with the value the body is on now. The other copies stay out
/// of the menu so picking ISO 100 does not walk through the repeated list.
fn iso_bank(prop: &props::Prop) -> Vec<u64> {
    let bank = prop.current >> 24;
    let same: Vec<u64> = prop.enums.iter().copied().filter(|v| v >> 24 == bank).collect();
    if same.is_empty() { prop.enums.clone() } else { same }
}

/// Names for the white-balance codes an A7 III lists on property 0x5005.
///
/// The order in that camera's list matches its menu: Auto, Daylight, Shade, Cloudy,
/// Incandescent, the four fluorescent presets, Flash, Underwater, Color Temp, then three customs.
/// Standard PTP only names the first few, so the rest were showing up as raw numbers.
pub fn format_white_balance(v: u64) -> String {
    match v {
        1 => "Manual".into(),
        2 => "Auto".into(),
        4 => "Daylight".into(),
        6 => "Incandescent".into(),
        7 => "Flash".into(),
        0x8001 => "Fluorescent Warm".into(),
        0x8002 => "Fluorescent Cool".into(),
        0x8003 => "Fluorescent Day White".into(),
        0x8004 => "Fluorescent Daylight".into(),
        0x8010 => "Cloudy".into(),
        0x8011 => "Shade".into(),
        0x8012 => "Color Temp".into(),
        0x8020 => "Custom 1".into(),
        0x8021 => "Custom 2".into(),
        0x8022 => "Custom 3".into(),
        0x8030 => "Underwater".into(),
        0 => "—".into(),
        other => other.to_string(),
    }
}

/// JPEG inside a live-view object. The scan stops at the buffer; a missing end marker is an error.
pub fn jpeg_payload(data: &[u8]) -> Result<Vec<u8>> {
    let start = data.windows(2).position(|w| w == [0xFF, 0xD8]).ok_or_else(|| Error::new("live view had no JPEG"))?;
    let rest = data.get(start..).ok_or_else(|| Error::new("live view had no JPEG"))?;
    let mut i = 0;
    while i + 1 < rest.len() {
        if rest.get(i) == Some(&0xFF) && rest.get(i + 1) == Some(&0xD9) {
            let end = i + 2;
            return rest.get(..end).map(|s| s.to_vec()).ok_or_else(|| Error::new("live view JPEG was cut short"));
        }
        i += 1;
    }
    Err(Error::new("live view JPEG had no end"))
}

fn file_bytes(data: &[u8]) -> Vec<u8> {
    if let Some(i) = data.windows(4).position(|w| w == b"II*\0" || w == b"MM\0*") {
        return data.get(i..).unwrap_or(data).to_vec();
    }
    if data.windows(2).any(|w| w == [0xFF, 0xD8])
        && let Ok(jpeg) = jpeg_payload(data)
    {
        return jpeg;
    }
    data.to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aperture_and_shutter_read_as_photographers_write_them() {
        assert_eq!(format_aperture(800), "f/8");
        assert_eq!(format_aperture(280), "f/2.8");
        assert_eq!(format_shutter(0x0001_007D), "1/125");
    }

    #[test]
    fn jpeg_is_cut_out_of_a_header() {
        let mut blob = vec![0, 1, 2, 0xFF, 0xD8, 9, 0xFF, 0xD9, 7];
        blob[0] = 1;
        assert_eq!(jpeg_payload(&blob).unwrap(), vec![0xFF, 0xD8, 9, 0xFF, 0xD9]);
    }

    #[test]
    fn a_truncated_jpeg_is_an_error() {
        assert!(jpeg_payload(&[0xFF, 0xD8, 1, 2, 3]).is_err());
    }

    #[test]
    fn white_balance_uses_the_absolute_setter_and_exposure_uses_the_other() {
        assert_eq!(setter_for(0x01), SDIO_SET_EXT_DEVICE_PROP_VALUE);
        assert_eq!(setter_for(0x00), SDIO_CONTROL_DEVICE);
        assert_eq!(exposure_step_bytes(1), [1, 0]);
        assert_eq!(exposure_step_bytes(-1), [0xff, 0xff]);
        assert!(control_click(&exposure_step_bytes(1)));
        assert!(control_click(&exposure_step_bytes(-1)));
        assert!(control_click(&[1, 0]) && control_click(&[2, 0]));
        assert!(!control_click(&6i16.to_le_bytes()));
        assert!(!control_click(&40i16.to_le_bytes()));
        assert!(!control_click(&((1u32 << 16) | 40).to_le_bytes()));
        assert!(!control_click(&400u16.to_le_bytes()));
        assert!(!control_click(&6400u32.to_le_bytes()));
        // 1/40 is the next stop after 1/30. Its low half is 40, which the body repeated.
        let one_fortieth = (1u32 << 16) | 40;
        assert_eq!(one_fortieth as u16, 40);
        assert_ne!(one_fortieth.to_le_bytes().as_slice(), &exposure_step_bytes(1));
    }

    #[test]
    fn a_shutter_click_sends_one_step_and_not_the_speed() {
        use std::sync::{Arc, Mutex};
        struct Rec {
            controls: Arc<Mutex<Vec<Vec<u8>>>>,
        }
        impl Transport for Rec {
            fn transact(&mut self, code: u16, _params: &[u32], outgoing: Option<&[u8]>) -> Result<Reply> {
                if code == SDIO_CONTROL_DEVICE {
                    self.controls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).push(outgoing.unwrap_or_default().to_vec());
                }
                let data = if code == GET_DEVICE_INFO {
                    crate::info::device_info("Sony", "ILCE-7M3")
                } else if code == SDIO_GET_ALL_EXT_DEVICE_PROP_INFO {
                    let mut shutter = props::Prop::u32(PROP_SHUTTER, 0x0001_001E, &[], false);
                    shutter.flags = 0;
                    shutter.availability = 2;
                    shutter.writable = false;
                    shutter.minimum = Some(0);
                    shutter.maximum = Some(0xFFFF_FFFF);
                    shutter.step = Some(1);
                    props::encode_props(&[shutter]).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let reply_code = if code == OPEN_SESSION { SESSION_ALREADY_OPEN } else { OK };
                Ok(Reply { code: reply_code, params: Vec::new(), data })
            }
        }
        let controls = Arc::new(Mutex::new(Vec::new()));
        let mut session = SonySession::open(Rec { controls: Arc::clone(&controls) }).unwrap();
        session.set_dial(Dial::Shutter, -1, None).unwrap();
        let picked = session.set_dial(Dial::Shutter, 0, Some(0x0001_007D)).unwrap_err();
        assert!(picked.message().contains("kept"), "{picked:?}");
        let sent = controls.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone();
        assert_eq!(sent.first().map(Vec::as_slice), Some([0xff, 0xff].as_slice()));
        // 1/30 to 1/125 is six third-stops. Each click is ±1. The mock never moves, so the
        // walk stops instead of sending the shutter word or a count of 6.
        assert!(sent.len() > 2, "{sent:?}");
        assert!(sent.iter().skip(1).all(|b| b.as_slice() == [1, 0]), "{sent:?}");
        assert!(sent.iter().all(|b| b.len() == 2), "{sent:?}");
    }

    #[test]
    fn aperture_and_iso_clicks_are_one_step_and_a_jump_locks_the_dials() {
        use std::sync::{Arc, Mutex};
        struct Rec {
            state: Arc<Mutex<RecState>>,
        }
        struct RecState {
            controls: Vec<Vec<u8>>,
            jumped: bool,
        }
        impl Transport for Rec {
            fn transact(&mut self, code: u16, params: &[u32], outgoing: Option<&[u8]>) -> Result<Reply> {
                let mut state = self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
                if code == SDIO_CONTROL_DEVICE {
                    state.controls.push(outgoing.unwrap_or_default().to_vec());
                    if params.first().copied() == Some(u32::from(PROP_SHUTTER)) {
                        state.jumped = true;
                    }
                }
                let jumped = state.jumped;
                drop(state);
                let data = if code == GET_DEVICE_INFO {
                    crate::info::device_info("Sony", "ILCE-7M3")
                } else if code == SDIO_GET_ALL_EXT_DEVICE_PROP_INFO {
                    let mut aperture = props::Prop::u16(PROP_FNUMBER, 400, &[], false);
                    aperture.flags = 0;
                    aperture.minimum = Some(0);
                    aperture.maximum = Some(0xFFFF);
                    aperture.step = Some(1);
                    let mut iso = props::Prop::u32(PROP_ISO, 6400, &[3200, 6400, 12800], false);
                    iso.flags = 0;
                    // 1/250 is several third-stops from 1/30, and not the end of the ladder.
                    let shutter_now = if jumped { 0x0001_00FA } else { 0x0001_001E };
                    let mut shutter = props::Prop::u32(PROP_SHUTTER, shutter_now, &[], false);
                    shutter.flags = 0;
                    shutter.minimum = Some(0);
                    shutter.maximum = Some(0xFFFF_FFFF);
                    shutter.step = Some(1);
                    props::encode_props(&[aperture, iso, shutter]).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let reply_code = if code == OPEN_SESSION { SESSION_ALREADY_OPEN } else { OK };
                Ok(Reply { code: reply_code, params: Vec::new(), data })
            }
        }
        let state = Arc::new(Mutex::new(RecState { controls: Vec::new(), jumped: false }));
        let mut session = SonySession::open(Rec { state: Arc::clone(&state) }).unwrap();
        session.set_dial(Dial::Aperture, 1, None).unwrap();
        session.set_dial(Dial::Iso, -1, None).unwrap();
        let jump = session.set_dial(Dial::Shutter, 1, None).unwrap_err();
        assert!(jump.message().contains("jumped"), "{jump:?}");
        let again = session.set_dial(Dial::Shutter, 1, None).unwrap_err();
        assert!(again.message().contains("stopped"), "{again:?}");
        let controls = state.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(controls.controls, vec![vec![1, 0], vec![0xff, 0xff], vec![1, 0]]);
    }

    #[test]
    fn an_a7iii_exposure_reads_as_stops_and_auto_iso() {
        assert_eq!(format_aperture(400), "f/4");
        assert_eq!(format_shutter(65_696), "1/160");
        assert_eq!(format_exposure_comp(0), "+0.0");
        assert_eq!(format_exposure_comp(1_000), "+1.0");
        assert_eq!(format_exposure_comp((-300i16) as u16 as u64), "-0.3");
        assert_eq!(format_exposure_comp((-5_000i16) as u16 as u64), "-5.0");
        assert_eq!(format_iso(0x00FF_FFFF), "Auto");
        assert_eq!(format_iso(400), "400");
        assert_eq!(format_iso(0x0100_0190), "NR 400");
        assert_eq!(format_iso(0x01FF_FFFF), "NR Auto");
        assert_eq!(format_iso(0x0200_0064), "NR+ 100");
        let enums = [0x00FF_FFFFu32, 100, 400, 0x0100_0190, 0x01FF_FFFF, 0x0200_0064];
        let iso = props::Prop::u32(PROP_ISO, 400, &enums, false);
        assert_eq!(ladder(&iso, Dial::Iso), vec![0x00FF_FFFF, 100, 400]);
        assert_eq!(format_white_balance(2), "Auto");
        assert_eq!(format_white_balance(4), "Daylight");
        assert_eq!(format_white_balance(0x8011), "Shade");
        assert_eq!(format_white_balance(0x8010), "Cloudy");
        assert_eq!(format_white_balance(6), "Incandescent");
        assert_eq!(format_white_balance(0x8001), "Fluorescent Warm");
        assert_eq!(format_white_balance(0x8004), "Fluorescent Daylight");
        assert_eq!(format_white_balance(0x8012), "Color Temp");
        assert_eq!(format_white_balance(0x8020), "Custom 1");
        assert_eq!(format_white_balance(0x8030), "Underwater");
    }

    #[test]
    fn a_display_only_dial_still_steps_in_stops() {
        let aperture = props::Prop {
            code: PROP_FNUMBER,
            datatype: props::TY_U16,
            flags: 0,
            availability: 2,
            writable: false,
            enabled: true,
            current: 400,
            enums: Vec::new(),
            minimum: Some(0),
            maximum: Some(65_535),
            step: Some(1),
        };
        assert_eq!(next_dial_value(&aperture, Dial::Aperture, 1), Some(450));
        assert_eq!(next_dial_value(&aperture, Dial::Aperture, -1), Some(350));
        let shutter = props::Prop {
            code: PROP_SHUTTER,
            datatype: props::TY_U32,
            flags: 0,
            availability: 2,
            writable: false,
            enabled: true,
            current: 0x0001_003C,
            enums: Vec::new(),
            minimum: Some(0),
            maximum: Some(0xFFFF_FFFF),
            step: Some(1),
        };
        assert_eq!(format_shutter(next_dial_value(&shutter, Dial::Shutter, 1).unwrap_or(0)), "1/80");
        assert_eq!(format_shutter(next_dial_value(&shutter, Dial::Shutter, -1).unwrap_or(0)), "1/50");
    }

    #[test]
    fn an_already_open_session_does_not_repeat_the_handshake() {
        struct Already;
        impl Transport for Already {
            fn transact(&mut self, code: u16, _params: &[u32], _outgoing: Option<&[u8]>) -> Result<Reply> {
                if code == SDIO_CONNECT {
                    return Err(Error::new("connect must not run when the session is already open"));
                }
                let data = if code == GET_DEVICE_INFO {
                    crate::info::device_info("Sony", "ILCE-7M3")
                } else if code == SDIO_GET_ALL_EXT_DEVICE_PROP_INFO {
                    props::encode_props(&[props::Prop::u16(PROP_FNUMBER, 400, &[280, 400, 560], true)]).unwrap_or_default()
                } else {
                    Vec::new()
                };
                let reply_code = if code == OPEN_SESSION { SESSION_ALREADY_OPEN } else { OK };
                Ok(Reply { code: reply_code, params: Vec::new(), data })
            }
        }
        let session = SonySession::open(Already).unwrap();
        assert_eq!(session.model(), "ILCE-7M3");
        assert_eq!(session.text(Dial::Aperture).as_deref(), Some("f/4"));
    }

    #[test]
    fn a7iii_sim_connects_steps_aperture_and_returns_an_arw() {
        let mut session = SonySession::open(crate::Sim::a7iii()).unwrap();
        assert_eq!(session.model(), "ILCE-7M3");
        assert!(session.hint.is_empty(), "save destination moves to the computer");
        assert_eq!(session.text(Dial::Aperture).as_deref(), Some("f/8"));
        session.set_dial(Dial::Aperture, -1, None).unwrap();
        assert_eq!(session.text(Dial::Aperture).as_deref(), Some("f/5.6"));
        session.magnify(true).unwrap();
        assert!(session.magnified);
        session.nudge("left").unwrap();
        assert!(session.choices(Dial::Aperture).iter().any(|(_, label)| label == "f/5.6"));
        let jpeg = session.live_jpeg().unwrap().unwrap();
        assert!(jpeg.starts_with(&[0xFF, 0xD8]));
        let shot = session.capture().unwrap();
        assert_eq!(shot.name, "DSC00001.ARW");
        assert!(shot.bytes.starts_with(b"II*\0"));
        session.close().unwrap();
    }

    #[test]
    fn a_non_sony_body_is_refused() {
        struct Canon;
        impl Transport for Canon {
            fn transact(&mut self, code: u16, _params: &[u32], _outgoing: Option<&[u8]>) -> Result<Reply> {
                let data = if code == GET_DEVICE_INFO { crate::info::device_info("Canon", "EOS R5") } else { Vec::new() };
                Ok(Reply { code: OK, params: Vec::new(), data })
            }
        }
        match SonySession::open(Canon) {
            Err(err) => assert!(err.message().contains("not a Sony")),
            Ok(_) => panic!("a Canon body must be refused"),
        }
    }
}
