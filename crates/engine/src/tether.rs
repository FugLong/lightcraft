//! Tethered capture for a Sony body in PC Remote.
//!
//! The desktop app pumps [`lightcraft_cameras::Endpoint`] on the UI thread. A worker thread speaks
//! Sony PTP 2 and pushes stills here; the pump imports them. Tests install a scripted camera and
//! never touch USB.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
use std::time::Duration;

use lightcraft_catalog::PhotoId;
#[cfg(any(test, all(target_os = "macos", not(target_arch = "wasm32"))))]
use lightcraft_ptp::Error as PtpError;
#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
use lightcraft_ptp::Reply;
#[cfg(test)]
use lightcraft_ptp::Sim;
use lightcraft_ptp::{Captured, Dial, SonySession, Transport};
use serde_json::{Value, json};

use crate::activity::Activity;
#[cfg(any(test, all(target_os = "macos", not(target_arch = "wasm32"))))]
use crate::activity::{Cancel, TaskGuard};
use crate::import::{ImportMode, ImportOptions, import_with};
use crate::view::Selection;
use crate::{EngineError, Result, Session};

const SONY: u16 = 0x054C;

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

#[cfg(test)]
fn ptp_err(e: PtpError) -> EngineError {
    EngineError::Other(e.message().to_string())
}

#[derive(Clone)]
enum Ask {
    Shoot,
    Focus,
    Dial { dial: Dial, step: i32, absolute: Option<u64> },
    Magnify(bool),
    Nudge(String),
}

#[derive(Default)]
struct Inner {
    connected: bool,
    model: String,
    aperture: String,
    shutter: String,
    iso: String,
    white_balance: Option<String>,
    exposure_comp: String,
    aperture_choices: Vec<(u64, String)>,
    shutter_choices: Vec<(u64, String)>,
    iso_choices: Vec<(u64, String)>,
    white_balance_choices: Vec<(u64, String)>,
    magnified: bool,
    hint: String,
    error: Option<String>,
    generation: u64,
    width: u32,
    height: u32,
    rgba: Option<Arc<Vec<u8>>>,
    imports: Vec<Captured>,
    asks: Vec<Ask>,
}

/// What the loupe draws while a camera is connected.
#[derive(Clone)]
pub struct LiveFrame {
    pub generation: u64,
    pub width: u32,
    pub height: u32,
    pub rgba: Arc<Vec<u8>>,
}

/// Read by the tether bar.
#[derive(Clone, Debug)]
pub struct TetherStatus {
    pub running: bool,
    pub connected: bool,
    pub model: String,
    pub aperture: String,
    pub shutter: String,
    pub iso: String,
    pub white_balance: Option<String>,
    /// Exposure compensation dial, such as `+0.0`. Empty when the body has no such property.
    pub exposure_comp: String,
    pub aperture_choices: Vec<(u64, String)>,
    pub shutter_choices: Vec<(u64, String)>,
    pub iso_choices: Vec<(u64, String)>,
    pub white_balance_choices: Vec<(u64, String)>,
    pub magnified: bool,
    pub hint: String,
    pub error: Option<String>,
}

/// The tether session the UI and the commands share.
pub struct Hub {
    shared: Arc<Mutex<Inner>>,
    cancel: Arc<AtomicBool>,
    client: lightcraft_cameras::Client,
    endpoint: lightcraft_cameras::Endpoint,
    worker: Option<std::thread::JoinHandle<()>>,
    #[cfg(test)]
    guard: Option<TaskGuard>,
    preset: Option<String>,
    #[cfg(test)]
    script: Option<Sim>,
    #[cfg(test)]
    camera: Option<SonySession<Sim>>,
}

impl Hub {
    pub fn new() -> Self {
        let (client, endpoint) = lightcraft_cameras::link();
        Self {
            shared: Arc::new(Mutex::new(Inner::default())),
            cancel: Arc::new(AtomicBool::new(false)),
            client,
            endpoint,
            worker: None,
            #[cfg(test)]
            guard: None,
            preset: None,
            #[cfg(test)]
            script: None,
            #[cfg(test)]
            camera: None,
        }
    }

