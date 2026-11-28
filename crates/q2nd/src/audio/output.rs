#![forbid(unsafe_code)]
//! Normal-player USB output discovery. This observes ALSA's USB driver, never
//! changes USB role, VBUS, charging, mixer gains or card ownership.
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// One enumerated USB playback endpoint. Card numbers are an opening address,
/// not identity: Linux can reuse them after an unplug.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub struct UsbPlayback {
    pub identity: String,
    pub card: u32,
    pub device: u32,
}

impl UsbPlayback {
    /// ALSA's plug layer negotiates channels, rate and sample width with the
    /// actual USB hardware, retaining source-frame delay accounting.
    pub fn pcm(&self) -> String {
        format!("plughw:CARD={},DEV={}", self.card, self.device)
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Route {
    #[default]
    Headphones,
    Usb(UsbPlayback),
    /// Paired headphones with an open stream (`bluetooth::pcm`).
    Bluetooth,
}

impl Route {
    pub fn pcm(&self) -> String {
        match self { Self::Headphones => "hw:Q2,0".into(), Self::Usb(usb) => usb.pcm(), Self::Bluetooth => crate::bluetooth::pcm::DEVICE.into() }
    }
    pub fn label(&self) -> &'static str {
        match self { Self::Headphones => "Headphones", Self::Usb(_) => "USB DAC", Self::Bluetooth => "Bluetooth" }
    }
}

/// Deterministic preference/failure policy shared by the live monitor and
/// generated topology/fault tests. A failed DAC is retried only after its
/// enumeration changes; unrelated uevents never create an open/retry loop.
#[derive(Default)]
pub struct Policy {
    known: BTreeSet<UsbPlayback>,
    rejected: BTreeSet<UsbPlayback>,
    route: Route,
    /// The route among the wired outputs, used whenever Bluetooth has no stream.
    wired: Route,
    bluetooth: bool,
}

impl Policy {
    pub fn update(&mut self, devices: impl IntoIterator<Item = UsbPlayback>) -> bool {
        let devices: BTreeSet<_> = devices.into_iter().collect();
        self.rejected.retain(|device| devices.contains(device));
        let inserted = devices.difference(&self.known)
            .filter(|device| !self.rejected.contains(*device)).next().cloned();
        let current = match &self.wired {
            Route::Usb(device) if devices.contains(device) && !self.rejected.contains(device) => Some(device.clone()),
            _ => None,
        };
        let next = inserted.or(current).or_else(|| devices.iter()
            .find(|device| !self.rejected.contains(*device)).cloned());
        self.known = devices;
        self.set_route(next.map(Route::Usb).unwrap_or_default())
    }

    /// Called only for the exact route whose open/write failed. A late failure
    /// from a removed endpoint must not reject a newly inserted replacement.
    pub fn reject(&mut self, failed: &Route) -> bool {
        if *failed == Route::Bluetooth { return self.route == *failed && self.set_bluetooth(false); }
        let Route::Usb(device) = failed else { return false };
        if self.route != *failed { return false; }
        self.rejected.insert(device.clone());
        let devices = self.known.clone();
        self.update(devices)
    }

    pub fn route(&self) -> &Route { &self.route }

    /// An open Bluetooth stream takes the output; when it closes the wired choice returns.
    pub fn set_bluetooth(&mut self, open: bool) -> bool {
        self.bluetooth = open;
        self.apply()
    }

    fn set_route(&mut self, route: Route) -> bool {
        self.wired = route;
        self.apply()
    }

