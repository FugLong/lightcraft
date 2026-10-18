//! Parse a dump from `cargo run -p lightcraft-cameras --example probe`.
//!
//! `cargo run -p lightcraft-ptp --example dump -- /path/to/lightcraft-tether`

use lightcraft_ptp::{jpeg_payload, parse_device_info, parse_props};

fn main() {
    let dir = std::env::args().nth(1).unwrap_or_else(|| "missing directory".into());
    let info = std::fs::read(format!("{dir}/info.bin")).expect("info");
    match parse_device_info(&info) {
        Ok(info) => {
            println!("device {} {} {} serial {}", info.manufacturer, info.model, info.version, info.serial);
        }
        Err(e) => println!("device info: {e} ({} bytes)", info.len()),
    }
    let props = std::fs::read(format!("{dir}/props.bin")).expect("props");
    match parse_props(&props) {
        Ok(list) => {
            println!("{} properties", list.len());
            for code in [0x5005u16, 0x5007, 0xD20D, 0xD21E, 0xD215, 0xD221, 0xD222] {
                match list.iter().find(|p| p.code == code) {
                    Some(p) => println!(
                        "  {code:#06x} type {:#06x} current {} writable {} enabled {} values {} range {:?}",
                        p.datatype, p.current, p.writable, p.enabled, p.enums.len(), p.minimum.zip(p.maximum)
                    ),
                    None => println!("  {code:#06x} absent"),
                }
            }
        }
        Err(e) => println!("props: {e} ({} bytes, head {})", props.len(), hex(&props[..props.len().min(32)])),
    }
    let live = std::fs::read(format!("{dir}/live.bin")).expect("live");
    match jpeg_payload(&live) {
        Ok(jpeg) => println!("jpeg {} bytes, starts {:02x}{:02x}", jpeg.len(), jpeg[0], jpeg[1]),
        Err(e) => println!("jpeg: {e}"),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}