    /// Tests drive this camera instead of Image Capture.
    #[cfg(test)]
    pub fn install_script(&mut self, sim: Sim) {
        self.script = Some(sim);
    }

    pub fn preset_id(&self) -> Option<String> {
        self.preset.clone()
    }

    pub fn is_running(&self) -> bool {
        #[cfg(test)]
        if self.camera.is_some() {
            return true;
        }
        self.worker.as_ref().is_some_and(|h| !h.is_finished())
    }

    pub fn status(&self) -> TetherStatus {
        let g = lock(&self.shared);
        TetherStatus {
            running: self.is_running(),
            connected: g.connected && self.is_running(),
            model: g.model.clone(),
            aperture: g.aperture.clone(),
            shutter: g.shutter.clone(),
            iso: g.iso.clone(),
            white_balance: g.white_balance.clone(),
            exposure_comp: g.exposure_comp.clone(),
            aperture_choices: g.aperture_choices.clone(),
            shutter_choices: g.shutter_choices.clone(),
            iso_choices: g.iso_choices.clone(),
            white_balance_choices: g.white_balance_choices.clone(),
            magnified: g.magnified,
            hint: g.hint.clone(),
            error: g.error.clone(),
        }
    }

    pub fn frame(&self) -> Option<LiveFrame> {
        let g = lock(&self.shared);
        let rgba = g.rgba.clone()?;
        if !self.is_running() || g.width == 0 || g.height == 0 {
            return None;
        }
        Some(LiveFrame { generation: g.generation, width: g.width, height: g.height, rgba })
    }

    pub fn begin(&mut self, activity: Activity, preset: Option<String>) -> Result<()> {
        if self.is_running() {
            return Err(EngineError::Other("tethered capture is already running".into()));
        }
        self.reap();
        self.preset = preset;
        self.cancel.store(false, Ordering::Relaxed);
        {
            let mut g = lock(&self.shared);
            *g = Inner::default();
        }
        #[cfg(test)]
        if self.script.is_some() {
            return self.begin_script(activity);
        }
        self.begin_live(activity)
    }

    pub fn stop(&mut self) {
        self.cancel.store(true, Ordering::Relaxed);
        if !self.is_running() {
            lock(&self.shared).error = None;
        }
        #[cfg(test)]
        if self.camera.is_some() {
            if let Some(mut cam) = self.camera.take() {
                let _ = cam.close();
            }
            self.guard = None;
            let mut g = lock(&self.shared);
            g.connected = false;
        }
    }

    /// Stop, keep pumping Image Capture until the camera session has closed, then return.
    /// Quitting has to call this on the main thread: the worker cannot finish the close by itself.
    pub fn finish(&mut self, timeout: std::time::Duration) {
        self.stop();
        let start = std::time::Instant::now();
        while start.elapsed() < timeout && (self.is_running() || !self.endpoint.settled()) {
            self.endpoint.spin();
        }
        self.endpoint.spin();
        self.reap();
    }

    pub fn queue_shoot(&mut self) -> Result<()> {
        self.queue(Ask::Shoot)
    }

    pub fn queue_focus(&mut self) -> Result<()> {
        self.queue(Ask::Focus)
    }

    pub fn queue_dial(&mut self, dial: Dial, step: i32, absolute: Option<u64>) -> Result<()> {
        self.queue(Ask::Dial { dial, step, absolute })
    }

    pub fn queue_magnify(&mut self, on: bool) -> Result<()> {
        self.queue(Ask::Magnify(on))
    }

    pub fn queue_nudge(&mut self, direction: &str) -> Result<()> {
        match direction {
            "up" | "down" | "left" | "right" => self.queue(Ask::Nudge(direction.to_string())),
            _ => Err(EngineError::Other("magnifier move is up, down, left, or right".into())),
        }
    }

