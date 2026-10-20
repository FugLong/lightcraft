//! Manual check for a Sony body already in PC Remote.
//!
//! `cargo run -p lightcraft-cameras --example probe`
//!
//! Prints the cameras Image Capture can see, and, for a Sony, the raw bytes of Open Session and
//! Get Device Info. It does not import a photo.

fn main() {
    #[cfg(not(target_os = "macos"))]
    {
        eprintln!("this probe only runs on macOS");
        return;
    }
    #[cfg(target_os = "macos")]
    if let Err(e) = mac::probe() {
        eprintln!("{e}");
        std::process::exit(1);
    }
}

#[cfg(target_os = "macos")]
mod mac {
    use std::time::{Duration, Instant};

    use lightcraft_cameras::{CameraInfo, Client, link};

    pub fn probe() -> Result<(), String> {
        let (client, mut endpoint) = link();
        let worker =
            std::thread::Builder::new().name("probe".into()).spawn(move || talk(client)).map_err(|e| format!("couldn't start the probe: {e}"))?;
        let start = Instant::now();
        while !worker.is_finished() && start.elapsed() < Duration::from_secs(70) {
            endpoint.spin();
        }
        if !worker.is_finished() {
            return Err("the camera did not answer within 30 seconds".into());
        }
        match worker.join() {
            Ok(result) => result,
            Err(_) => Err("the probe stopped unexpectedly".into()),
        }
    }

    fn talk(client: Client) -> Result<(), String> {
        let mut list = Vec::new();
        for _ in 0..80 {
            list = client.list(Duration::from_secs(2))?;
            if !list.is_empty() {
                break;
            }
        }
        if list.is_empty() {
            println!("no camera. On the body: USB Connection → PC Remote.");
            return Ok(());
        }
        for cam in &list {
            println!("camera: {} vendor {:#06x} id {}", cam.name, cam.vendor, cam.id);
        }
        let Some(sony) = list.iter().find(|c| c.vendor == 0x054C || is_sony(&c.name)) else {
            println!("none of those is a Sony");
            return Ok(());
        };
        open_and_ask(&client, sony)
    }

    fn open_and_ask(client: &Client, sony: &CameraInfo) -> Result<(), String> {
        client.open(Some(sony.id.clone()), Duration::from_secs(8))?;
        println!("session open");
        let mut txn = 1u32;
        show(client, "OpenSession", ptp(0x1002, txn, &[1]), None)?;
        txn += 1;
        let _info = show(client, "GetDeviceInfo", ptp(0x1001, txn, &[]), None)?;
        txn += 1;
        show(client, "Connect 1", ptp(0x9201, txn, &[1, 0, 0]), None)?;
        txn += 1;
        show(client, "Connect 2", ptp(0x9201, txn, &[2, 0, 0]), None)?;
        txn += 1;
        let ext = show(client, "ExtDeviceInfo", ptp(0x9202, txn, &[0x00c8]), None)?;
        txn += 1;
        show(client, "Connect 3", ptp(0x9201, txn, &[3, 0, 0]), None)?;
        txn += 1;
        let props = show(client, "AllProps", ptp(0x9209, txn, &[]), None)?;
        println!("property blob {} bytes", props.len());
        txn += 1;
        let info = show(client, "LiveViewInfo", ptp(0x1008, txn, &[0xFFFF_C002]), None)?;
        println!("live view info {} bytes {}", info.len(), hex(&info));
        txn += 1;
        let live = show(client, "LiveView", ptp(0x1009, txn, &[0xFFFF_C002]), None)?;
        let jpeg = live.windows(2).position(|w| w == [0xFF, 0xD8]);
        println!("live view {} bytes, jpeg at {:?}", live.len(), jpeg);
        let dir = std::env::temp_dir().join("lightcraft-tether");
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("info.bin"), &info).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("ext.bin"), &ext).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("props.bin"), &props).map_err(|e| e.to_string())?;
        std::fs::write(dir.join("live.bin"), &live).map_err(|e| e.to_string())?;
        println!("wrote {}", dir.display());
        client.close(Duration::from_secs(5))?;
        println!("session closed");
        Ok(())
    }

    fn show(client: &Client, name: &str, cmd: Vec<u8>, data: Option<Vec<u8>>) -> Result<Vec<u8>, String> {
        let (response, incoming) = client.command(cmd, data, Duration::from_secs(8))?;
        println!("{name}: code {} response {} bytes, data {} bytes", response_code(&response), response.len(), incoming.len());
        if response.len() <= 32 {
            println!("  response {}", hex(&response));
        }
        if !incoming.is_empty() && incoming.len() <= 32 {
            println!("  data {}", hex(&incoming));
        }
        Ok(incoming)
    }

    fn response_code(response: &[u8]) -> String {
        if response.len() < 8 {
            return "short".into();
        }
        format!("{:#06x}", u16::from_le_bytes([response[6], response[7]]))
    }

    fn is_sony(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        n.contains("sony") || n.contains("ilce")
    }

    fn ptp(code: u16, txn: u32, params: &[u32]) -> Vec<u8> {
        let len = 12usize.saturating_add(params.len().saturating_mul(4));
        let mut out = Vec::with_capacity(len);
        out.extend_from_slice(&u32::try_from(len).unwrap_or(0).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&txn.to_le_bytes());
        for p in params {
            out.extend_from_slice(&p.to_le_bytes());
        }
        out
    }

    fn hex(bytes: &[u8]) -> String {
        const MAX: usize = 64;
        let shown = bytes.len().min(MAX);
        let mut s = String::new();
        for (i, b) in bytes.iter().take(shown).enumerate() {
            if i > 0 {
                s.push(' ');
            }
            s.push_str(&format!("{b:02x}"));
        }
        if bytes.len() > MAX {
            s.push_str(" …");
        }
        s
    }
}
