//! Ordered, owned settings writes. Waiting for earlier writes consumes no
//! worker; a finite write retains its turn through atomic rename and fsync.
use super::{Config, Paths};
use std::{io, sync::{Arc, Mutex}};

type Result = std::result::Result<(), io::ErrorKind>;
pub(super) type Order = Arc<Mutex<Option<tokio::sync::oneshot::Receiver<()>>>>;

#[derive(Clone)]
pub struct Save {
    result: tokio::sync::watch::Receiver<Option<Result>>,
}
impl Save {
    pub async fn wait(mut self) -> Result {
        loop {
            if let Some(result) = *self.result.borrow_and_update() { return result; }
            if self.result.changed().await.is_err() { return Err(io::ErrorKind::Other); }
        }
    }
}
impl Paths {
    /// Eager ordered admission. Dropping any/all observers cannot cancel a
    /// write or let a later snapshot overtake it.
    pub fn save_config_async(&self, config: Config) -> Save {
        let (done, next) = tokio::sync::oneshot::channel();
        let previous = self.save_order.lock().unwrap().replace(next);
        let paths = self.clone();
        let (result, receive) = tokio::sync::watch::channel(None);
        crate::rt::io().spawn(async move {
            if let Some(previous) = previous { let _ = previous.await; }
            let saved = crate::rt::jobs().run_isolated(move |blocking| {
                let _turn = done;
                paths.save_config_serial(blocking, &config).map_err(|error| error.kind())
            }).await.unwrap_or(Err(io::ErrorKind::Other));
            result.send_replace(Some(saved));
        });
        Save { result: receive }
    }

    /// Only ordinary startup/tests may wait. Runtime consumers own/await a Save.
    #[cfg(test)]
    pub fn save_config(&self, config: &Config) -> io::Result<()> {
        assert!(crate::rt::may_wait_for_tasks(), "await settings persistence on runtime/pool callers");
        let save = self.save_config_async(config.clone());
        let (send, receive) = tokio::sync::oneshot::channel();
        crate::rt::io().spawn(async move { let _ = send.send(save.wait().await); });
        receive.blocking_recv().unwrap_or(Err(io::ErrorKind::Other))
            .map_err(|kind| io::Error::new(kind, "Settings could not be saved"))
    }

    #[cfg(test)]
    pub(crate) fn hold_config_saves(&self) -> tokio::sync::oneshot::Sender<()> {
        let (release, wait) = tokio::sync::oneshot::channel();
        assert!(self.save_order.lock().unwrap().replace(wait).is_none(), "hold before first save");
        release
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture(name: &str) -> Paths {
        let paths = Paths::new(crate::library::generated_test_directory(name));
        paths.create_all(&crate::rt::Blocking::dedicated_thread()).unwrap();
        paths
    }
    #[test]
    fn ordered_writes_survive_dropped_observers_without_parking_a_worker() {
        let paths = fixture("ordered-settings-save");
        let release = paths.hold_config_saves();
        let mut first = Config::default(); first.playback.volume = 17;
        drop(paths.save_config_async(first.clone()));
        crate::rt::Ui::new().unwrap().block_on(async {
            assert_eq!(tokio::time::timeout(std::time::Duration::from_secs(2), crate::rt::jobs().run_isolated(|_| 41)).await.unwrap().unwrap(), 41);
            assert!(!paths.config().exists());
            release.send(()).unwrap();
            let last = paths.save_order.lock().unwrap().take().unwrap();
            tokio::time::timeout(std::time::Duration::from_secs(2), last).await.unwrap().unwrap_err();
        });
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &paths.config()), first, "all dropped observers still leave an owned durable write");
        let mut second = first.clone(); second.playback.volume = 23;
        let mut third = second.clone(); third.playback.volume = 39;
        drop(paths.save_config_async(second));
        let final_write = paths.save_config_async(third.clone());
        crate::rt::Ui::new().unwrap().block_on(async { tokio::time::timeout(std::time::Duration::from_secs(2), final_write.wait()).await.unwrap().unwrap(); });
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &paths.config()), third);
    }
    #[test]
    fn failed_settings_write_retires_its_turn_and_identical_retry_can_commit() {
        let paths = fixture("failed-settings-save");
        std::fs::create_dir(paths.config()).unwrap();
        let mut config = Config::default(); config.playback.volume = 51;
        crate::rt::Ui::new().unwrap().block_on(async {
            assert!(paths.save_config_async(config.clone()).wait().await.is_err());
            std::fs::remove_dir(paths.config()).unwrap();
            paths.save_config_async(config.clone()).wait().await.unwrap();
        });
        assert_eq!(Config::load(&crate::rt::Blocking::dedicated_thread(), &paths.config()), config);
    }
}