    /// A scripted camera shoots on the calling thread. A real camera returns `None` and the
    /// command queues the shutter instead.
    pub fn take_script_capture(&mut self) -> Result<Option<Captured>> {
        #[cfg(test)]
        if self.camera.is_some() {
            return self.script_capture().map(Some);
        }
        Ok(None)
    }

    /// A scripted camera changes the dial immediately.
    pub fn apply_script_dial(&mut self, dial: Dial, step: i32, absolute: Option<u64>) -> Result<bool> {
        #[cfg(test)]
        if self.camera.is_some() {
            self.script_dial(dial, step, absolute)?;
            return Ok(true);
        }
        let _ = (dial, step, absolute);
        Ok(false)
    }

    pub fn apply_script_magnify(&mut self, on: bool) -> Result<bool> {
        #[cfg(test)]
        if self.camera.is_some() {
            self.script_magnify(on)?;
            return Ok(true);
        }
        let _ = on;
        Ok(false)
    }

    pub fn apply_script_nudge(&mut self, direction: &str) -> Result<bool> {
        #[cfg(test)]
        if self.camera.is_some() {
            self.script_nudge(direction)?;
            return Ok(true);
        }
        let _ = direction;
        Ok(false)
    }

    pub fn apply_script_focus(&mut self) -> Result<bool> {
        #[cfg(test)]
        if self.camera.is_some() {
            self.script_focus()?;
            return Ok(true);
        }
        Ok(false)
    }

    pub fn note_error(&mut self, message: String) {
        lock(&self.shared).error = Some(message);
    }

    /// One Image Capture call. The frame loop calls this; it does not block.
    pub fn service(&mut self) {
        self.endpoint.pump();
    }

    pub fn take_imports(&mut self) -> Vec<Captured> {
        std::mem::take(&mut lock(&self.shared).imports)
    }

    pub fn reap(&mut self) {
        if self.worker.as_ref().is_some_and(|h| h.is_finished())
            && let Some(handle) = self.worker.take()
        {
            let _ = handle.join();
        }
    }

    fn queue(&mut self, ask: Ask) -> Result<()> {
        if !self.is_running() {
            return Err(EngineError::Other("start tethered capture first".into()));
        }
        lock(&self.shared).asks.push(ask);
        Ok(())
    }

    #[cfg(test)]
    fn begin_script(&mut self, activity: Activity) -> Result<()> {
        let sim = self.script.take().ok_or_else(|| EngineError::Other("no scripted camera".into()))?;
        let mut cam = SonySession::open(sim).map_err(ptp_err)?;
        if let Some(jpeg) = cam.live_jpeg().map_err(ptp_err)? {
            self.store_jpeg(&jpeg);
        }
        remember(&self.shared, &cam);
        self.camera = Some(cam);
        self.guard = Some(activity.start("tether", "Tethered Capture", Cancel::Flag(self.cancel.clone())));
        Ok(())
    }

    #[cfg(test)]
    fn script_capture(&mut self) -> Result<Captured> {
        let file = {
            let cam = self.camera.as_mut().ok_or_else(|| EngineError::Other("start tethered capture first".into()))?;
            cam.capture().map_err(ptp_err)?
        };
        if let Some(cam) = self.camera.as_ref() {
            remember(&self.shared, cam);
        }
        Ok(file)
    }

    #[cfg(test)]
    fn script_dial(&mut self, dial: Dial, step: i32, absolute: Option<u64>) -> Result<()> {
        {
            let cam = self.camera.as_mut().ok_or_else(|| EngineError::Other("start tethered capture first".into()))?;
            cam.set_dial(dial, step, absolute).map_err(ptp_err)?;
        }
        if let Some(cam) = self.camera.as_ref() {
            remember(&self.shared, cam);
        }
        Ok(())
    }