    fn apply(&mut self) -> bool {
        let route = if self.bluetooth { Route::Bluetooth } else { self.wired.clone() };
        if route == self.route { return false; }
        crate::ux::set_route(match route { Route::Headphones => 1, Route::Usb(_) => 2, Route::Bluetooth => 3 });
        self.route = route;
        true
    }
}

fn number(_blocking: &crate::rt::Blocking, path: &Path, radix: u32, maximum: u32) -> Option<u32> {
    let text = std::fs::read_to_string(path).ok()?;
    let number = u32::from_str_radix(text.trim(), radix).ok()?;
    (number <= maximum).then_some(number)
}

fn endpoint(name: &str) -> Option<(u32, u32)> {
    let (card, device) = name.strip_prefix("pcmC")?.strip_suffix('p')?.split_once('D')?;
    // Numeric ALSA addresses only, never a product string inserted into config.
    if card.is_empty() || device.is_empty() || !card.bytes().chain(device.bytes()).all(|b| b.is_ascii_digit()) { return None; }
    Some((card.parse().ok()?, device.parse().ok()?))
}

/// Read only public kernel device metadata. Requiring the exact USB-audio
/// driver excludes gadget audio, Clicker, HDMI and arbitrary numbered PCMs.
/// A disappearing/incomplete sysfs node is simply absent on this snapshot.
pub fn discover(blocking: &crate::rt::Blocking, sys: &Path) -> Vec<UsbPlayback> {
    let Ok(sound) = std::fs::read_dir(sys.join("class/sound")) else { return Vec::new() };
    let Ok(devices_root) = sys.join("devices").canonicalize() else { return Vec::new() };
    let mut result = BTreeSet::new();
    for entry in sound.flatten() {
        let Some((card, device)) = entry.file_name().to_str().and_then(endpoint) else { continue };
        let Ok(mut path) = entry.path().join("device").canonicalize() else { continue };
        if !path.starts_with(&devices_root) { continue; }
        let mut usb_driver = false;
        loop {
            if std::fs::read_link(path.join("driver")).ok().and_then(|driver| driver.file_name().map(|name| name == "snd-usb-audio")) == Some(true) {
                usb_driver = true;
            }
            if usb_driver {
                let metadata = (
                    number(blocking, &path.join("busnum"), 10, 65535), number(blocking, &path.join("devnum"), 10, 127),
                    number(blocking, &path.join("idVendor"), 16, 65535), number(blocking, &path.join("idProduct"), 16, 65535),
                );
                if let (Some(bus), Some(address), Some(vendor), Some(product)) = metadata {
                    if bus != 0 && address != 0 {
                        let relative = path.strip_prefix(&devices_root).unwrap().to_string_lossy();
                        result.insert(UsbPlayback { identity: format!("{relative}@{bus}:{address}:{vendor:04x}:{product:04x}"), card, device });
                    }
                    break;
                }
            }
            if path == devices_root || !path.pop() { break; }
        }
    }
    result.into_iter().collect()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snapshot {
    pub generation: u32,
    pub route: Route,
}

/// Event-driven hotplug monitor. Construction subscribes before the first
/// snapshot, preventing a scan/subscribe race. The socket waits on the shared
/// I/O reactor; sysfs scans use one isolated, demand-created pool worker.
/// Construction and Drop belong
/// to ordinary blocking threads (startup/player), never an executor or pool.
/// Drop cancels and joins the task, including any in-flight finite sysfs scan.
pub struct Monitor {
    shared: std::sync::Arc<Shared>,
    worker: Option<MonitorTask>,
}

struct MonitorTask {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    finished: Option<tokio::sync::oneshot::Receiver<()>>,
    // Retain the runtime owner until joined task/scan completion; its final
    // drop must stay on this blocking owner, not the I/O task.
    _scanner: std::sync::Arc<crate::rt::Jobs>,
}

// Drop the entire future (including descriptors/state) before acknowledging
// completion, also on panic, registration failure or runtime shutdown.
struct CompleteTask {
    work: Option<std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>>,
    _finished: std::sync::Arc<Finished>,
}
struct Finished(Option<tokio::sync::oneshot::Sender<()>>);
impl Drop for Finished {
    fn drop(&mut self) { if let Some(finished)=self.0.take() { let _ = finished.send(()); } }
}
impl std::future::Future for CompleteTask {
    type Output = ();
    fn poll(mut self: std::pin::Pin<&mut Self>, context: &mut std::task::Context<'_>) -> std::task::Poll<()> {
        self.work.as_mut().unwrap().as_mut().poll(context)
    }
}
impl Drop for CompleteTask {
    fn drop(&mut self) {
        drop(self.work.take());
    }
}
impl Drop for MonitorTask {
    fn drop(&mut self) {
        assert!(crate::rt::may_wait_for_tasks(), "USB monitor must be dropped on its blocking owner");
        if let Some(stop) = self.stop.take() { let _ = stop.send(()); }
        if let Some(finished)=self.finished.take() { let _ = finished.blocking_recv(); }
    }
}

#[derive(Default)]
struct Batch { rescan: bool, uncertain: bool }

type Receive = fn(&std::fs::File, &mut [u8]) -> std::io::Result<(usize, bool)>;

// At most 64 nonblocking receives per poll, preserving shutdown/fairness under
// continuous input. Sender authentication precedes all policy changes.
fn drain(state: &Shared, flags: i16, mut receive: impl FnMut(&mut [u8]) -> std::io::Result<(usize, bool)>) -> std::io::Result<Batch> {
    if flags & (libc::POLLHUP | libc::POLLNVAL) != 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::BrokenPipe, "USB event stream closed"));
    }
    let mut batch = Batch { rescan: flags & libc::POLLERR != 0, uncertain: flags & libc::POLLERR != 0 };
    let mut observed = batch.rescan;
    let mut buffer = [0u8; 8192];
    for _ in 0..64 {
        match receive(&mut buffer) {
            Ok((0, _)) => return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "USB event stream ended")),
            Ok((count, trusted)) => {
                observed = true;
                if trusted {
                    state.removed(&buffer[..count]);
                    batch.uncertain |= count == buffer.len();
                    batch.rescan |= count == buffer.len() || relevant_event(&buffer[..count]);
                }
            }
            Err(error) if error.raw_os_error() == Some(libc::ENOBUFS) => {
                observed = true; batch.rescan = true; batch.uncertain = true;
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => { observed = true; }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock && observed => break,
            Err(error) => return Err(error),
        }
    }
    Ok(batch)
}

type Scan = std::sync::Arc<dyn Fn(&crate::rt::Blocking) -> Vec<UsbPlayback> + Send + Sync>;

async fn monitor_events(socket: crate::rt::Fd, state: std::sync::Arc<Shared>, mut stop: tokio::sync::oneshot::Receiver<()>, scan: Scan, receive: Receive, completion: std::sync::Arc<Finished>, scanner: std::sync::Arc<crate::rt::Jobs>) {
    loop {
        let result = tokio::select! {
            biased;
            _ = &mut stop => return,
            result = socket.read_with(|file| {
                let flags = crate::platform::netlink::read_events(file)?;
                drain(&state, flags, |buffer| receive(file, buffer))
            }) => result,
        };
        let batch = match result {
            Ok(batch) => batch,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => { state.update(Vec::new()); return; }
        };
        if batch.rescan {
            let scan = scan.clone();
            // No state/callback is owned by this closure. Join even if stop
            // arrives during a scan, then discard its result before publication.
            let scan_completion = completion.clone();
            let devices = scanner.run_isolated(move |blocking| {
                let _completion = scan_completion;
                scan(blocking)
            }).await;
            tokio::select! {
                biased;
                _ = &mut stop => return,
                _ = std::future::ready(()) => {},
            }
            match devices {
                Ok(devices) => state.reconcile(devices, batch.uncertain),
                Err(_) => { state.update(Vec::new()); return; }
            }
        }
        tokio::task::yield_now().await;
    }
}

struct Shared {
    policy: std::sync::Mutex<Policy>,
    generation: std::sync::atomic::AtomicU32,
    notify:std::sync::Mutex<Option<std::sync::Arc<dyn Fn()+Send+Sync>>>,
}

impl Shared {
    fn update(&self, devices: Vec<UsbPlayback>) {
        self.reconcile(devices,false);
    }
    fn reconcile(&self,devices:Vec<UsbPlayback>,uncertain:bool) {
        let changed={
            let mut policy = self.policy.lock().unwrap();
            let retired=uncertain && matches!(policy.route,Route::Usb(_));
            if uncertain {
                // An overrun can hide remove/add while leaving identical final
                // metadata. Requalify that enumeration once, not an old PCM.
                policy.known.clear();policy.rejected.clear();policy.wired=Route::Headphones;
                if !policy.bluetooth {policy.route=Route::Headphones;}
            }
            let changed=policy.update(devices)||retired;
            if changed { self.generation.fetch_add(1, std::sync::atomic::Ordering::Release); }
            changed
        };
        if changed {self.notify();}
    }
    fn removed(&self,event:&[u8]) {
        let Some((subsystem,path))=removal(event) else {return;};
        let changed={
            let mut policy=self.policy.lock().unwrap();
            let remaining:Vec<_>=policy.known.iter().filter(|device|!removed_device(device,subsystem,path)).cloned().collect();
            let changed=policy.update(remaining);
            if changed {self.generation.fetch_add(1,std::sync::atomic::Ordering::Release);}
            changed
        };
        if changed {self.notify();}
    }
    fn notify(&self) {
        let notify=self.notify.lock().unwrap().clone();
        if let Some(notify)=notify {notify();}
    }
}

