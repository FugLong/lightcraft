//! PTP containers (PIMA 15740): 4-byte length, 2-byte type, 2-byte code, 4-byte transaction,
//! then parameters. The length is never trusted to allocate past the buffer we already hold.

use crate::{Error, Result};

pub const KIND_COMMAND: u16 = 1;
pub const KIND_DATA: u16 = 2;

/// One container. Command and response parameters are `params`. A data container's bytes are `data`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub kind: u16,
    pub code: u16,
    pub transaction: u32,
    pub params: Vec<u32>,
    pub data: Vec<u8>,
}

/// A command container. At most five parameters, as the still-image class allows.
pub fn encode_command(code: u16, transaction: u32, params: &[u32]) -> Result<Vec<u8>> {
    if params.len() > 5 {
        return Err(Error::new("PTP command has more than five parameters"));
    }
    let len = 12usize.saturating_add(params.len().saturating_mul(4));
    let mut out = Vec::with_capacity(len);
    out.extend_from_slice(&u32::try_from(len).unwrap_or(0).to_le_bytes());
    out.extend_from_slice(&KIND_COMMAND.to_le_bytes());
    out.extend_from_slice(&code.to_le_bytes());
    out.extend_from_slice(&transaction.to_le_bytes());
    for p in params {
        out.extend_from_slice(&p.to_le_bytes());
    }
    Ok(out)
}

/// Decode one container. `buf` may be longer than the container; bytes after `length` are ignored.
/// A length that is shorter than the header, or longer than `buf`, is an error.
pub fn decode(buf: &[u8]) -> Result<Packet> {
    if buf.len() < 12 {
        return Err(Error::new("PTP packet is shorter than a header"));
    }
    let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if len < 12 {
        return Err(Error::new("PTP packet length is shorter than a header"));
    }
    if len > buf.len() {
        return Err(Error::new("PTP packet length runs past the buffer"));
    }
    // A single container from the camera. Refuse a header that claims to be enormous relative
    // to a command or response (those carry at most five parameters). Data containers may be a
    // raw file; the caller passes that buffer separately, so a data container here is capped too.
    if len > 12 + 5 * 4 && kind_of(buf) != KIND_DATA {
        return Err(Error::new("PTP command or response is unreasonably long"));
    }
    let kind = u16::from_le_bytes([buf[4], buf[5]]);
    let code = u16::from_le_bytes([buf[6], buf[7]]);
    let transaction = u32::from_le_bytes([buf[8], buf[9], buf[10], buf[11]]);
    let rest = buf.get(12..len).ok_or_else(|| Error::new("PTP packet ended early"))?;
    if kind == KIND_DATA {
        return Ok(Packet { kind, code, transaction, params: Vec::new(), data: rest.to_vec() });
    }
    if rest.len() % 4 != 0 {
        return Err(Error::new("PTP parameters are not a whole number of words"));
    }
    if rest.len() / 4 > 5 {
        return Err(Error::new("PTP packet has more than five parameters"));
    }
    let mut params = Vec::with_capacity(rest.len() / 4);
    for chunk in rest.chunks(4) {
        let c = chunk.get(..4).ok_or_else(|| Error::new("PTP parameter ended early"))?;
        params.push(u32::from_le_bytes([c[0], c[1], c[2], c[3]]));
    }
    Ok(Packet { kind, code, transaction, params, data: Vec::new() })
}

fn kind_of(buf: &[u8]) -> u16 {
    buf.get(4..6).map(|b| u16::from_le_bytes([b[0], b[1]])).unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_round_trips() {
        let buf = encode_command(0x1002, 7, &[1]).unwrap();
        let p = decode(&buf).unwrap();
        assert_eq!(p.kind, KIND_COMMAND);
        assert_eq!(p.code, 0x1002);
        assert_eq!(p.transaction, 7);
        assert_eq!(p.params, vec![1]);
    }

    #[test]
    fn short_buffer_is_an_error() {
        assert!(decode(&[0, 1, 2]).is_err());
    }

    #[test]
    fn length_past_the_buffer_is_an_error() {
        let mut buf = encode_command(0x1001, 1, &[]).unwrap();
        buf[0] = 0xff;
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn length_shorter_than_a_header_is_an_error() {
        let mut buf = encode_command(0x1001, 1, &[]).unwrap();
        buf[0] = 4;
        assert!(decode(&buf).is_err());
    }

    #[test]
    fn extra_trailing_bytes_are_ignored() {
        let mut buf = encode_command(0x1001, 1, &[]).unwrap();
        buf.push(0xab);
        assert_eq!(decode(&buf).unwrap().code, 0x1001);
    }
}