    #[cfg(test)]
    fn script_magnify(&mut self, on: bool) -> Result<()> {
        {
            let cam = self.camera.as_mut().ok_or_else(|| EngineError::Other("start tethered capture first".into()))?;
            cam.magnify(on).map_err(ptp_err)?;
        }
        if let Some(cam) = self.camera.as_ref() {
            remember(&self.shared, cam);
        }
        Ok(())
    }

    #[cfg(test)]
    fn script_nudge(&mut self, direction: &str) -> Result<()> {
        {
            let cam = self.camera.as_mut().ok_or_else(|| EngineError::Other("start tethered capture first".into()))?;
            cam.nudge(direction).map_err(ptp_err)?;
        }
        if let Some(cam) = self.camera.as_ref() {
            remember(&self.shared, cam);
        }
        Ok(())
    }

    #[cfg(test)]
    fn script_focus(&mut self) -> Result<()> {
        {
            let cam = self.camera.as_mut().ok_or_else(|| EngineError::Other("start tethered capture first".into()))?;
            cam.autofocus().map_err(ptp_err)?;
        }
        if let Some(cam) = self.camera.as_ref() {
            remember(&self.shared, cam);
        }
        Ok(())
    }

    #[cfg(not(all(target_os = "macos", not(target_arch = "wasm32"))))]
    fn begin_live(&mut self, _activity: Activity) -> Result<()> {
        Err(EngineError::Other("tethered capture on this system isn't available yet. It needs macOS and a Sony camera in PC Remote.".into()))
    }

    #[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
    fn begin_live(&mut self, activity: Activity) -> Result<()> {
        let client = self.client.clone();
        let shared = Arc::clone(&self.shared);
        let cancel = Arc::clone(&self.cancel);
        let guard = activity.start("tether", "Tethered Capture", Cancel::Flag(Arc::clone(&cancel)));
        let handle = std::thread::Builder::new()
            .name("lightcraft-tether".into())
            .spawn(move || worker(client, shared, cancel, guard))
            .map_err(|e| EngineError::Other(format!("couldn't start tethered capture: {e}")))?;
        self.worker = Some(handle);
        Ok(())
    }

