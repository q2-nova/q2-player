//! A touchscreen client exists only while the panel accepts taps.
#![forbid(unsafe_code)]

use super::InputSink;
use super::evdev::{Completed, CompletedWork, FrameClock, cancelled, read_batch, scanner};
use super::input_ffi::{Api, Event, TouchState};
use crate::gfx::{HEIGHT, WIDTH};
use crate::input::Raw;
use crate::rt;
use std::ffi::CString;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{oneshot, watch};

const NAME: &str = "Q2 touchscreen";
const TAP_RADIUS: i32 = 8;

fn discover_at(input_class: &Path) -> Option<CString> {
    let event = std::fs::read_dir(input_class)
        .ok()?
        .flatten()
        .find(|entry| {
            let name = entry.file_name();
            let event = name.to_str().and_then(|name| name.strip_prefix("event"));
            event.is_some_and(|digits| {
                !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
            }) && std::fs::read_to_string(entry.path().join("device/name"))
                .is_ok_and(|name| name.trim_end() == NAME)
        })?;
    let path = Path::new("/dev/input").join(event.file_name());
    CString::new(path.as_os_str().as_encoded_bytes()).ok()
}

#[derive(Clone, Copy)]
enum Contact {
    Up,
    Tap { x: i32, y: i32, at: Instant },
    Cancelled,
}

struct Taps {
    state: TouchState,
    contact: Contact,
    fresh: Instant,
    dropped: bool,
}

impl Taps {
    fn new(state: TouchState, fresh: Instant) -> Self {
        let contact = if state.pressure == 0 {
            Contact::Up
        } else {
            Contact::Cancelled
        };
        Self {
            state,
            contact,
            fresh,
            dropped: false,
        }
    }

    fn event(&mut self, event: &Event, clock: FrameClock) -> Option<Raw> {
        if self.dropped {
            return None;
        }
        match (event.kind, event.code) {
            (3, 0) => self.state.x = event.value,         // ABS_X
            (3, 1) => self.state.y = event.value,         // ABS_Y
            (3, 24) => self.state.pressure = event.value, // ABS_PRESSURE
            // The panel driver reports a failed contact before its forced release.
            (4, 3) if event.value == 1 => self.contact = Contact::Cancelled, // MSC_RAW
            (0, 3) => {
                // SYN_DROPPED
                self.contact = Contact::Cancelled;
                self.dropped = true;
            }
            (0, 0) => return self.frame(clock.timestamp(event)), // SYN_REPORT
            _ => {}
        }
        None
    }

    fn frame(&mut self, now: Instant) -> Option<Raw> {
        let TouchState { x, y, pressure } = self.state;
        let in_bounds = (0..HEIGHT).contains(&x) && (0..WIDTH).contains(&y);
        if let Contact::Tap {
            x: start_x,
            y: start_y,
            ..
        } = self.contact
        {
            if !in_bounds
                || pressure < 0
                || (x - start_x).pow(2) + (y - start_y).pow(2) > TAP_RADIUS.pow(2)
            {
                self.contact = Contact::Cancelled;
            }
        }
        if pressure == 0 {
            return match std::mem::replace(&mut self.contact, Contact::Up) {
                // Invert the clockwise landscape-to-scanout mapping in display.c.
                Contact::Tap {
                    x: start_x,
                    y: start_y,
                    at,
                } => Some(Raw::Touch {
                    start_x: start_y as u16,
                    start_y: (HEIGHT - 1 - start_x) as u16,
                    x: y as u16,
                    y: (HEIGHT - 1 - x) as u16,
                    at,
                }),
                _ => None,
            };
        }
        if matches!(self.contact, Contact::Up) {
            self.contact = if in_bounds && now >= self.fresh && pressure > 0 {
                Contact::Tap { x, y, at: now }
            } else {
                Contact::Cancelled
            };
        }
        None
    }
}