impl Monitor {
    pub fn start() -> std::io::Result<Self> { Self::at(PathBuf::from("/sys")) }

    fn at(sys: PathBuf) -> std::io::Result<Self> {
        if !crate::rt::may_wait_for_tasks() {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "USB monitor needs a blocking owner"));
        }
        let socket = crate::platform::netlink::kobject_events()?;
        let blocking = crate::rt::Blocking::dedicated_thread();
        let initial = discover(&blocking, &sys);
        let scan: Scan = std::sync::Arc::new(move |blocking| discover(blocking, &sys));
        Self::with_socket(socket, initial, scan, crate::platform::netlink::receive_kernel)
    }

    fn with_socket(socket: std::os::fd::OwnedFd, initial: Vec<UsbPlayback>, scan: Scan, receive: Receive) -> std::io::Result<Self> {
        Self::with_socket_named(socket, initial, scan, receive, "usb-audio-scan")
    }

    fn with_socket_named(socket: std::os::fd::OwnedFd, initial: Vec<UsbPlayback>, scan: Scan, receive: Receive, name: &'static str) -> std::io::Result<Self> {
        if !crate::rt::may_wait_for_tasks() {
            return Err(std::io::Error::new(std::io::ErrorKind::WouldBlock, "USB monitor needs a blocking owner"));
        }
        let shared = std::sync::Arc::new(Shared { policy: Default::default(), generation: 0.into(), notify: Default::default() });
        // Startup has subscribed and scanned on its blocking caller already.
        shared.update(initial);
        let (ready, registered) = tokio::sync::oneshot::channel();
        let (finished, completed) = tokio::sync::oneshot::channel();
        let (stop, cancelled) = tokio::sync::oneshot::channel();
        let finished = std::sync::Arc::new(Finished(Some(finished)));
        let completion = finished.clone();
        let state = shared.clone();
        let scanner = std::sync::Arc::new(crate::rt::Jobs::serial(name)?);
        let scan_lane = scanner.clone();
        let work = async move {
            let socket = match crate::rt::Fd::new(socket) {
                Ok(socket) => socket,
                Err(error) => { let _ = ready.send(Err(error)); return; }
            };
            if ready.send(Ok(())).is_err() { return; }
            monitor_events(socket, state, cancelled, scan, receive, completion, scan_lane).await;
        };
        crate::rt::io().spawn(CompleteTask { work: Some(Box::pin(work)), _finished: finished });
        let worker = MonitorTask { stop: Some(stop), finished: Some(completed), _scanner: scanner };
        registered.blocking_recv().map_err(|_| std::io::Error::other("USB monitor registration ended"))??;
        Ok(Self { shared, worker: Some(worker) })
    }

    /// A monitor without USB discovery (simulator and headless runs): Bluetooth can still take the route.
    pub fn detached() -> Self {
        Self { shared: std::sync::Arc::new(Shared { policy: Default::default(), generation: 0.into(), notify: Default::default() }), worker: None }
    }

    /// The Bluetooth thread's switch: called with whether its stream is open.
    pub fn bluetooth_switch(&self) -> std::sync::Arc<dyn Fn(bool) + Send + Sync> {
        let shared = self.shared.clone();
        std::sync::Arc::new(move |open| {
            let changed = {
                let mut policy = shared.policy.lock().unwrap();
                let changed = policy.set_bluetooth(open);
                if changed { shared.generation.fetch_add(1, std::sync::atomic::Ordering::Release); }
                changed
            };
            if changed { shared.notify(); }
        })
    }

    pub fn snapshot(&self) -> Snapshot {
        let policy = self.shared.policy.lock().unwrap();
        Snapshot { generation: self.shared.generation.load(std::sync::atomic::Ordering::Acquire), route: policy.route().clone() }
    }

    pub fn changed(&self, generation: u32) -> bool {
        self.shared.generation.load(std::sync::atomic::Ordering::Acquire) != generation
    }

    pub fn set_notify(&mut self,notify:std::sync::Arc<dyn Fn()+Send+Sync>) {
        *self.shared.notify.lock().unwrap()=Some(notify);
    }

    pub fn reject(&self, failed: &Snapshot) -> Snapshot {
        let changed={
            let mut policy = self.shared.policy.lock().unwrap();
            let current=self.shared.generation.load(std::sync::atomic::Ordering::Acquire);
            let changed=current==failed.generation && policy.reject(&failed.route);
            if changed { self.shared.generation.fetch_add(1, std::sync::atomic::Ordering::Release); }
            changed
        };
        if changed {self.shared.notify();}
        self.snapshot()
    }

    #[cfg(test)]
    pub(crate) fn fixture()->(Self,FixtureOutputs) {
        let shared=std::sync::Arc::new(Shared {policy:Default::default(),generation:0.into(),notify:Default::default()});
        (Self {shared:shared.clone(),worker:None},FixtureOutputs(shared))
    }
}

#[cfg(test)]
pub(crate) struct FixtureOutputs(std::sync::Arc<Shared>);
#[cfg(test)]
impl FixtureOutputs {
    pub fn update(&self,devices:Vec<UsbPlayback>) {self.0.update(devices);}
    pub fn removed(&self,event:&[u8]) {self.0.removed(event);}
}

impl Drop for Monitor {
    fn drop(&mut self) { drop(self.worker.take()); }
}

fn relevant_event(event: &[u8]) -> bool {
    event.split(|&byte| byte == 0).any(|field| field == b"SUBSYSTEM=sound" || field == b"SUBSYSTEM=usb")
}

