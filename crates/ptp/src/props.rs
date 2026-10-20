//! Sony extended device properties (Camera Control PTP 2).
//!
//! The dataset is the PIMA device-property descriptor with Sony's extra "is enabled" byte after
//! Get/Set. `SDIO_GetAllExtDevicePropInfo` (0x9209) is a little-endian count of those descriptors.

use crate::read::{self, R};
use crate::{Error, Result};

pub const FORM_NONE: u8 = 0;
pub const FORM_RANGE: u8 = 1;
pub const FORM_ENUM: u8 = 2;

/// PTP data types we read. Anything else in a camera blob is an error, not a guess.
pub const TY_I8: u16 = 0x0001;
pub const TY_U8: u16 = 0x0002;
pub const TY_I16: u16 = 0x0003;
pub const TY_U16: u16 = 0x0004;
pub const TY_I32: u16 = 0x0005;
pub const TY_U32: u16 = 0x0006;
pub const TY_I64: u16 = 0x0007;
pub const TY_U64: u16 = 0x0008;
pub const TY_STR: u16 = 0xFFFF;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Prop {
    pub code: u16,
    pub datatype: u16,
    /// First descriptor byte. Bit 0 set means an absolute value (white balance on an A7 III).
    /// Bit 7 set means a button. Clear means the stop is stepped with the other setter.
    pub flags: u8,
    /// Second descriptor byte: 0 off, 1 the host may change it, 2 the camera's dial or auto mode owns it.
    pub availability: u8,
    pub writable: bool,
    pub enabled: bool,
    pub current: u64,
    pub enums: Vec<u64>,
    pub minimum: Option<u64>,
    pub maximum: Option<u64>,
    pub step: Option<u64>,
}

impl Prop {
    pub fn u16(code: u16, current: u16, enums: &[u16], writable: bool) -> Self {
        Self {
            code,
            datatype: TY_U16,
            flags: u8::from(writable),
            availability: 1,
            writable,
            enabled: true,
            current: u64::from(current),
            enums: enums.iter().map(|v| u64::from(*v)).collect(),
            minimum: None,
            maximum: None,
            step: None,
        }
    }

    pub fn u32(code: u16, current: u32, enums: &[u32], writable: bool) -> Self {
        Self {
            code,
            datatype: TY_U32,
            flags: u8::from(writable),
            availability: 1,
            writable,
            enabled: true,
            current: u64::from(current),
            enums: enums.iter().map(|v| u64::from(*v)).collect(),
            minimum: None,
            maximum: None,
            step: None,
        }
    }
}

/// Move to the neighbouring enumeration value. `dir` is -1 or +1. The ends stay put.
pub fn step_enum(prop: &Prop, dir: i32) -> Option<u64> {
    if prop.enums.is_empty() {
        let (min, max, step) = (prop.minimum?, prop.maximum?, prop.step?);
        if step == 0 {
            return None;
        }
        let next = if dir < 0 { prop.current.saturating_sub(step) } else { prop.current.saturating_add(step) };
        return Some(next.clamp(min, max));
    }
    let idx = prop.enums.iter().position(|v| *v == prop.current)?;
    let next = if dir < 0 { idx.checked_sub(1)? } else { idx.checked_add(1).filter(|i| *i < prop.enums.len())? };
    prop.enums.get(next).copied()
}

pub fn encode_props(props: &[Prop]) -> Result<Vec<u8>> {
    if props.len() > 512 {
        return Err(Error::new("too many device properties"));
    }
    let mut out = Vec::new();
    read::push_u32(&mut out, props.len() as u32);
    read::push_u32(&mut out, 0);
    for p in props {
        read::push_u16(&mut out, p.code);
        read::push_u16(&mut out, p.datatype);
        read::push_u8(&mut out, p.flags);
        read::push_u8(&mut out, p.availability);
        push_value(&mut out, p.datatype, p.current)?;
        push_value(&mut out, p.datatype, p.current)?;
        if !p.enums.is_empty() {
            read::push_u8(&mut out, FORM_ENUM);
            let n = u16::try_from(p.enums.len()).map_err(|_| Error::new("too many property values"))?;
            read::push_u16(&mut out, n);
            for v in &p.enums {
                push_value(&mut out, p.datatype, *v)?;
            }
        } else if let (Some(min), Some(max), Some(step)) = (p.minimum, p.maximum, p.step) {
            read::push_u8(&mut out, FORM_RANGE);
            push_value(&mut out, p.datatype, min)?;
            push_value(&mut out, p.datatype, max)?;
            push_value(&mut out, p.datatype, step)?;
        } else {
            read::push_u8(&mut out, FORM_NONE);
        }
    }
    Ok(out)
}

pub fn parse_props(data: &[u8]) -> Result<Vec<Prop>> {
    let mut r = R::new(data);
    let count = r.u32()?;
    if count > 512 {
        return Err(Error::new("device property list is unreasonably long"));
    }
    // The A7 III writes a zero word after the count. A list that ends at the count has no word.
    if r.remaining() >= 4 {
        let _marker = r.u32()?;
    }
    let mut props = Vec::with_capacity(count as usize);
    for _ in 0..count {
        props.push(parse_one(&mut r)?);
    }
    finish(&r)?;
    Ok(props)
}

