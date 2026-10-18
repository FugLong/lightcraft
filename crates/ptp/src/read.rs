//! A cursor over camera bytes. Every read checks the end; nothing here indexes with a number
//! that came from the buffer.

use crate::{Error, Result};

pub struct R<'a> {
    buf: &'a [u8],
    i: usize,
}

impl<'a> R<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Self { buf, i: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len().saturating_sub(self.i)
    }

    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.i.checked_add(n).ok_or_else(|| Error::new("PTP read overflowed"))?;
        let slice = self.buf.get(self.i..end).ok_or_else(|| Error::new("PTP packet ended early"))?;
        self.i = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16> {
        let b = self.take(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }

    pub fn u32(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    pub fn u64(&mut self) -> Result<u64> {
        let b = self.take(8)?;
        let mut a = [0u8; 8];
        a.copy_from_slice(b);
        Ok(u64::from_le_bytes(a))
    }

    /// A PTP string: one byte counting UTF-16 code units, including the trailing NUL.
    /// A count of 0 is the empty string. A count that runs past the buffer is an error.
    pub fn ptp_string(&mut self) -> Result<String> {
        let n = self.u8()? as usize;
        if n == 0 {
            return Ok(String::new());
        }
        if n > 4096 {
            return Err(Error::new("PTP string is unreasonably long"));
        }
        let bytes = self.take(n.saturating_mul(2))?;
        let mut units = Vec::with_capacity(n);
        for chunk in bytes.chunks(2) {
            let pair = chunk.get(..2).ok_or_else(|| Error::new("PTP string ended mid-character"))?;
            units.push(u16::from_le_bytes([pair[0], pair[1]]));
        }
        let end = units.iter().position(|u| *u == 0).unwrap_or(units.len());
        Ok(String::from_utf16_lossy(units.get(..end).unwrap_or(&[])))
    }
}

pub fn push_u8(out: &mut Vec<u8>, v: u8) {
    out.push(v);
}

pub fn push_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

pub fn push_u64(out: &mut Vec<u8>, v: u64) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Writes a PTP string, truncating to 254 characters so the length byte stays honest.
pub fn push_string(out: &mut Vec<u8>, s: &str) {
    let mut units: Vec<u16> = s.encode_utf16().take(254).collect();
    units.push(0);
    let n = u8::try_from(units.len()).unwrap_or(255);
    let n = n.min(units.len() as u8);
    out.push(n);
    for u in units.iter().take(n as usize) {
        push_u16(out, *u);
    }
}

pub fn push_u16_array(out: &mut Vec<u8>, values: &[u16]) {
    let n = u32::try_from(values.len()).unwrap_or(u32::MAX);
    push_u32(out, n);
    for v in values.iter().take(n as usize) {
        push_u16(out, *v);
    }
}
