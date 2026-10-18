//! A PTP link to a camera plugged into this computer.
//!
//! On macOS the system camera service owns the USB device, so this crate talks through Image
//! Capture rather than claiming the interface. Framework calls run on the main thread:
//! [`Endpoint::pump`] does that, and [`Client`] only queues work. Other platforms have no
//! transport yet and every call fails with a clear error.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// A camera Image Capture can see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CameraInfo {
    pub name: String,
    /// Stable while the camera stays plugged in.
    pub id: String,
    /// USB vendor id, when the camera has one. Sony is `0x054C`.
    pub vendor: u16,
}

type CmdResult = std::result::Result<(Vec<u8>, Vec<u8>), String>;

#[derive(Debug)]
enum Request {
    List(Sender<std::result::Result<Vec<CameraInfo>, String>>),
    Open { id: Option<String>, reply: Sender<std::result::Result<(), String>> },
    Command { bytes: Vec<u8>, data: Option<Vec<u8>>, reply: Sender<CmdResult> },
    Close(Sender<std::result::Result<(), String>>),
    /// The worker stopped waiting. Drop the in-flight call so the next one, usually close, can run.
    Abandon,
    /// Close timed out. Drop the USB claim anyway.
    Release,
}

/// The side a worker holds. It blocks until [`Endpoint::pump`] runs the request on the main thread.
#[derive(Clone)]
pub struct Client {
    tx: Sender<Request>,
}

/// The side the UI thread holds. [`Endpoint::pump`] never blocks and never starts a nested run loop.
pub struct Endpoint {
    rx: Receiver<Request>,
    #[cfg(target_os = "macos")]
    mac: macos::Host,
}

/// A worker end and a main-thread end.
pub fn link() -> (Client, Endpoint) {
    let (tx, rx) = mpsc::channel();
    (
        Client { tx },
        Endpoint {
            rx,
            #[cfg(target_os = "macos")]
            mac: macos::Host::new(),
        },
    )
}

impl Client {
    pub fn list(&self, timeout: Duration) -> Result<Vec<CameraInfo>, String> {
        self.round(timeout, Request::List)
    }

    pub fn open(&self, id: Option<String>, timeout: Duration) -> Result<(), String> {
        self.round(timeout, |reply| Request::Open { id, reply })
    }

    /// `bytes` is a PTP command container. `data` is the optional data phase. Returns the response
    /// container and the data-phase bytes.
    pub fn command(&self, bytes: Vec<u8>, data: Option<Vec<u8>>, timeout: Duration) -> Result<(Vec<u8>, Vec<u8>), String> {
        self.round(timeout, |reply| Request::Command { bytes, data, reply })
    }

    pub fn close(&self, timeout: Duration) -> Result<(), String> {
        match self.round(timeout, Request::Close) {
            Err(e) if e.contains("did not answer") => {
                let _ = self.tx.send(Request::Release);
                Err(e)
            }
            other => other,
        }
    }

    fn round<T>(&self, timeout: Duration, make: impl FnOnce(Sender<Result<T, String>>) -> Request) -> Result<T, String> {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.tx.send(make(reply_tx)).map_err(|_| "the camera link is gone".to_string())?;
        match reply_rx.recv_timeout(timeout) {
            Ok(r) => r,
            Err(RecvTimeoutError::Timeout) => {
                let _ = self.tx.send(Request::Abandon);
                Err("the camera did not answer in time".into())
            }
            Err(RecvTimeoutError::Disconnected) => Err("the camera link is gone".into()),
        }
    }
}

impl Endpoint {
    /// Run one pending Image Capture call. Call this on the main thread, from the frame loop.
    pub fn pump(&mut self) {
        #[cfg(target_os = "macos")]
        {
            self.mac.service(&self.rx);
        }
        #[cfg(not(target_os = "macos"))]
        while let Ok(req) = self.rx.try_recv() {
            if matches!(req, Request::Abandon | Request::Release) {
                continue;
            }
            refuse(req, "tethered capture on this system isn't available yet");
        }
    }