fn removal(event:&[u8])->Option<(&str,&str)> {
    let fields:Vec<_>=event.split(|&byte|byte==0).collect();
    if !fields.contains(&b"ACTION=remove".as_slice()) {return None;}
    let field=|prefix:&[u8]|fields.iter().find_map(|field|field.strip_prefix(prefix)).and_then(|value|std::str::from_utf8(value).ok());
    let subsystem=field(b"SUBSYSTEM=")?;
    if !matches!(subsystem,"sound"|"usb") {return None;}
    let path=field(b"DEVPATH=")?;
    path.starts_with("/devices/").then_some((subsystem,path))
}
fn removed_device(device:&UsbPlayback,subsystem:&str,path:&str)->bool {
    let Some((usb,_))=device.identity.rsplit_once('@') else {return false;};
    let root=format!("/devices/{usb}");
    if subsystem=="usb" {root==path || root.starts_with(&format!("{path}/"))}
    else {path.starts_with(&format!("{root}/")) &&
        (path.ends_with(&format!("/pcmC{}D{}p",device.card,device.device)) || path.ends_with(&format!("/card{}",device.card)))}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);

    fn dac(port: u32, address: u32, card: u32, device: u32) -> UsbPlayback {
        UsbPlayback { identity: format!("platform/usb/1-{port}@1:{address}:1234:5678"), card, device }
    }

    fn receive_fixture(mut file: &std::fs::File, buffer: &mut [u8]) -> std::io::Result<(usize, bool)> {
        use std::io::Read;
        let count = file.read(buffer)?;
        if count == 0 { return Ok((0, false)); }
        let trusted = buffer[0] == b'+';
        buffer.copy_within(1..count, 0);
        Ok((count - 1, trusted))
    }

    fn wait_for(mut condition: impl FnMut() -> bool) {
        let until = std::time::Instant::now() + crate::library::TEST_LIVENESS;
        while !condition() {
            assert!(std::time::Instant::now() < until, "event monitor made no progress");
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
    }

    fn completion_within(completed: tokio::sync::oneshot::Receiver<()>) {
        let (sent,received)=std::sync::mpsc::channel();
        crate::rt::io().spawn(async move {
            sent.send(matches!(tokio::time::timeout(std::time::Duration::from_secs(3),completed).await,Ok(Ok(())))).unwrap();
        });
        assert!(received.recv_timeout(std::time::Duration::from_secs(4)).unwrap());
    }

    fn shared(devices: Vec<UsbPlayback>) -> std::sync::Arc<Shared> {
        let shared = std::sync::Arc::new(Shared { policy: Default::default(), generation: 0.into(), notify: Default::default() });
        shared.update(devices);
        shared
    }

    #[test]
    fn bounded_drain_authenticates_removal_and_requalifies_uncertain_streams() {
        let state = shared(vec![dac(1,2,9,3)]);
        let remove = b"ACTION=remove\0SUBSYSTEM=usb\0DEVPATH=/devices/platform/usb/1-1\0";
        let mut calls = 0;
        let batch = drain(&state, libc::POLLIN, |buffer| {
            calls += 1;
            if calls == 1 { buffer[..remove.len()].copy_from_slice(remove); Ok((remove.len(),false)) }
            else { Err(std::io::ErrorKind::WouldBlock.into()) }
        }).unwrap();
        assert!(!batch.rescan);
        assert_eq!(*state.policy.lock().unwrap().route(),Route::Usb(dac(1,2,9,3)));
        let mut calls = 0;
        let batch = drain(&state, libc::POLLIN, |buffer| {
            calls += 1;
            if calls == 1 { buffer[..remove.len()].copy_from_slice(remove); Ok((remove.len(),true)) }
            else { Err(std::io::ErrorKind::WouldBlock.into()) }
        }).unwrap();
        assert!(batch.rescan);
        assert_eq!(*state.policy.lock().unwrap().route(),Route::Headphones);
        for code in [libc::ENOBUFS, libc::EINTR] {
            let mut calls = 0;
            let batch = drain(&state, libc::POLLIN, |_| {
                calls += 1;
                Err(std::io::Error::from_raw_os_error(if calls == 1 {code} else {libc::EAGAIN}))
            }).unwrap();
            assert_eq!(batch.uncertain,code==libc::ENOBUFS);
            assert_eq!(batch.rescan,batch.uncertain);
        }
        assert!(drain(&state,libc::POLLERR,|_|Err(std::io::ErrorKind::WouldBlock.into())).unwrap().uncertain);
        let mut calls = 0;
        assert!(drain(&state,libc::POLLIN,|buffer| {
            calls += 1;buffer.fill(0);Ok((buffer.len(),true))
        }).unwrap().uncertain);
        assert_eq!(calls,64,"continuous events have a bounded reactor poll");
        for flags in [libc::POLLHUP,libc::POLLNVAL] {
            assert!(drain(&state,flags,|_|panic!("closed descriptor read")).is_err());
        }
        assert!(drain(&state,libc::POLLIN,|_|Ok((0,true))).is_err());
        assert_eq!(drain(&state,libc::POLLIN,|_|Err(std::io::ErrorKind::WouldBlock.into())).err().unwrap().kind(),std::io::ErrorKind::WouldBlock);
    }

    #[test]
    fn reactor_delivers_presubscribed_events_reenumeration_and_ignores_untrusted_sender() {
        use std::sync::{Arc,Mutex,atomic::{AtomicU32,Ordering}};
        let (socket,events)=std::os::unix::net::UnixDatagram::pair().unwrap();
        let devices=Arc::new(Mutex::new(vec![dac(1,2,9,3)]));
        let scans=Arc::new(AtomicU32::new(0));
        // The first scan waits until the test has installed its notifier, so
        // the first publication's notification is received here and cannot
        // arrive later in place of one the untrusted sender would cause.
        let (release_first,first_gate)=std::sync::mpsc::channel::<()>();let first_gate=Mutex::new(Some(first_gate));
        let scan:Scan={let devices=devices.clone();let scans=scans.clone();Arc::new(move |_| {
            assert!(!crate::rt::on_executor() && crate::rt::on_pool());
            scans.fetch_add(1,Ordering::SeqCst);
            if let Some(gate)=first_gate.lock().unwrap().take() {gate.recv().unwrap();}
            devices.lock().unwrap().clone()
        })};
        events.send(b"+SUBSYSTEM=sound\0").unwrap();
        let monitor=Monitor::with_socket(socket.into(),vec![],scan,receive_fixture).unwrap();
        let changed=monitor.shared.clone();
        let (sent,got)=std::sync::mpsc::channel();
        *changed.notify.lock().unwrap()=Some(Arc::new(move||{sent.send(()).unwrap();}));
        release_first.send(()).unwrap();
        got.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        let first=monitor.snapshot();
        assert_eq!(first.route,Route::Usb(dac(1,2,9,3)));
        assert_eq!(scans.load(Ordering::SeqCst),1);
        // A userspace-shaped message never retires/requalifies this route.
        events.send(b"?ACTION=remove\0SUBSYSTEM=usb\0DEVPATH=/devices/platform/usb/1-1\0").unwrap();
        events.send(b"+SUBSYSTEM=power_supply\0").unwrap();
        // Barrier through the same socket: a trusted sound event scans once.
        events.send(b"+SUBSYSTEM=sound\0").unwrap();
        wait_for(||scans.load(Ordering::SeqCst)==2);
        assert_eq!(monitor.snapshot(),first);
        assert!(got.try_recv().is_err());
        events.send(b"+ACTION=remove\0SUBSYSTEM=usb\0DEVPATH=/devices/platform/usb/1-1\0").unwrap();
        wait_for(||monitor.snapshot().generation>=first.generation+2);
        assert_eq!(monitor.snapshot().route,first.route,"same final metadata still reopens after remove/add");
        got.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        got.recv_timeout(crate::library::TEST_LIVENESS).unwrap();
        assert!(got.try_recv().is_err());
        drop(monitor);
        assert_eq!(Arc::strong_count(&changed),1,"joined task released its policy");
    }

    #[test]
    fn shutdown_joins_stalled_scan_without_late_publication_or_executor_stall() {
        use std::sync::{Arc,Mutex};
        let (socket,events)=std::os::unix::net::UnixDatagram::pair().unwrap();
        let (entered,started)=std::sync::mpsc::channel();
        let (release,gate)=std::sync::mpsc::channel();let gate=Mutex::new(gate);
        let scan:Scan=Arc::new(move |_| {
            entered.send(()).unwrap();gate.lock().unwrap().recv().unwrap();vec![dac(1,2,9,3)]
        });
        let monitor=Monitor::with_socket(socket.into(),vec![],scan,receive_fixture).unwrap();
        let state=monitor.shared.clone();
        events.send(b"+SUBSYSTEM=sound\0").unwrap();
        started.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        let (ping,pong)=std::sync::mpsc::channel();crate::rt::io().spawn(async move {ping.send(()).unwrap();});
        pong.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let (dropping,began)=std::sync::mpsc::channel();let (done,finished)=std::sync::mpsc::channel();
        let worker=std::thread::spawn(move||{let mut monitor=monitor;let _=monitor.worker.as_mut().unwrap().stop.take().unwrap().send(());dropping.send(()).unwrap();drop(monitor);done.send(()).unwrap();});
        began.recv().unwrap();
        assert!(finished.recv_timeout(std::time::Duration::from_millis(20)).is_err());
        release.send(()).unwrap();finished.recv_timeout(std::time::Duration::from_secs(3)).unwrap();worker.join().unwrap();
        assert_eq!(*state.policy.lock().unwrap().route(),Route::Headphones,"cancelled scan must not publish");
        assert_eq!(state.generation.load(std::sync::atomic::Ordering::Acquire),0);
        assert_eq!(Arc::strong_count(&state),1);
    }

    #[test]
    fn registration_failure_executor_rejection_and_unpolled_task_drop_complete() {
        use std::sync::Arc;
        let fixture=Fixture::new();let path=fixture.0.join("ordinary");std::fs::write(&path,b"generated").unwrap();
        let file=std::fs::File::open(path).unwrap();
        assert!(Monitor::with_socket(file.into(),vec![],Arc::new(|_|panic!("no scan after registration failure")),receive_fixture).is_err());
        let (reply,answer)=std::sync::mpsc::channel();
        crate::rt::io().spawn(async move {reply.send(Monitor::at(PathBuf::from("/not-read-on-executor")).err().unwrap().kind()).unwrap();});
        assert_eq!(answer.recv_timeout(std::time::Duration::from_secs(3)).unwrap(),std::io::ErrorKind::WouldBlock);
        let (reply,answer)=std::sync::mpsc::channel();
        crate::rt::spawn_blocking(move |_| {reply.send(Monitor::at(PathBuf::from("/not-read-on-pool")).err().unwrap().kind()).unwrap();});
        assert_eq!(answer.recv_timeout(std::time::Duration::from_secs(3)).unwrap(),std::io::ErrorKind::WouldBlock);
        let (finished,completed)=tokio::sync::oneshot::channel();
        let owned=Arc::new(());let captured=owned.clone();
        let future=async move {let _owned=captured;std::future::pending::<()>().await;};
        let task=CompleteTask{work:Some(Box::pin(future)),_finished:Arc::new(Finished(Some(finished)))};
        let runtime=tokio::runtime::Builder::new_current_thread().build().unwrap();
        runtime.spawn(task);
        drop(runtime);
        completion_within(completed);
        assert_eq!(Arc::strong_count(&owned),1);
    }

    #[test]
    fn continuous_unrelated_events_yield_and_shutdown_without_a_scan() {
        use std::sync::{Arc,atomic::{AtomicBool,AtomicU32,Ordering}};
        let (socket,events)=std::os::unix::net::UnixDatagram::pair().unwrap();
        events.set_nonblocking(true).unwrap();
        let monitor=Monitor::with_socket(socket.into(),vec![],Arc::new(|_|panic!("unrelated events must not scan")),receive_fixture).unwrap();
        let count=Arc::new(AtomicU32::new(0));let stop=Arc::new(AtomicBool::new(false));
        let running=stop.clone();let sent=count.clone();
        let producer=std::thread::spawn(move|| {
            while !running.load(Ordering::Acquire) {
                match events.send(b"+SUBSYSTEM=power_supply\0") {
                    Ok(_)=>{sent.fetch_add(1,Ordering::Relaxed);}
                    Err(error) if error.kind()==std::io::ErrorKind::WouldBlock=>std::thread::yield_now(),
                    Err(_)=>break,
                }
            }
        });
        wait_for(||count.load(Ordering::Relaxed)>=128);
        let (ping,pong)=std::sync::mpsc::channel();crate::rt::io().spawn(async move {ping.send(()).unwrap();});
        pong.recv_timeout(std::time::Duration::from_secs(1)).unwrap();
        let (done,finished)=std::sync::mpsc::channel();
        let owner=std::thread::spawn(move||{drop(monitor);done.send(()).unwrap();});
        let result=finished.recv_timeout(std::time::Duration::from_secs(3));
        stop.store(true,Ordering::Release);producer.join().unwrap();result.unwrap();owner.join().unwrap();
    }

    #[derive(Debug, PartialEq, Eq)]
    struct WorkerStat { tid: u32, start: u64, state: u8, flags: u64 }

    fn worker_stat(text: &str) -> Option<WorkerStat> {
        // comm may contain spaces and parentheses. Fields after its final ") "
        // start at stat field 3: state[0], flags[6] (field 9), start[19] (22).
        let (head, tail) = text.rsplit_once(") ")?;
        let (tid, _) = head.split_once(" (")?;
        let fields: Vec<_> = tail.split_ascii_whitespace().collect();
        if fields.len() < 20 || fields[0].len() != 1 { return None; }
        let state = fields[0].as_bytes()[0];
        if !b"RSDZTtXxKWPIN".contains(&state) { return None; }
        let result = WorkerStat { tid: tid.parse().ok()?, start: fields[19].parse().ok()?, state, flags: fields[6].parse().ok()? };
        (result.tid > 0 && result.start > 0).then_some(result)
    }

    fn worker_exiting(worker: &WorkerStat) -> bool {
        // Linux include/linux/sched.h: PF_EXITING = 0x00000004. The
        // clear_child_tid/futex wake in exit_mm precedes exit_notify and
        // proc removal, so pthread_join can return with this task visible.
        worker.flags & 0x00000004 != 0
    }

    #[test]
    fn scanner_retirement_stat_parser_preserves_identity_and_refuses_live_workers() {
        let mut fields = vec!["0"; 20];
        fields[0] = "R"; fields[6] = "4"; fields[19] = "12345";
        let fixture = format!("42 (generated ) worker) {}", fields.join(" "));
        let stat = worker_stat(&fixture).unwrap();
        assert_eq!(stat, WorkerStat { tid: 42, start: 12345, state: b'R', flags: 4 });
        assert!(worker_exiting(&stat), "exiting task may still report runnable");
        fields[6] = "0";
        let live = worker_stat(&format!("42 (generated) {}", fields.join(" "))).unwrap();
        assert!(!worker_exiting(&live));
        for bad in [fixture.replace("R ", "unknown "), fixture.replace("12345", "-1"), fixture.replace("42 (", "0 ("), "42 (short) R 0".into()] {
            assert!(worker_stat(&bad).is_none());
        }
    }

    #[test]
    fn saturated_general_pool_cannot_delay_arrival_and_scan_worker_retires() {
        crate::rt::test_support::isolated_pool_test(
            "audio::output::tests::saturated_general_pool_cannot_delay_arrival_and_scan_worker_retires",
            "Q2_USB_SCAN_POOL_ISOLATED_TEST",
            || {
                use std::sync::{Arc,atomic::{AtomicU32,Ordering}};
                fn workers() -> usize {
                    // Only this generated test's own process and unique thread role.
                    std::fs::read_dir("/proc/self/task").unwrap().filter_map(Result::ok)
                        .filter(|entry|std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|name|name.trim()=="usb-scan-test")).count()
                }
                let (socket,events)=std::os::unix::net::UnixDatagram::pair().unwrap();
                let calls=Arc::new(AtomicU32::new(0));let count=calls.clone();
                let scan:Scan=Arc::new(move |_| {
                    assert!(!crate::rt::on_executor() && crate::rt::on_pool());
                    vec![dac(1,2+count.fetch_add(1,Ordering::SeqCst),9,3)]
                });
                assert_eq!(workers(),0);
                let monitor=Monitor::with_socket_named(socket.into(),vec![],scan,receive_fixture,"usb-scan-test").unwrap();
                assert_eq!(workers(),0,"no scanner worker before an actual relevant event");
                let first_event_us=crate::rt::test_support::with_saturated_general_pool(std::time::Duration::from_secs(10), || {
                    let begin=std::time::Instant::now();events.send(b"+SUBSYSTEM=sound\0").unwrap();
                    wait_for(||monitor.snapshot().route==Route::Usb(dac(1,2,9,3)));
                    let elapsed=begin.elapsed().as_micros();
                    assert_eq!(workers(),1,"exactly one isolated scanner despite saturated general pool");
                    elapsed
                });
                let deadline=std::time::Instant::now()+std::time::Duration::from_secs(15);
                while workers()!=0 {
                    assert!(std::time::Instant::now()<deadline,"idle scanner did not retire");
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                let begin=std::time::Instant::now();events.send(b"+SUBSYSTEM=sound\0").unwrap();
                wait_for(||monitor.snapshot().route==Route::Usb(dac(1,3,9,3)));
                println!("Q2_USB_SCAN_COST first_event_us={first_event_us} restarted_event_us={}",begin.elapsed().as_micros());
                assert_eq!(calls.load(Ordering::SeqCst),2);
                let scanner = Arc::downgrade(&monitor.worker.as_ref().unwrap()._scanner);
                let tasks: Vec<_> = std::fs::read_dir("/proc/self/task").unwrap().filter_map(Result::ok)
                    .filter(|entry| std::fs::read_to_string(entry.path().join("comm")).is_ok_and(|name| name.trim()=="usb-scan-test"))
                    .map(|entry| (entry.path().join("stat"), worker_stat(&std::fs::read_to_string(entry.path().join("stat")).unwrap()).unwrap()))
                    .collect();
                assert_eq!(tasks.len(),1,"pin the exact restarted scanner before shutdown");
                let (path, before) = &tasks[0];
                drop(monitor);
                // Final Jobs/Runtime ownership is gone synchronously. Tokio's
                // BlockingPool::drop joins both active and idle-exited handles;
                // /proc directory disappearance is a later kernel observation.
                assert!(scanner.upgrade().is_none(),"shutdown released the final scanner runtime owner");
                match std::fs::read_to_string(path) {
                    Ok(text) => {
                        let after = worker_stat(&text).expect("complete scanner stat after join");
                        assert_eq!((after.tid,after.start),(before.tid,before.start),"scanner identity changed during retirement");
                        println!("Q2_USB_SCAN_RETIRE joined=1 exiting={} absent=0 state={} flags={}",u8::from(worker_exiting(&after)),after.state,after.flags);
                        assert!(worker_exiting(&after),"joined scanner remains live rather than in kernel exit: {after:?}");
                    }
                    Err(error) if error.kind()==std::io::ErrorKind::NotFound || error.raw_os_error()==Some(libc::ESRCH) => println!("Q2_USB_SCAN_RETIRE joined=1 exiting=0 absent=1 state=0 flags=0"),
                    Err(error) => panic!("scanner retirement observation failed: {error}"),
                }
            },
        );
    }

    #[test]
    fn runtime_cancellation_retains_completion_until_its_scan_finishes() {
        use std::sync::Arc;
        let (release,gate)=std::sync::mpsc::channel();let (entered,started)=tokio::sync::oneshot::channel();
        let (finished,mut completed)=tokio::sync::oneshot::channel();
        let (retired,cancelled)=std::sync::mpsc::channel();
        let owner=std::thread::spawn(move|| {
            let runtime=tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            let completion=Arc::new(Finished(Some(finished)));let scan_completion=completion.clone();
            let work=async move {
                let scan=crate::rt::spawn_blocking(move |_| {
                    let _completion=scan_completion;
                    entered.send(()).unwrap();gate.recv().unwrap();
                });
                let _=scan.await;
            };
            runtime.spawn(CompleteTask{work:Some(Box::pin(work)),_finished:completion});
            runtime.block_on(async {started.await.unwrap();});
            drop(runtime);retired.send(()).unwrap();
        });
        cancelled.recv_timeout(std::time::Duration::from_secs(3)).unwrap();
        assert!(matches!(completed.try_recv(),Err(tokio::sync::oneshot::error::TryRecvError::Empty)));
        release.send(()).unwrap();completion_within(completed);owner.join().unwrap();
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn scan_and_task_panics_complete_shutdown_without_retaining_state() {
        use std::sync::Arc;
        let (socket,events)=std::os::unix::net::UnixDatagram::pair().unwrap();
        let monitor=Monitor::with_socket(socket.into(),vec![dac(1,2,9,3)],Arc::new(|_|panic!("generated scan failure")),receive_fixture).unwrap();
        events.send(b"+SUBSYSTEM=sound\0").unwrap();
        wait_for(||monitor.snapshot().route==Route::Headphones);
        drop(monitor);
        let (finished,completed)=tokio::sync::oneshot::channel();let owned=Arc::new(());let captured=owned.clone();
        crate::rt::io().spawn(CompleteTask{work:Some(Box::pin(async move {let _owned=captured;panic!("generated task failure")})),_finished:Arc::new(Finished(Some(finished)))});
        completion_within(completed);assert_eq!(Arc::strong_count(&owned),1);
    }

    #[test]
    fn bluetooth_takes_the_route_while_its_stream_is_open_and_returns_the_wired_choice() {
        let mut policy = Policy::default();
        assert!(policy.set_bluetooth(true) && *policy.route() == Route::Bluetooth);
        assert!(!policy.set_bluetooth(true));
        // USB changes underneath do not move the route; they decide what comes back.
        assert!(!policy.update([dac(1, 2, 3, 0)]));
        assert_eq!(*policy.route(), Route::Bluetooth);
        assert!(!policy.reject(&Route::Usb(dac(1, 2, 3, 0))), "a wired route that is not current cannot be rejected");
        assert!(policy.set_bluetooth(false));
        assert_eq!(*policy.route(), Route::Usb(dac(1, 2, 3, 0)));
        assert!(policy.set_bluetooth(true) && !policy.update([]));
        assert!(policy.reject(&Route::Bluetooth) && *policy.route() == Route::Headphones);
        assert!(!policy.reject(&Route::Bluetooth));
        assert_eq!((Route::Bluetooth.pcm(), Route::Bluetooth.label()), ("q2bt".to_string(), "Bluetooth"));
        let monitor = Monitor::detached();
        let switch = monitor.bluetooth_switch();
        let before = monitor.snapshot();
        switch(true);
        assert!(monitor.changed(before.generation) && monitor.snapshot().route == Route::Bluetooth);
        let open = monitor.snapshot();
        switch(true);
        assert!(!monitor.changed(open.generation));
        switch(false);
        assert_eq!(monitor.snapshot().route, Route::Headphones);
    }

    #[test]
    fn insert_defaults_to_usb_remove_recovers_and_card_reuse_is_new_identity() {
        let (a,b) = (dac(1,2,1,0),dac(2,3,2,0));
        let mut policy = Policy::default();
        assert!(!policy.update([]));
        assert!(policy.update([a.clone()]));
        assert_eq!(policy.route(), &Route::Usb(a.clone()));
        assert!(!policy.update([a.clone()]));
        assert!(policy.update([a.clone(),b.clone()]));
        assert_eq!(policy.route(), &Route::Usb(b));
        assert!(policy.update([a.clone()]));
        assert_eq!(policy.route(), &Route::Usb(a));
        assert!(policy.update([]));
        assert_eq!(policy.route(), &Route::Headphones);
        let replacement = dac(1,4,1,0);
        assert!(policy.update([replacement.clone()]));
        assert_eq!(policy.route(), &Route::Usb(replacement));
    }

    #[test]
    fn failed_format_or_removed_device_never_loops_or_rejects_replacement() {
        let (a,b) = (dac(1,2,1,0),dac(2,3,2,0));
        let mut policy = Policy::default();
        policy.update([a.clone(), b.clone()]);
        assert!(policy.reject(&Route::Usb(a.clone())));
        assert_eq!(policy.route(), &Route::Usb(b.clone()));
        assert!(policy.reject(&Route::Usb(b.clone())));
        for _ in 0..1000 {
            assert!(!policy.update([a.clone(),b.clone()]));
            assert_eq!(policy.route(), &Route::Headphones);
        }
        let replacement = dac(1,4,1,0);
        assert!(policy.update([replacement.clone(), b]));
        assert!(!policy.reject(&Route::Usb(a)));
        assert_eq!(policy.route(), &Route::Usb(replacement));
    }

    #[test]
    fn seeded_topology_failures_always_choose_available_unrejected_route() {
        let mut seed = 0x5132_u64;
        let mut devices = Vec::new();
        let mut rejected = BTreeSet::new();
        let mut policy = Policy::default();
        for step in 0..20000 {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let port = (seed >> 32) as u32 % 8;
            match seed % 4 {
                0 => {
                    devices.retain(|device: &UsbPlayback| !device.identity.starts_with(&format!("platform/usb/1-{port}@")));
                    devices.push(dac(port, step % 127 + 1, port + 1, 0));
                }
                1 => { if !devices.is_empty() { devices.remove(port as usize % devices.len()); } }
                2 => {
                    if let Route::Usb(device) = policy.route() { rejected.insert(device.clone()); }
                    policy.reject(&policy.route().clone());
                }
                _ => {}
            }
            rejected.retain(|device| devices.contains(device));
            policy.update(devices.clone());
            match policy.route() {
                Route::Usb(device) => { assert!(devices.contains(device)); assert!(!rejected.contains(device)); }
                Route::Headphones => assert!(devices.iter().all(|device| rejected.contains(device))),
                Route::Bluetooth => unreachable!("no Bluetooth stream in this model"),
            }
            assert!(!policy.update(devices.clone()), "unchanged event changed selection at step {step}");
        }
    }

    struct Fixture(PathBuf);
    impl Fixture {
        fn new() -> Self {
            let base = std::env::var_os("Q2_GENERATED_TEST_ROOT").map(PathBuf::from)
                .unwrap_or_else(|| Path::new(env!("CARGO_MANIFEST_DIR")).join("../../build/perf"));
            let root = base.join(format!("usb-output-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
            std::fs::create_dir_all(root.join("class/sound")).unwrap();
            std::fs::create_dir_all(root.join("devices")).unwrap();
            Self(root)
        }
        fn pcm(&self, name: &str, driver: &str, address: &str) -> PathBuf {
            let device = self.0.join("devices/platform/usb/1-1");
            let interface = device.join("1-1:1.0");
            let class = self.0.join("class/sound").join(name);
            std::fs::create_dir_all(&interface).unwrap();
            std::fs::create_dir_all(&class).unwrap();
            let _ = std::fs::remove_file(interface.join("driver"));
            symlink(format!("/sys/bus/usb/drivers/{driver}"),interface.join("driver")).unwrap();
            symlink(&interface,class.join("device")).unwrap();
            for (field,value) in [("busnum","1"),("devnum",address),("idVendor","1234"),("idProduct","5678")] {
                std::fs::write(device.join(field),value).unwrap();
            }
            device
        }
    }
    impl Drop for Fixture { fn drop(&mut self) { let _ = std::fs::remove_dir_all(&self.0); } }

    #[test]
    fn actual_sysfs_fixture_requires_usb_audio_playback_and_complete_identity() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let usb = fixture.pcm("pcmC9D3p","snd-usb-audio","2");
        fixture.pcm("pcmC9D3c","snd-usb-audio","2"); // capture never selected
        let devices = discover(&blocking, &fixture.0);
        assert_eq!(devices,vec![dac(1,2,9,3)]);
        assert_eq!(devices[0].pcm(),"plughw:CARD=9,DEV=3");
        assert_eq!(Route::Usb(devices[0].clone()).label(),"USB DAC");
        std::fs::write(usb.join("devnum"),"3").unwrap();
        assert_ne!(devices,discover(&blocking, &fixture.0));
        for invalid in ["", "0", "128", "-1", "2\n3", "99;invalid"] {
            std::fs::write(usb.join("devnum"),invalid).unwrap();
            assert!(discover(&blocking, &fixture.0).is_empty());
        }
        std::fs::write(usb.join("devnum"),"2").unwrap();
        std::fs::remove_file(usb.join("1-1:1.0/driver")).unwrap();
        symlink("/sys/bus/platform/drivers/shanling-q2",usb.join("1-1:1.0/driver")).unwrap();
        assert!(discover(&blocking, &fixture.0).is_empty());
    }

    #[test]
    fn malformed_endpoint_paths_and_events_do_not_select_non_usb_outputs() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        for invalid in ["pcmC0D0c","card1","pcmC1D0p/evil","pcmC-1D0p","pcmC1D0;evilp","pcmC4294967296D0p","pcmCD0p"] {
            assert_eq!(endpoint(invalid),None);
        }
        for value in [b"SUBSYSTEM=sound".as_slice(),b"add@/devices/generated\0SUBSYSTEM=usb\0"] { assert!(relevant_event(value)); }
        for value in [b"SUBSYSTEM=power_supply".as_slice(),b"SUBSYSTEM=sound-extra\0",b"SUBSYSTEM=notusb\0"] { assert!(!relevant_event(value)); }
        let fixture = Fixture::new();
        let class = fixture.0.join("class/sound/pcmC1D0p");
        std::fs::create_dir_all(&class).unwrap();
        symlink(&fixture.0,class.join("device")).unwrap();
        assert!(discover(&blocking, &fixture.0).is_empty());
    }

    #[test]
    fn real_event_worker_initial_scan_snapshot_rejection_and_join() {
        let blocking = crate::rt::Blocking::dedicated_thread();
        let fixture = Fixture::new();
        let usb = fixture.pcm("pcmC9D3p", "snd-usb-audio", "2");
        let monitor = Monitor::at(fixture.0.clone()).unwrap();
        let state = monitor.shared.clone();
        let initial = monitor.snapshot();
        assert_eq!(initial.route, Route::Usb(dac(1, 2, 9, 3)));
        assert!(!monitor.changed(initial.generation));
        let rejected = monitor.reject(&initial);
        assert_eq!(rejected.route, Route::Headphones);
        assert!(monitor.changed(initial.generation));
        // The same production update operation used after a kernel event,
        // with generated topology. No host hardware uevent is triggered.
        std::fs::write(usb.join("devnum"), "3").unwrap();
        state.update(discover(&blocking, &fixture.0));
        assert_eq!(monitor.snapshot().route, Route::Usb(dac(1, 3, 9, 3)));
        assert_eq!(monitor.reject(&initial).route, Route::Usb(dac(1, 3, 9, 3)));
        drop(monitor);
        // Drop cancels actual reactor readiness and joins the task. Its Arc
        // must be gone, rather than leaving an undisposed background thread.
        assert_eq!(std::sync::Arc::strong_count(&state), 1);
    }

    #[test]
    fn stale_failure_cannot_reject_a_reenumeration_with_identical_metadata() {
        let (monitor,events)=Monitor::fixture();
        let same=dac(1,2,9,3);
        events.update(vec![same.clone()]);
        let old=monitor.snapshot();
        events.update(Vec::new());
        events.update(vec![same]);
        let replacement=monitor.snapshot();
        assert_eq!(old.route,replacement.route);
        assert_ne!(old.generation,replacement.generation);
        assert_eq!(monitor.reject(&old),replacement);
        assert_eq!(monitor.reject(&replacement).route,Route::Headphones);
    }

    #[test]
    fn coalesced_remove_add_and_lost_events_retire_the_old_enumeration() {
        let (monitor,events)=Monitor::fixture();
        let same=dac(1,2,9,3);
        events.update(vec![same.clone()]);
        let original=monitor.snapshot();
        for irrelevant in [
            b"ACTION=remove\0SUBSYSTEM=usb\0DEVPATH=/devices/platform/usb/1-2\0".as_slice(),
            b"ACTION=remove\0SUBSYSTEM=sound\0DEVPATH=/devices/platform/usb/1-1/1-1:1.0/sound/card9/pcmC9D3c\0",
            b"ACTION=change\0SUBSYSTEM=usb\0DEVPATH=/devices/platform/usb/1-1\0",
        ] {events.removed(irrelevant);assert_eq!(monitor.snapshot(),original);}
        events.removed(b"ACTION=remove\0SUBSYSTEM=sound\0DEVPATH=/devices/platform/usb/1-1/1-1:1.0/sound/card9/pcmC9D3p\0");
        events.update(vec![same.clone()]); // One batch's final sysfs is identical.
        let replacement=monitor.snapshot();
        assert_ne!(replacement.generation,original.generation);
        assert_eq!(replacement.route,original.route);
        assert_eq!(monitor.reject(&original),replacement);
        monitor.shared.reconcile(vec![same],true); // ENOBUFS/truncated-event rescan.
        let after_loss=monitor.snapshot();
        assert_ne!(after_loss.generation,replacement.generation);
        assert_eq!(monitor.reject(&replacement),after_loss);
    }
}
