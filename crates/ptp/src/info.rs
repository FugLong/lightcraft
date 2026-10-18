//! Standard PTP datasets: GetDeviceInfo and the file name inside ObjectInfo.

use crate::read::{self, R};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub manufacturer: String,
    pub model: String,
    pub version: String,
    pub serial: String,
}

/// Build a GetDeviceInfo payload. Arrays are empty; the strings are what a session shows.
pub fn device_info(manufacturer: &str, model: &str) -> Vec<u8> {
    let mut out = Vec::new();
    read::push_u16(&mut out, 100);
    read::push_u32(&mut out, 0);
    read::push_u16(&mut out, 100);
    read::push_string(&mut out, "");
    read::push_u16(&mut out, 0);
    for _ in 0..5 {
        read::push_u16_array(&mut out, &[]);
    }
    read::push_string(&mut out, manufacturer);
    read::push_string(&mut out, model);
    read::push_string(&mut out, "1.00");
    read::push_string(&mut out, "0");
    out
}

pub fn parse_device_info(data: &[u8]) -> Result<DeviceInfo> {
    let mut r = R::new(data);
    let _standard = r.u16()?;
    let _vendor = r.u32()?;
    let _vendor_version = r.u16()?;
    let _vendor_desc = r.ptp_string()?;
    let _mode = r.u16()?;
    for _ in 0..5 {
        skip_u16_array(&mut r)?;
    }
    let manufacturer = r.ptp_string()?;
    let model = r.ptp_string()?;
    let version = r.ptp_string()?;
    let serial = r.ptp_string()?;
    Ok(DeviceInfo { manufacturer, model, version, serial })
}

fn skip_u16_array(r: &mut R<'_>) -> Result<()> {
    let n = r.u32()?;
    if n > 4096 {
        return Err(Error::new("PTP array is unreasonably long"));
    }
    for _ in 0..n {
        let _ = r.u16()?;
    }
    Ok(())
}

/// ObjectInfo is 52 bytes of fixed fields, then the file name.
pub fn object_info(filename: &str, bytes: u32) -> Vec<u8> {
    let mut out = vec![0u8; 52];
    if let Some(slot) = out.get_mut(8..12) {
        slot.copy_from_slice(&bytes.to_le_bytes());
    }
    read::push_string(&mut out, filename);
    read::push_string(&mut out, "");
    read::push_string(&mut out, "");
    read::push_string(&mut out, "");
    out
}

pub fn object_filename(data: &[u8]) -> Result<String> {
    let mut r = R::new(data);
    if r.remaining() < 52 {
        return Err(Error::new("object info is shorter than its fixed fields"));
    }
    let _ = r.take_fixed(52)?;
    r.ptp_string()
}

impl<'a> R<'a> {
    pub fn take_fixed(&mut self, n: usize) -> Result<&'a [u8]> {
        // Re-read through the public cursor by pulling bytes one chunk at a time via u8 when
        // n is the known 52-byte object header. Done here so the cursor stays the only indexer.
        let mut skipped = 0;
        while skipped < n {
            let _ = self.u8()?;
            skipped += 1;
        }
        Ok(&[])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn device_info_names_round_trip() {
        let info = parse_device_info(&device_info("Sony", "ILCE-7M3")).unwrap();
        assert_eq!(info.manufacturer, "Sony");
        assert_eq!(info.model, "ILCE-7M3");
    }

    #[test]
    fn a_huge_operation_array_is_refused() {
        let mut blob = Vec::new();
        read::push_u16(&mut blob, 100);
        read::push_u32(&mut blob, 0);
        read::push_u16(&mut blob, 100);
        read::push_string(&mut blob, "");
        read::push_u16(&mut blob, 0);
        read::push_u32(&mut blob, 100_000);
        assert!(parse_device_info(&blob).is_err());
    }

    #[test]
    fn an_a7iii_device_info_names_the_body() {
        let info = parse_device_info(include_bytes!("../testdata/a7iii-info.bin")).unwrap();
        assert_eq!(info.manufacturer, "Sony Corporation");
        assert_eq!(info.model, "ILCE-7M3");
        assert_eq!(info.version, "4.0");
        assert!(!info.serial.is_empty());
    }

    #[test]
    fn object_info_keeps_the_file_name() {
        let name = object_filename(&object_info("DSC00001.ARW", 12)).unwrap();
        assert_eq!(name, "DSC00001.ARW");
    }
}