async fn watch_taps(
    api: Arc<Api>,
    fd: Arc<rt::Fd<Arc<Completed>>>,
    mut taps: Taps,
    sink: InputSink,
    mut cancel: watch::Receiver<()>,
    completion: Arc<Completed>,
) -> Result<(), ()> {
    loop {
        let batch = tokio::select! {
            biased;
            _ = cancel.changed() => return Ok(()),
            batch = read_batch(&api, fd.as_ref(), taps.dropped) => batch.map_err(|_| ())?,
        };
        if batch.disconnected {
            return Err(());
        }
        let clock = FrameClock::read(true);
        for event in &batch.events {
            if cancelled(&cancel) {
                return Ok(());
            }
            if let Some(tap) = taps.event(event, clock) {
                sink(tap);
            }
        }
        if taps.dropped && batch.drained {
            let (api, fd, completion) = (api.clone(), fd.clone(), completion.clone());
            let snapshot = scanner()
                .run_isolated(move |blocking| {
                    let _completion = completion;
                    let fd = fd;
                    api.touch_snapshot(&*fd, blocking)
                })
                .await
                .map_err(|_| ())?;
            if cancelled(&cancel) {
                return Ok(());
            }
            taps = Taps::new(snapshot.ok_or(())?, Instant::now());
        }
        tokio::task::yield_now().await;
    }
}

struct Session {
    cancel: Option<watch::Sender<()>>,
    ready: Option<oneshot::Receiver<Result<(), String>>>,
    completed: Option<oneshot::Receiver<()>>,
}

impl Session {
    fn start(path: CString, sink: InputSink, api: Option<Api>) -> Self {
        let (cancel, receive_cancel) = watch::channel(());
        let (send_ready, ready) = oneshot::channel();
        let (send_done, completed) = oneshot::channel();
        let completion = Arc::new(Completed(Some(send_done)));
        rt::io().spawn(async move {
            let keep = completion.clone();
            let opened = scanner()
                .run_isolated(move |blocking| {
                    let value = (|| {
                        let api = Arc::new(match api {
                            Some(api) => api,
                            None => Api::load(blocking)?,
                        });
                        let (fd, state) = api.open_touch(&path, blocking)?;
                        let fd = rt::ReactorFile::new(fd, keep.clone());
                        Ok::<_, String>((api, fd, Taps::new(state, Instant::now())))
                    })();
                    CompletedWork {
                        value,
                        _completion: keep,
                    }
                })
                .await;
            let device = opened
                .map_err(|_| "Touchscreen open stopped".to_string())
                .and_then(|opened| opened.value)
                .and_then(|(api, fd, taps)| {
                    let fd = rt::Fd::register(fd).map_err(|error| error.to_string())?;
                    Ok((api, Arc::new(fd), taps))
                });
            let (api, fd, taps) = match device {
                Ok(device) => device,
                Err(error) => {
                    let _ = send_ready.send(Err(error));
                    return;
                }
            };
            if cancelled(&receive_cancel) || send_ready.send(Ok(())).is_err() {
                return;
            }
            let result = watch_taps(
                api,
                fd,
                taps,
                sink.clone(),
                receive_cancel.clone(),
                completion.clone(),
            )
            .await;
            if result.is_err() && !cancelled(&receive_cancel) {
                log::warn!("Touchscreen input disconnected");
                sink(Raw::TouchUnavailable { at: Instant::now() });
            }
        });
        Self {
            cancel: Some(cancel),
            ready: Some(ready),
            completed: Some(completed),
        }
    }

    fn ending(&mut self) -> bool {
        let finished = self.completed.as_mut().is_some_and(|done| {
            !matches!(done.try_recv(), Err(oneshot::error::TryRecvError::Empty))
        });
        if finished {
            self.completed.take();
        }
        self.cancel.is_none() || finished
    }

    async fn close(&mut self) {
        self.cancel.take();
        if let Some(done) = self.completed.as_mut() {
            let _ = done.await;
        }
        self.completed.take();
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancel.take();
        if let Some(done) = self.completed.take() {
            assert!(
                rt::may_wait_for_tasks(),
                "touchscreen must be asynchronously closed before executor teardown"
            );
            let _ = done.blocking_recv();
        }
    }
}