fn parse_one(r: &mut R<'_>) -> Result<Prop> {
    let code = r.u16()?;
    let datatype = r.u16()?;
    let flags = r.u8()?;
    let availability = r.u8()?;
    // Bit 0 and the button bit are host-writable. Availability 2 is the camera's own dial, which
    // still accepts a step when the mode allows it; that decision is the session's.
    let writable = flags != 0;
    let enabled = availability != 0;
    let _factory = read_value(r, datatype)?;
    let current = read_value(r, datatype)?;
    let form = r.u8()?;
    let mut prop =
        Prop { code, datatype, flags, availability, writable, enabled, current, enums: Vec::new(), minimum: None, maximum: None, step: None };
    match form {
        FORM_NONE => {}
        FORM_RANGE => {
            prop.minimum = Some(read_value(r, datatype)?);
            prop.maximum = Some(read_value(r, datatype)?);
            prop.step = Some(read_value(r, datatype)?);
        }
        FORM_ENUM => {
            let n = r.u16()? as usize;
            if n > 4096 {
                return Err(Error::new("property enumeration is unreasonably long"));
            }
            for _ in 0..n {
                prop.enums.push(read_value(r, datatype)?);
            }
        }
        _ => return Err(Error::new(format!("unknown property form {form}"))),
    }
    Ok(prop)
}

fn finish(r: &R<'_>) -> Result<()> {
    if r.remaining() != 0 {
        return Err(Error::new(format!("{} trailing property bytes", r.remaining())));
    }
    Ok(())
}

fn read_value(r: &mut R<'_>, datatype: u16) -> Result<u64> {
    match datatype {
        TY_I8 | TY_U8 => Ok(u64::from(r.u8()?)),
        TY_I16 | TY_U16 => Ok(u64::from(r.u16()?)),
        TY_I32 | TY_U32 => Ok(u64::from(r.u32()?)),
        TY_I64 | TY_U64 => r.u64(),
        TY_STR => {
            let _ = r.ptp_string()?;
            Ok(0)
        }
        other => Err(Error::new(format!("unsupported property type {other:#06x}"))),
    }
}

pub fn encode_value(datatype: u16, value: u64) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    push_value(&mut out, datatype, value)?;
    Ok(out)
}

fn push_value(out: &mut Vec<u8>, datatype: u16, value: u64) -> Result<()> {
    match datatype {
        TY_I8 | TY_U8 => read::push_u8(out, u8::try_from(value & 0xff).unwrap_or(0)),
        TY_I16 | TY_U16 => read::push_u16(out, u16::try_from(value & 0xffff).unwrap_or(0)),
        TY_I32 | TY_U32 => read::push_u32(out, u32::try_from(value & 0xffff_ffff).unwrap_or(0)),
        TY_I64 | TY_U64 => read::push_u64(out, value),
        TY_STR => read::push_string(out, ""),
        other => return Err(Error::new(format!("unsupported property type {other:#06x}"))),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enumeration_round_trips_and_steps() {
        let prop = Prop::u16(0x5007, 400, &[280, 400, 560], true);
        let blob = encode_props(&[prop.clone()]).unwrap();
        let parsed = parse_props(&blob).unwrap();
        assert_eq!(parsed, vec![prop]);
        assert_eq!(step_enum(&parsed[0], 1), Some(560));
        assert_eq!(step_enum(&parsed[0], -1), Some(280));
        assert_eq!(step_enum(&parsed[0], 1).and_then(|_| Some(())), Some(()));
        let at_end = Prop { current: 560, ..parsed[0].clone() };
        assert_eq!(step_enum(&at_end, 1), None);
    }

    #[test]
    fn a_huge_count_is_refused() {
        let mut blob = vec![0xff, 0xff, 0, 0];
        blob.extend_from_slice(&[0; 8]);
        assert!(parse_props(&blob).is_err());
    }

    #[test]
    fn a_truncated_property_is_refused() {
        let mut blob = encode_props(&[Prop::u16(1, 1, &[1], false)]).unwrap();
        blob.pop();
        assert!(parse_props(&blob).is_err());
    }

    #[test]
    fn an_a7iii_property_list_parses() {
        let props = parse_props(include_bytes!("../testdata/a7iii-props.bin")).unwrap();
        assert_eq!(props.len(), 60);
        let aperture = props.iter().find(|p| p.code == 0x5007).unwrap();
        assert_eq!(aperture.current, 400);
        assert_eq!((aperture.flags, aperture.availability), (0, 2));
        let wb = props.iter().find(|p| p.code == 0x5005).unwrap();
        assert_eq!(wb.flags & 0x01, 0x01);
        let shutter = props.iter().find(|p| p.code == 0xD20D).unwrap();
        assert_eq!(shutter.current, 65_696);
        let iso = props.iter().find(|p| p.code == 0xD21E).unwrap();
        assert_eq!(iso.enums.len(), 89);
        let live = props.iter().find(|p| p.code == 0xD221).unwrap();
        assert_eq!(live.current, 1);
    }
}