    #[cfg(test)]
    fn store_jpeg(&mut self, jpeg: &[u8]) {
        if let Some((w, h, rgba)) = decode_live(jpeg) {
            let mut g = lock(&self.shared);
            g.generation = g.generation.saturating_add(1);
            g.width = w;
            g.height = h;
            g.rgba = Some(Arc::new(rgba));
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

fn remember<T: Transport>(shared: &Mutex<Inner>, cam: &SonySession<T>) {
    let mut g = lock(shared);
    g.connected = true;
    g.model = cam.model().to_string();
    g.aperture = cam.text(Dial::Aperture).unwrap_or_default();
    g.shutter = cam.text(Dial::Shutter).unwrap_or_default();
    g.iso = cam.text(Dial::Iso).unwrap_or_default();
    g.white_balance = if cam.has_white_balance() { cam.text(Dial::WhiteBalance) } else { None };
    g.exposure_comp = cam.exposure_comp().unwrap_or_default();
    g.aperture_choices = cam.choices(Dial::Aperture);
    g.shutter_choices = cam.choices(Dial::Shutter);
    g.iso_choices = cam.choices(Dial::Iso);
    g.white_balance_choices = if cam.has_white_balance() { cam.choices(Dial::WhiteBalance) } else { Vec::new() };
    g.magnified = cam.magnified;
    g.hint = cam.hint.clone();
}

/// Import one captured still and select it. Returns the new photo id.
pub fn import_capture(s: &mut Session, file: &Captured, preset_id: Option<&str>) -> Result<u64> {
    let name = safe_name(&file.name);
    let dir = std::env::temp_dir().join("lightcraft-tether");
    std::fs::create_dir_all(&dir).map_err(|e| EngineError::Other(format!("couldn't save the capture: {e}")))?;
    let millis = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
    let path = dir.join(format!("{millis}-{name}"));
    std::fs::write(&path, &file.bytes).map_err(|e| EngineError::Other(format!("couldn't save the capture: {e}")))?;
    let preset = match preset_id {
        Some(id) => match s.presets.iter().find(|p| p.id == id).cloned() {
            Some(p) => Some(p),
            None => {
                let _ = std::fs::remove_file(&path);
                return Err(EngineError::Other(format!("no preset named {id}")));
            }
        },
        None => None,
    };
    let mode = if s.library.as_ref().is_some_and(|l| l.on_disk) { ImportMode::Copy } else { ImportMode::Add };
    let opts = ImportOptions { mode, preset, ..ImportOptions::default() };
    let report = import_with(s, &[path.display().to_string()], &opts)?;
    if mode == ImportMode::Copy {
        let _ = std::fs::remove_file(&path);
    }
    let Some(&id) = report.imported.last() else {
        let why = report.failed.first().map(|(_, e)| e.clone()).unwrap_or_else(|| "the capture didn't import".into());
        return Err(EngineError::Other(why));
    };
    s.selection = Selection::single(PhotoId(id));
    Ok(id)
}

/// Pump the camera link and import stills the worker has downloaded.
pub fn pump(s: &mut Session) -> bool {
    s.tether.service();
    let preset = s.tether.preset_id();
    let files = s.tether.take_imports();
    for file in files {
        if let Err(e) = import_capture(s, &file, preset.as_deref()) {
            s.tether.note_error(e.to_string());
        }
    }
    s.tether.reap();
    s.tether.is_running()
}

fn safe_name(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or(name).trim();
    let cleaned: String = base.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')).take(80).collect();
    if cleaned.is_empty() || cleaned.starts_with('.') { "capture.arw".into() } else { cleaned }
}

fn decode_live(jpeg: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    let decoded = lightcraft_codecs::decode(jpeg, lightcraft_codecs::DecodeOptions::fit(1600, 1200)).ok()?;
    if decoded.width == 0 || decoded.height == 0 {
        return None;
    }
    let rgba = decoded.to_srgb8();
    let bytes = rgba.as_bytes();
    let need = (decoded.width as usize).saturating_mul(decoded.height as usize).saturating_mul(4);
    if bytes.len() != need {
        return None;
    }
    Some((decoded.width, decoded.height, bytes))
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn worker(client: lightcraft_cameras::Client, shared: Arc<Mutex<Inner>>, cancel: Arc<AtomicBool>, guard: TaskGuard) {
    let opened = AtomicBool::new(false);
    if let Err(e) = worker_session(&client, &shared, &cancel, guard, &opened)
        && !cancel.load(Ordering::Relaxed)
    {
        lock(&shared).error = Some(e);
    }
    {
        let mut g = lock(&shared);
        g.connected = false;
        g.asks.clear();
    }
    // A search that never opened a session must leave the browser up. Stopping it
    // throws away the camera Image Capture was about to report.
    if opened.load(Ordering::Relaxed) {
        let _ = client.close(Duration::from_secs(5));
    }
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn worker_session(client: &lightcraft_cameras::Client, shared: &Mutex<Inner>, cancel: &AtomicBool, guard: TaskGuard, opened: &AtomicBool) -> std::result::Result<(), String> {
    let cam = find_sony(client, cancel)?;
    if cancel.load(Ordering::Relaxed) {
        return Ok(());
    }
    client.open(Some(cam.id), Duration::from_secs(20))?;
    opened.store(true, Ordering::Relaxed);
    let mut session = SonySession::open(Bridge { client: client.clone(), txn: 1, budget_ms: 8_000 }).map_err(|e| e.message().to_string())?;
    // The handshake snapshot can lag the dials. One more read before the bar is drawn.
    session.set_budget(3_000);
    let _ = session.refresh();
    remember(shared, &session);
    guard.detail(session.model());
    // Connected tether is the bar on the loupe. Leaving this row up looks like a job that never finishes.
    drop(guard);
    if let Some(jpeg) = session.live_jpeg().map_err(|e| e.message().to_string())? {
        store_shared(shared, &jpeg);
    }
    while !cancel.load(Ordering::Relaxed) {
        let asks = std::mem::take(&mut lock(shared).asks);
        let mut dial_sent = false;
        let mut nudged = false;
        for ask in asks {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            if matches!(ask, Ask::Dial { .. }) {
                if dial_sent {
                    continue;
                }
                dial_sent = true;
            }
            if matches!(ask, Ask::Nudge(_)) {
                if nudged {
                    continue;
                }
                nudged = true;
            }
            match ask {
                Ask::Shoot => match session.capture() {
                    Ok(file) => {
                        let mut g = lock(shared);
                        g.imports.push(file);
                        g.error = None;
                    }
                    Err(e) => lock(shared).error = Some(e.message().to_string()),
                },
                Ask::Focus => match session.autofocus() {
                    Ok(()) => lock(shared).error = None,
                    Err(e) => lock(shared).error = Some(e.message().to_string()),
                },
                Ask::Dial { dial, step, absolute } => match session.set_dial(dial, step, absolute) {
                    Ok(_) => lock(shared).error = None,
                    Err(e) => lock(shared).error = Some(e.message().to_string()),
                },
                Ask::Nudge(direction) => match session.nudge(&direction) {
                    Ok(()) => lock(shared).error = None,
                    Err(e) => lock(shared).error = Some(e.message().to_string()),
                },
                Ask::Magnify(on) => match session.magnify(on) {
                    Ok(()) => lock(shared).error = None,
                    Err(e) => lock(shared).error = Some(e.message().to_string()),
                },
            }
        }
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        session.set_budget(3_000);
        if let Err(e) = session.refresh() {
            lock(shared).error = Some(e.message().to_string());
        }
        match session.take_new_file() {
            Ok(Some(file)) => lock(shared).imports.push(file),
            Ok(None) => {}
            Err(e) => lock(shared).error = Some(e.message().to_string()),
        }
        match session.live_jpeg() {
            Ok(Some(jpeg)) => {
                store_shared(shared, &jpeg);
                let mut g = lock(shared);
                // A later frame means a refused or late live-view read was not a dead link.
                if g.error.as_deref().is_some_and(|e| e.contains("did not answer") || e.contains("reading the file")) {
                    g.error = None;
                }
            }
            Ok(None) => {}
            Err(e) => lock(shared).error = Some(e.message().to_string()),
        }
        remember(shared, &session);
        std::thread::sleep(Duration::from_millis(40));
    }
    let _ = session.close();
    Ok(())
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn find_sony(client: &lightcraft_cameras::Client, cancel: &AtomicBool) -> std::result::Result<lightcraft_cameras::CameraInfo, String> {
    let mut other = None;
    // Image Capture fills its list after the browser has been running for a moment.
    // Twenty instant polls finish before that, and the tether bar disappears.
    for _ in 0..40 {
        if cancel.load(Ordering::Relaxed) {
            return Err("stopped".into());
        }
        let list = client.list(Duration::from_secs(2))?;
        if let Some(cam) = list.iter().find(|c| c.vendor == SONY || is_sony_name(&c.name)) {
            return Ok(cam.clone());
        }
        if let Some(cam) = list.first() {
            other = Some(cam.name.clone());
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    match other {
        Some(name) => Err(format!("{name} isn't a Sony camera this tether can drive")),
        None => Err("no camera in PC Remote. On the camera set USB Connection to PC Remote.".into()),
    }
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn is_sony_name(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.contains("sony") || n.contains("ilce")
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
fn store_shared(shared: &Mutex<Inner>, jpeg: &[u8]) {
    if let Some((w, h, rgba)) = decode_live(jpeg) {
        let mut g = lock(shared);
        g.generation = g.generation.saturating_add(1);
        g.width = w;
        g.height = h;
        g.rgba = Some(Arc::new(rgba));
    }
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
struct Bridge {
    client: lightcraft_cameras::Client,
    txn: u32,
    budget_ms: u64,
}

#[cfg(all(target_os = "macos", not(target_arch = "wasm32")))]
impl Transport for Bridge {
    fn set_budget(&mut self, millis: u64) {
        self.budget_ms = millis.max(500);
    }

    fn transact(&mut self, code: u16, params: &[u32], outgoing: Option<&[u8]>) -> lightcraft_ptp::Result<Reply> {
        // 0x9207's two bytes are one click: +1, −1, or a button. A shutter word or an ISO does not pass.
        if code == 0x9207 && !lightcraft_ptp::control_click(outgoing.unwrap_or(&[])) {
            return Err(PtpError::new("refusing a camera control that is not one click"));
        }
        let cmd = lightcraft_ptp::encode_command(code, self.txn, params)?;
        self.txn = self.txn.wrapping_add(1).max(1);
        let (response, data) = self.client.command(cmd, outgoing.map(|d| d.to_vec()), Duration::from_millis(self.budget_ms)).map_err(PtpError::new)?;
        let packet = lightcraft_ptp::decode(&response)?;
        Ok(Reply { code: packet.code, params: packet.params, data })
    }
}

pub fn status_json(s: &Session) -> Value {
    let t = s.tether.status();
    json!({
        "running": t.running,
        "connected": t.connected,
        "model": t.model,
        "aperture": t.aperture,
        "shutter": t.shutter,
        "iso": t.iso,
        "whiteBalance": t.white_balance,
        "magnified": t.magnified,
        "hint": t.hint,
        "error": t.error,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use lightcraft_develop::Preset;

    #[test]
    fn a_scripted_sony_imports_the_shot_and_applies_the_preset() {
        let mut sim = Sim::a7iii();
        sim.capture_payload = Some(lightcraft_ptp::TINY_JPEG.to_vec());
        let mut s = Session::new();
        s.add_presets(vec![Preset {
            id: "bright".into(),
            name: "Bright".into(),
            group: "User".into(),
            settings: json!({"light": {"exposure": 1.25}}),
            favorite: false,
            builtin: false,
        }]);
        s.tether.install_script(sim);
        s.execute("tether.start", &json!({"preset": "bright"})).expect("start");
        let status = s.execute("tether.status", &json!({})).expect("status");
        assert_eq!(status["model"], "ILCE-7M3");
        assert_eq!(status["running"], true);
        assert!(s.activity.list().iter().any(|t| t.kind == "tether"));
        let frame = s.tether.frame();
        assert!(frame.is_some(), "live view decoded");
        s.execute("tether.set", &json!({"dial": "aperture", "step": -1})).expect("aperture");
        let status = s.execute("tether.status", &json!({})).expect("status");
        assert_eq!(status["aperture"], "f/5.6");
        let shot = s.execute("tether.capture", &json!({})).expect("capture");
        let id = shot["photo"].as_u64().expect("photo id");
        let photo = s.catalog.photo(PhotoId(id)).expect("imported");
        assert!((photo.develop.light.exposure - 1.25).abs() < 0.001, "the preset set exposure");
        assert_eq!(s.selection.active, Some(PhotoId(id)));
        s.execute("tether.stop", &json!({})).expect("stop");
        assert!(!s.tether.is_running());
        assert!(!s.activity.list().iter().any(|t| t.kind == "tether"));
    }

    #[test]
    fn start_without_a_camera_explains_itself() {
        // On macOS this would open a plugged-in body. The message is the non-mac path.
        #[cfg(not(target_os = "macos"))]
        {
            let mut s = Session::new();
            let err = s.execute("tether.start", &json!({})).expect_err("no camera");
            let text = err.to_string();
            assert!(text.contains("isn't available"), "{text}");
        }
    }
}