#[derive(Default)]
pub(super) struct Touchscreen {
    path: Option<CString>,
    session: Option<Session>,
}

impl Touchscreen {
    pub(super) fn discover() -> Self {
        Self {
            path: discover_at(Path::new("/sys/class/input")),
            session: None,
        }
    }

    pub(super) fn available(&self) -> bool {
        self.path.is_some()
    }

    pub(super) async fn close(&mut self) {
        if let Some(session) = self.session.as_mut() {
            session.close().await;
        }
        self.session.take();
    }

    pub(super) async fn open(&mut self, sink: InputSink) -> Result<(), String> {
        self.open_with(sink, None).await
    }

    async fn open_with(&mut self, sink: InputSink, api: Option<Api>) -> Result<(), String> {
        if self.session.as_mut().is_some_and(Session::ending) {
            self.close().await;
        }
        let path = self.path.as_ref().ok_or("Touchscreen unavailable")?;
        // The slot survives a cancelled UI observer during finite evdev open.
        let session = self
            .session
            .get_or_insert_with(|| Session::start(path.clone(), sink, api));
        let Some(ready) = session.ready.as_mut() else {
            return Ok(());
        };
        let result = ready
            .await
            .unwrap_or_else(|_| Err("Touchscreen registration stopped".into()));
        session.ready.take();
        if result.is_err() {
            self.close().await;
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::super::input_ffi::fixture::{
        Fixture, HOOKS, MOCK_TEST, mock_api, mock_devices, new_device, send_event,
        writer_has_reader,
    };
    use super::*;
    use std::os::fd::OwnedFd;
    use std::time::Duration;

    #[test]
    fn discovery_reads_identity_without_opening_an_evdev_client() {
        let root = crate::library::generated_test_directory("touchscreen-discovery");
        for (event, name) in [
            ("event41", "hyn_sliderbar\n"),
            ("event42", "Q2 touchscreen\n"),
        ] {
            let device = root.join(event).join("device");
            std::fs::create_dir_all(&device).unwrap();
            std::fs::write(device.join("name"), name).unwrap();
        }
        assert_eq!(discover_at(&root).as_deref(), Some(c"/dev/input/event42"));
        std::fs::remove_file(root.join("event42/device/name")).unwrap();
        assert!(discover_at(&root).is_none());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn gesture_uses_scanout_inverse_preserves_start_and_rejects_drag_or_loss() {
        let now = Instant::now();
        let mut taps = Taps::new(TouchState::default(), now);
        let frame = |taps: &mut Taps, x, y, pressure, at| {
            taps.state = TouchState { x, y, pressure };
            taps.frame(at)
        };
        assert_eq!(frame(&mut taps, 319, 0, 1, now), None);
        assert_eq!(
            frame(&mut taps, 316, 8, 0, now),
            None,
            "combined diagonal movement exceeds the radius"
        );
        assert_eq!(frame(&mut taps, 1, 374, 1, now), None);
        assert_eq!(frame(&mut taps, 12, 374, 1, now), None);
        assert_eq!(
            frame(&mut taps, 1, 374, 0, now),
            None,
            "drag remains cancelled on return"
        );
        assert_eq!(frame(&mut taps, 319, 0, 1, now), None);
        assert_eq!(
            frame(&mut taps, 315, 2, 0, now + Duration::from_secs(1)),
            Some(Raw::Touch {
                start_x: 0,
                start_y: 0,
                x: 2,
                y: 4,
                at: now
            })
        );

        assert_eq!(frame(&mut taps, 0, 374, 1, now), None);
        let clock = FrameClock::read(false);
        taps.event(
            &Event {
                kind: 4,
                code: 3,
                value: 1,
                ..Default::default()
            },
            clock,
        );
        assert_eq!(
            frame(&mut taps, 0, 374, 0, now),
            None,
            "driver cancellation is not a release tap"
        );
        assert_eq!(frame(&mut taps, 0, 374, 1, now), None);
        taps.event(
            &Event {
                kind: 0,
                code: 3,
                ..Default::default()
            },
            clock,
        );
        taps.event(
            &Event {
                kind: 3,
                code: 24,
                value: 0,
                ..Default::default()
            },
            clock,
        );
        assert_eq!(taps.event(&Event::default(), clock), None);
        let mut taps = Taps::new(
            TouchState {
                x: 0,
                y: 374,
                pressure: 1,
            },
            now,
        );
        assert_eq!(
            frame(&mut taps, 0, 374, 0, now),
            None,
            "suppress snapshot's held contact"
        );
        assert_eq!(
            frame(&mut taps, 0, 374, 1, now - Duration::from_millis(1)),
            None
        );
        assert_eq!(
            frame(&mut taps, 0, 374, 0, now),
            None,
            "suppress queued pre-snapshot contacts"
        );
        assert_eq!(frame(&mut taps, 0, 374, 1, now), None);
        assert_eq!(
            frame(&mut taps, 0, 374, 0, now),
            Some(Raw::Touch {
                start_x: 374,
                start_y: 319,
                x: 374,
                y: 319,
                at: now
            })
        );
    }

    fn panel_frame(fd: &OwnedFd, axes: &[(u16, i32)]) {
        for &(code, value) in axes {
            send_event(
                fd,
                &Event {
                    kind: 3,
                    code,
                    value,
                    ..Event::default()
                },
            );
        }
        let time = super::super::system::monotonic_time().unwrap();
        send_event(
            fd,
            &Event {
                seconds: time.as_secs() as i64,
                microseconds: time.subsec_micros() as i32,
                ..Event::default()
            },
        );
    }

    #[test]
    fn evdev_client_opens_only_on_enable_and_joins_close_before_reopening() {
        let _exclusive = MOCK_TEST.lock().unwrap();
        let _fixture = Fixture::new();
        let (send, receive) = std::sync::mpsc::channel();
        let sink: InputSink = Arc::new(move |event| {
            let _ = send.send(event);
        });
        let mut touch = Touchscreen {
            path: Some(c"/dev/input/event42".into()),
            session: None,
        };
        assert!(touch.available());
        assert_eq!(HOOKS.lock().unwrap().opens, [0, 0, 0]);

        let ui = rt::Ui::new().unwrap();
        let panel = new_device(2);
        ui.block_on(async {
            touch
                .open_with(sink.clone(), Some(mock_api()))
                .await
                .unwrap();
            touch
                .open_with(sink.clone(), Some(mock_api()))
                .await
                .unwrap();
        });
        mock_devices().lock().unwrap().readers[2].take();
        assert!(writer_has_reader(&panel));
        assert_eq!(HOOKS.lock().unwrap().opens, [0, 0, 1]);
        panel_frame(&panel, &[(0, 35), (1, 70), (24, 1)]);
        ui.block_on(touch.close());
        assert!(
            !writer_has_reader(&panel),
            "disable must close the last reader before returning"
        );
        assert!(
            touch.available(),
            "settings capability remains available during reset"
        );

        let panel = new_device(2);
        ui.block_on(touch.open_with(sink, Some(mock_api())))
            .unwrap();
        panel_frame(&panel, &[(24, 0)]);
        // Keep generous timing slack between snapshot and the generated gesture.
        let started = Instant::now() - Duration::from_millis(1);
        panel_frame(&panel, &[(0, 22), (1, 75), (24, 1)]);
        panel_frame(&panel, &[(24, 0)]);
        let tap = receive.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(
            matches!(tap, Raw::Touch { start_x: 75, start_y: 297, x: 75, y: 297, at } if at >= started)
        );
        assert!(receive.try_recv().is_err());
        assert_eq!(HOOKS.lock().unwrap().opens, [0, 0, 2]);
        drop(panel);
        assert!(matches!(
            receive.recv_timeout(Duration::from_secs(2)).unwrap(),
            Raw::TouchUnavailable { at } if at >= started
        ));
        ui.block_on(touch.close());
        assert!(receive.try_recv().is_err());
    }
}