    /// Pump, then let the main run loop deliver Image Capture's reply. The desktop frame loop
    /// already runs that loop, so it only calls [`Endpoint::pump`].
    pub fn spin(&mut self) {
        self.pump();
        #[cfg(target_os = "macos")]
        macos::spin_run_loop();
    }

    /// The Image Capture browser is stopped and nothing is waiting on the camera.
    pub fn settled(&self) -> bool {
        #[cfg(target_os = "macos")]
        {
            self.mac.settled()
        }
        #[cfg(not(target_os = "macos"))]
        {
            true
        }
    }
}

fn refuse(req: Request, msg: &str) {
    let err = msg.to_string();
    match req {
        Request::List(tx) => {
            let _ = tx.send(Err(err));
        }
        Request::Open { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        Request::Command { reply, .. } => {
            let _ = reply.send(Err(err));
        }
        Request::Close(tx) => {
            let _ = tx.send(Err(err));
        }
        Request::Abandon | Request::Release => {}
    }
}

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod macos {
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::sync::{Arc, mpsc::Receiver};

    use block2::RcBlock;
    use objc2::define_class;
    use objc2::msg_send;
    use objc2::rc::Retained;
    use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
    use objc2::{MainThreadMarker, MainThreadOnly};
    use objc2_foundation::{NSData, NSError};
    use objc2_image_capture_core::{ICCameraDevice, ICDevice, ICDeviceBrowser, ICDeviceBrowserDelegate};

    use super::{CameraInfo, Request, refuse};

    // Image Capture ignores `start` until a delegate is set, and it does not fill `devices`
    // before the first delegate call. The methods are empty on purpose: the list is read from
    // the browser afterwards.
    define_class!(
        #[unsafe(super(NSObject))]
        #[thread_kind = MainThreadOnly]
        #[ivars = ()]
        struct BrowserDelegate;

        impl BrowserDelegate {}

        unsafe impl NSObjectProtocol for BrowserDelegate {}

        unsafe impl ICDeviceBrowserDelegate for BrowserDelegate {
            #[unsafe(method(deviceBrowser:didAddDevice:moreComing:))]
            unsafe fn device_added(&self, _: &ICDeviceBrowser, _: &ICDevice, _: bool) {}

            #[unsafe(method(deviceBrowser:didRemoveDevice:moreGoing:))]
            unsafe fn device_removed(&self, _: &ICDeviceBrowser, _: &ICDevice, _: bool) {}
        }
    );

    impl BrowserDelegate {
        fn new(mtm: MainThreadMarker) -> Retained<Self> {
            let this = Self::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    const SONY: u16 = 0x054C;

    type PtpBlock = RcBlock<dyn Fn(std::ptr::NonNull<NSData>, std::ptr::NonNull<NSData>, *mut NSError)>;

    pub struct Host {
        browser: Option<Retained<ICDeviceBrowser>>,
        /// `setDelegate:` does not retain. This keeps the delegate alive for the browser.
        delegate: Option<Retained<BrowserDelegate>>,
        device: Option<Retained<ICCameraDevice>>,
        inflight: Arc<AtomicBool>,
        /// Generation of the call that currently owns `inflight`. A late completion for an older
        /// call must not clear a newer one.
        live: Arc<AtomicU64>,
        generation: u64,
        /// Close finished (or timed out). Eject, then stop the browser, so the body can leave PC.
        wants_eject: Arc<AtomicBool>,
        /// After the Image Capture session closes, stop the browser so the camera can leave PC Remote.
        release_browser: bool,
        /// A request that arrived while a call was in flight. Taken on the next idle pump.
        pending: Option<Request>,
        /// The in-flight call is the eject. A stale abandon from the close must not cancel it.
        ejecting: bool,
        hold_open: Option<RcBlock<dyn Fn(*mut NSError)>>,
        hold_cmd: Option<PtpBlock>,
        hold_close: Option<RcBlock<dyn Fn(*mut NSError)>>,
        /// Kept until the completion runs. Image Capture may not retain these itself.
        command_buf: Option<Retained<NSData>>,
        out_buf: Option<Retained<NSData>>,
    }

    impl Host {
        pub fn new() -> Self {
            Self {
                browser: None,
                delegate: None,
                device: None,
                inflight: Arc::new(AtomicBool::new(false)),
                live: Arc::new(AtomicU64::new(0)),
                generation: 0,
                wants_eject: Arc::new(AtomicBool::new(false)),
                release_browser: false,
                pending: None,
                ejecting: false,
                hold_open: None,
                hold_cmd: None,
                hold_close: None,
                command_buf: None,
                out_buf: None,
            }
        }

        pub fn settled(&self) -> bool {
            !self.inflight.load(Ordering::SeqCst) && !self.wants_eject.load(Ordering::SeqCst) && self.browser.is_none() && !self.release_browser
        }

        pub fn service(&mut self, rx: &Receiver<Request>) {
            if self.inflight.load(Ordering::SeqCst) {
                if self.pending.is_none() {
                    match rx.try_recv() {
                        Ok(Request::Abandon) if !self.ejecting => {
                            self.live.store(0, Ordering::SeqCst);
                            self.inflight.store(false, Ordering::SeqCst);
                        }
                        Ok(Request::Abandon) => {}
                        Ok(other) => self.pending = Some(other),
                        Err(_) => {}
                    }
                }
                return;
            }
            self.hold_open = None;
            self.hold_cmd = None;
            self.hold_close = None;
            self.command_buf = None;
            self.out_buf = None;
            if self.wants_eject.swap(false, Ordering::SeqCst) {
                if self.device.is_some() {
                    self.begin_eject();
                    return;
                }
                self.release_browser = true;
            }
            if self.release_browser {
                if let Some(browser) = self.browser.take() {
                    unsafe { browser.stop() };
                }
                self.delegate = None;
                self.device = None;
                self.release_browser = false;
                self.ejecting = false;
            }
            let req = if let Some(req) = self.pending.take() {
                req
            } else {
                match rx.try_recv() {
                    Ok(req) => req,
                    Err(_) => return,
                }
            };
            if MainThreadMarker::new().is_none() {
                refuse(req, "camera calls have to run on the main thread");
                return;
            }
            match req {
                Request::List(tx) => {
                    let _ = tx.send(self.list());
                }
                Request::Open { id, reply } => self.begin_open(id, reply),
                Request::Command { bytes, data, reply } => self.begin_command(bytes, data, reply),
                Request::Close(tx) => self.begin_close(tx),
                Request::Abandon => {}
                Request::Release => self.wants_eject.store(true, Ordering::SeqCst),
            }
        }

        fn arm(&mut self) -> u64 {
            self.generation = self.generation.wrapping_add(1).max(1);
            let ticket = self.generation;
            self.live.store(ticket, Ordering::SeqCst);
            self.inflight.store(true, Ordering::SeqCst);
            ticket
        }

        fn browser(&mut self) -> Result<&ICDeviceBrowser, String> {
            if self.browser.is_none() {
                let mtm = MainThreadMarker::new().ok_or_else(|| "camera calls have to run on the main thread".to_string())?;
                let delegate = BrowserDelegate::new(mtm);
                let browser = unsafe { ICDeviceBrowser::new() };
                unsafe {
                    browser.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
                    browser.start();
                }
                self.delegate = Some(delegate);
                self.browser = Some(browser);
            }
            self.browser.as_deref().ok_or_else(|| "Image Capture didn't start".to_string())
        }

        fn list(&mut self) -> Result<Vec<CameraInfo>, String> {
            self.browser()?;
            let Some(browser) = self.browser.as_ref() else { return Err("Image Capture didn't start".into()) };
            let Some(devices) = (unsafe { browser.devices() }) else { return Ok(Vec::new()) };
            let n = devices.count().min(64);
            let mut out = Vec::new();
            for i in 0..n {
                let device = devices.objectAtIndex(i);
                let name = unsafe { device.name() }.map(|s| s.to_string()).unwrap_or_default();
                let id = unsafe { device.persistentIDString() }.map(|s| s.to_string()).unwrap_or_else(|| name.clone());
                let vendor = u16::try_from(unsafe { device.usbVendorID() }).unwrap_or(0);
                if !name.is_empty() || vendor != 0 {
                    out.push(CameraInfo { name, id, vendor });
                }
            }
            Ok(out)
        }

        fn begin_open(&mut self, id: Option<String>, reply: std::sync::mpsc::Sender<Result<(), String>>) {
            let listed = match self.list() {
                Ok(d) => d,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return;
                }
            };
            let Some(picked) = pick(&listed, id.as_deref()) else {
                let _ = reply.send(Err(
                    "no Sony camera in PC Remote mode. On the camera set USB Connection to PC Remote, and leave the screen on Connecting.".into(),
                ));
                return;
            };
            let Some(browser) = self.browser.as_ref() else {
                let _ = reply.send(Err("Image Capture didn't start".into()));
                return;
            };
            let Some(camera) = find_camera(browser, &picked.id) else {
                let _ = reply.send(Err(format!("{} disappeared", picked.name)));
                return;
            };
            let ticket = self.arm();
            let live = Arc::clone(&self.live);
            let flag = Arc::clone(&self.inflight);
            let block = RcBlock::new(move |error: *mut NSError| {
                let result = if error.is_null() { Ok(()) } else { Err(ns_error(unsafe { &*error })) };
                let _ = reply.send(result);
                finish_flight(&live, &flag, ticket);
            });
            unsafe { camera.requestOpenSessionWithOptions_completion(None, &block) };
            self.hold_open = Some(block);
            self.device = Some(camera);
        }

        fn begin_command(&mut self, bytes: Vec<u8>, data: Option<Vec<u8>>, reply: std::sync::mpsc::Sender<super::CmdResult>) {
            let Some(device) = self.device.clone() else {
                let _ = reply.send(Err("no camera session".into()));
                return;
            };
            let ticket = self.arm();
            let live = Arc::clone(&self.live);
            let flag = Arc::clone(&self.inflight);
            let command = NSData::with_bytes(&bytes);
            let out = data.as_ref().map(|d| NSData::with_bytes(d));
            // Measured on an ILCE-7M3: the block is (data phase, response container, error).
            // An absent data phase is nil. The response container is the PTP response (type 3).
            let block = RcBlock::new(move |data: std::ptr::NonNull<NSData>, response: std::ptr::NonNull<NSData>, error: *mut NSError| {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if error.is_null() {
                        Ok((data_bytes(response), data_bytes(data)))
                    } else {
                        Err(ns_error(unsafe { &*error }))
                    }
                }));
                let result = match result {
                    Ok(r) => r,
                    Err(_) => Err("Image Capture returned an empty PTP buffer".into()),
                };
                let _ = reply.send(result);
                finish_flight(&live, &flag, ticket);
            });
            unsafe { device.requestSendPTPCommand_outData_completion(&command, out.as_deref(), &block) };
            self.command_buf = Some(command);
            self.out_buf = out;
            self.hold_cmd = Some(block);
        }

        fn begin_close(&mut self, reply: std::sync::mpsc::Sender<Result<(), String>>) {
            let Some(device) = self.device.clone() else {
                let _ = reply.send(Ok(()));
                self.release_browser = true;
                return;
            };
            let ticket = self.arm();
            let live = Arc::clone(&self.live);
            let flag = Arc::clone(&self.inflight);
            let eject = Arc::clone(&self.wants_eject);
            let block = RcBlock::new(move |error: *mut NSError| {
                let result = if error.is_null() { Ok(()) } else { Err(ns_error(unsafe { &*error })) };
                let _ = reply.send(result);
                eject.store(true, Ordering::SeqCst);
                finish_flight(&live, &flag, ticket);
            });
            // Ask Image Capture to drop the remote claim before the session close. A bare
            // CloseSession leaves an A7 III showing PC because the host still owns the device.
            #[allow(deprecated)]
            unsafe {
                device.requestEjectOrDisconnect();
            }
            unsafe { device.requestCloseSessionWithOptions_completion(None, &block) };
            self.hold_close = Some(block);
        }

        fn begin_eject(&mut self) {
            let Some(device) = self.device.clone() else {
                self.release_browser = true;
                return;
            };
            let ticket = self.arm();
            let live = Arc::clone(&self.live);
            let flag = Arc::clone(&self.inflight);
            let block = RcBlock::new(move |_error: *mut NSError| {
                finish_flight(&live, &flag, ticket);
            });
            unsafe { device.requestEjectWithCompletion(&block) };
            self.hold_close = Some(block);
            self.release_browser = true;
            self.ejecting = true;
        }
    }

    fn pick(devices: &[CameraInfo], id: Option<&str>) -> Option<CameraInfo> {
        if let Some(id) = id {
            return devices.iter().find(|d| d.id == id).cloned();
        }
        devices.iter().find(|d| d.vendor == SONY || is_sony_name(&d.name)).cloned()
    }

    fn is_sony_name(name: &str) -> bool {
        let n = name.to_ascii_lowercase();
        n.contains("sony") || n.contains("ilce")
    }

    fn find_camera(browser: &ICDeviceBrowser, id: &str) -> Option<Retained<ICCameraDevice>> {
        let devices = unsafe { browser.devices() }?;
        let n = devices.count().min(64);
        for i in 0..n {
            let device = devices.objectAtIndex(i);
            let pid = unsafe { device.persistentIDString() }.map(|s| s.to_string()).unwrap_or_default();
            let name = unsafe { device.name() }.map(|s| s.to_string()).unwrap_or_default();
            if pid == id || name == id {
                return device.downcast::<ICCameraDevice>().ok();
            }
        }
        None
    }

    fn finish_flight(live: &AtomicU64, inflight: &AtomicBool, ticket: u64) {
        if live.load(Ordering::SeqCst) == ticket {
            inflight.store(false, Ordering::SeqCst);
        }
    }

    pub(super) fn spin_run_loop() {
        let until = objc2_foundation::NSDate::dateWithTimeIntervalSinceNow(0.05);
        let mode = unsafe { objc2_foundation::NSDefaultRunLoopMode };
        let _ = objc2_foundation::NSRunLoop::mainRunLoop().runMode_beforeDate(mode, &until);
    }

    /// A data phase can be absent. The binding types that as non-null, and messaging nil aborts.
    fn data_bytes(ptr: std::ptr::NonNull<NSData>) -> Vec<u8> {
        // `NonNull` forbids null, so a plain check is deleted, but Image Capture passes nil for an
        // absent data phase and the binding still builds one.
        let addr = std::hint::black_box(ptr.as_ptr() as usize);
        if addr == 0 {
            return Vec::new();
        }
        unsafe { ptr.as_ref() }.to_vec()
    }

    fn ns_error(error: &NSError) -> String {
        let text = error.localizedDescription().to_string();
        if text.is_empty() { "the camera refused the command".into() } else { text }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_link_answers_without_hanging() {
        let (client, mut end) = link();
        let worker = std::thread::spawn(move || client.list(Duration::from_millis(500)));
        let start = std::time::Instant::now();
        while !worker.is_finished() && start.elapsed() < Duration::from_secs(2) {
            end.pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let result = worker.join();
        assert!(result.is_ok(), "listing must not panic");
    }
}
