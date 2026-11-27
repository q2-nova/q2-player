#![forbid(unsafe_code)]
//! Ordered backlight operations and fresh battery reads. Accepted writes own
//! their turn until the finite syscall job completes, even if a waiter leaves.
use super::Battery;
use std::{path::PathBuf, sync::Arc};
use tokio::sync::{oneshot, watch};

pub(super) struct Backlight {
    path: Arc<PathBuf>,
    tail: Option<oneshot::Receiver<()>>,
    #[cfg(test)]
    hold: Option<(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>)>,
}
pub(super) struct WriteTicket(watch::Receiver<bool>);
impl WriteTicket {
    #[cfg(test)]
    pub(super) fn fixture() -> (watch::Sender<bool>, Self) {
        let (done, waiting) = watch::channel(false);
        (done, Self(waiting))
    }
    pub(super) async fn wait(&mut self) {
        while !*self.0.borrow_and_update() {
            if self.0.changed().await.is_err() { return; }
        }
    }
}
impl Backlight {
    pub(super) fn new(path: PathBuf) -> Self {
        Self { path: Arc::new(path), tail: None, #[cfg(test)] hold: None }
    }
    pub(super) fn write(&mut self, level: u8) -> WriteTicket {
        let (done, next) = oneshot::channel();
        let previous = self.tail.replace(next);
        let (ready, ticket) = watch::channel(false);
        let path = self.path.clone();
        #[cfg(test)]
        let hold = self.hold.take();
        crate::rt::io().spawn(async move {
            if let Some(previous) = previous { let _ = previous.await; }
            let result = crate::rt::jobs().run_isolated(move |blocking| {
                let _turn = done;
                #[cfg(test)]
                if let Some((entered, release)) = hold {
                    entered.send(()).unwrap();
                    release.recv_timeout(std::time::Duration::from_secs(5)).unwrap();
                }
                write_backlight(blocking, &path, level)
            }).await;
            match result {
                Ok(Ok(())) => {}
                Ok(Err(error)) => log::error!("DRM test backlight: {error}"),
                Err(_) => log::error!("DRM test backlight worker stopped"),
            }
            ready.send_replace(true);
        });
        WriteTicket(ticket)
    }
    pub(super) async fn dark_readback(&self) -> bool {
        let path = self.path.clone();
        crate::rt::jobs().run_isolated(move |blocking| actual_brightness(blocking, &path))
            .await.is_ok_and(|value| value == Some(0))
    }
    pub(super) fn write_ordinary(&mut self, level: u8) {
        assert!(crate::rt::may_wait_for_tasks(), "await backlight work on runtime/pool callers");
        let mut ticket = self.write(level);
        let (reply, completed) = oneshot::channel();
        crate::rt::io().spawn(async move { ticket.wait().await; let _ = reply.send(()); });
        let _ = completed.blocking_recv();
    }
}
fn actual_brightness(_blocking: &crate::rt::Blocking, path: &std::path::Path) -> Option<u32> {
    std::fs::read_to_string(path.join("actual_brightness")).ok()?.trim().parse().ok()
}
fn write_backlight(_blocking: &crate::rt::Blocking, path: &std::path::Path, level: u8) -> std::io::Result<()> {
    let maximum = std::fs::read_to_string(path.join("max_brightness"))
        .ok().and_then(|text| text.trim().parse::<u32>().ok()).unwrap_or(0);
    let value = backlight_value(level, maximum);
    std::fs::write(path.join("brightness"), value.to_string())
}
pub(super) async fn battery(path: PathBuf) -> Battery {
    crate::rt::jobs().run_isolated(move |blocking| read_battery(blocking, &path)).await.unwrap_or_default()
}
fn read_battery(_blocking: &crate::rt::Blocking, path: &std::path::Path) -> Battery {
    let read = |name| std::fs::read_to_string(path.join(name)).unwrap_or_default();
    Battery {
        level: read("cw2015-battery/capacity").trim().parse::<u8>().ok().filter(|value| *value <= 100),
        plugged: read("axp20x-usb/online").trim() == "1",
        charging: read("axp20x-battery/status").trim() == "Charging",
    }
}

pub(super) fn backlight_value(level: u8, maximum: u32) -> u32 {
    (u32::from(level.min(100)) * maximum).div_ceil(100)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;
    struct Release(Option<mpsc::Sender<()>>);
    impl Release { fn now(&mut self) { if let Some(send) = self.0.take() { let _ = send.send(()); } } }
    impl Drop for Release { fn drop(&mut self) { self.now(); } }
    fn hold(backlight: &mut Backlight) -> (mpsc::Receiver<()>, Release) {
        let (entered, seen) = mpsc::channel();
        let (release, wait) = mpsc::channel();
        backlight.hold = Some((entered, wait));
        (seen, Release(Some(release)))
    }
    #[test]
    fn cancelled_backlight_waiters_preserve_write_order_and_final_blank() {
        let root = crate::library::generated_test_directory("backlight-owner");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("max_brightness"), "100\n").unwrap();
        let mut backlight = Backlight::new(root.clone());
        let (first, mut release_first) = hold(&mut backlight);
        let discarded = backlight.write(17);
        first.recv_timeout(Duration::from_secs(3)).unwrap();
        let (second, mut release_second) = hold(&mut backlight);
        let discarded_second = backlight.write(83);
        drop(discarded);
        drop(discarded_second);
        assert!(matches!(second.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let mut blank = backlight.write(0);
        release_first.now();
        second.recv_timeout(Duration::from_secs(3)).unwrap();
        assert_eq!(std::fs::read_to_string(root.join("brightness")).unwrap(), "17");
        release_second.now();
        crate::rt::Ui::new().unwrap().block_on(async {
            tokio::time::timeout(Duration::from_secs(3), blank.wait()).await.unwrap();
        });
        assert_eq!(std::fs::read_to_string(root.join("brightness")).unwrap(), "0");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn battery_reads_current_values_and_keeps_missing_state_unknown() {
        let root = crate::library::generated_test_directory("battery-owner");
        for path in ["cw2015-battery", "axp20x-usb", "axp20x-battery"] { std::fs::create_dir_all(root.join(path)).unwrap(); }
        std::fs::write(root.join("cw2015-battery/capacity"), "64\n").unwrap();
        std::fs::write(root.join("axp20x-usb/online"), "1\n").unwrap();
        std::fs::write(root.join("axp20x-battery/status"), "Charging\n").unwrap();
        let ui = crate::rt::Ui::new().unwrap();
        assert_eq!(ui.block_on(battery(root.clone())), Battery { level: Some(64), plugged: true, charging: true });
        std::fs::write(root.join("cw2015-battery/capacity"), "17\n").unwrap();
        std::fs::write(root.join("axp20x-usb/online"), "0\n").unwrap();
        assert_eq!(ui.block_on(battery(root.clone())), Battery { level: Some(17), plugged: false, charging: true });
        std::fs::write(root.join("cw2015-battery/capacity"), "101\n").unwrap();
        assert_eq!(ui.block_on(battery(root.clone())).level, None);
        std::fs::remove_file(root.join("cw2015-battery/capacity")).unwrap();
        assert_eq!(ui.block_on(battery(root.clone())).level, None);
        std::fs::remove_dir_all(root).unwrap();
    }
}
