//! Streaming coordination and reader-held transfer lifetime.
//!
//! Cache-slot admission, playback priority, cancellation and the player's source
//! stay together here. Persistence/publication belongs to cache_storage, bounded
//! writes to transfer_io, scheduling to prefetch and account replacement to
//! configuration. Library still owns the same registry and generation guards.
use super::*;

/// One cache transfer under way: its own partial file, its progress, a
/// cancellation signal, and how many readers hold it
/// (player streams and the fetch-ahead worker), counted under the lock.
pub(super) struct Transfer {
    pub(super) generation: u32,
    pub(super) part: PathBuf,
    pub(super) progress: Arc<Progress>,
    pub(super) cancel: Arc<transfer_io::Cancel>,
    /// Its serial in the playback trace (crate::playtrace); never a song ID.
    pub(super) trace_id: u32,
    pub(super) journal_id: u64,
    pub(super) users: usize,
    /// Its last reader left. The transfer goes on (a seek, a pause or a
    /// failed probe reopens the same song and rejoins it) until the player
    /// opens another song or fetch-ahead moves on (`cancel_orphans`).
    pub(super) orphaned: bool,
    /// Started for playback (not by fetch-ahead): fetch-ahead waits for it.
    pub(super) playback: bool,
}
pub(super) type InFlight = HashMap<(String, String), Transfer>;

/// One foreground book range owns priority until success, error or cancellation.
/// Holding only during network work lets background downloads use buffered gaps.
pub(super) struct SpokenPlayback(Arc<Library>);
impl Drop for SpokenPlayback {
    fn drop(&mut self) {
        self.0.spoken_playback_requests.fetch_sub(1, Ordering::AcqRel);
        self.0.background_changed();
    }
}
impl Library {
    pub(super) fn spoken_playback_active(&self) -> bool {
        self.spoken_playback_requests.load(Ordering::Acquire)>0
    }
    pub(super) fn begin_spoken_playback(self:&Arc<Self>) -> SpokenPlayback {
        self.spoken_playback_requests.fetch_add(1,Ordering::AcqRel);
        self.background_changed();
        SpokenPlayback(self.clone())
    }
}

impl Transfer {
    /// The transfer as the slot machine sees it (`q2_machines::transfer`,
    /// docs/state-machines.md). Read and applied under the `inflight` lock.
    pub(super) fn slot(&self) -> xfer::Slot {
        xfer::Slot { users: self.users, orphaned: self.orphaned, cancelled: self.cancel.is_cancelled(), playback: self.playback }
    }
    /// Stores a transition's result. The cancel flag is shared with the
    /// transfer task and set only by `Effect::Cancel`.
    pub(super) fn apply(&mut self, slot: xfer::Slot) {
        self.users = slot.users;
        self.orphaned = slot.orphaned;
        self.playback = slot.playback;
    }
}

/// A transfer at least this far along is kept when its last reader leaves
/// (it is cached and may be wanted again); below it, the transfer stops.
const RETAIN_PERCENT: u64 = 80;

/// Percent downloaded for the playback trace; 255 when the length is unknown.
pub(super) fn fill_percent(fill: &crate::audio::source::Fill) -> u32 {
    // Widened: saturating in 64 bits reported 1 % for a complete huge file.
    fill.total.filter(|t| *t > 0).map_or(255, |t| (u128::from(fill.written) * 100 / u128::from(t)).min(100) as u32)
}

pub(super) fn retain_obsolete(fill: &crate::audio::source::Fill) -> bool {
    // Widened: the server's length is not trusted to leave room for the product.
    fill.done || fill.total.is_some_and(|t| t > 0 && u128::from(fill.written) * 100 >= u128::from(t) * u128::from(RETAIN_PERCENT))
}

/// Whether a finished transfer may be kept as the song. A raw stream that
/// ended before its declared Content-Length is a dropped connection, not a
/// song; transcodes declare an estimate, so their length is not compared.
pub(super) fn transfer_complete(variant: &str, size: u64, declared: Option<u64>) -> bool {
    variant != "raw" || declared.is_none_or(|total| size >= total)
}

/// A reader's hold on a transfer. When the last hold drops the transfer is
/// orphaned; `cancel_orphans` stops orphans below RETAIN_PERCENT once the
/// player opens another song, so rapid skips do not stack competing
/// downloads and card writes. Cancellation wakes a stalled HTTP wait directly
/// and joins any active card write before retiring the partial. Completed bytes
/// become non-cancellable only when the publication worker claims completion.
pub(crate) struct TransferUse {
    pub(super) lib: Arc<Library>,
    pub(super) key: (String, String),
    pub(super) cancel: Arc<transfer_io::Cancel>,
}

impl Drop for TransferUse {
    fn drop(&mut self) {
        // Never panic here (this can run while unwinding).
        let mut inflight = self.lib.inflight.lock().unwrap_or_else(|e| e.into_inner());
        let Some(t) = inflight.get_mut(&self.key) else { return };
        let own = Arc::ptr_eq(&t.cancel, &self.cancel);
        let (next, effect) = xfer::transition(Some(t.slot()), xfer::Event::Released { own });
        if let Some(next) = next { t.apply(next); }
        if effect == xfer::Effect::Orphaned {
            let fill = t.progress.snapshot();
            self.lib.trace().event(crate::playtrace::Kind::XferOrphan, t.trace_id, fill_percent(&fill), (fill.written / 1024) as u32);
        }
    }
}

/// A playback source that keeps its transfer alive while it is read.
struct HeldSource<R> {
    inner: R,
    _hold: Option<TransferUse>,
}

impl<R: Read> Read for HeldSource<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> { crate::audio::callback::require()?; self.inner.read(buf) }
}
impl<R: std::io::Seek> std::io::Seek for HeldSource<R> {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> { crate::audio::callback::require()?; self.inner.seek(pos) }
}
impl<R: symphonia::core::io::MediaSource> symphonia::core::io::MediaSource for HeldSource<R> {
    fn is_seekable(&self) -> bool { crate::audio::callback::assert_allowed(); self.inner.is_seekable() }
    fn byte_len(&self) -> Option<u64> { crate::audio::callback::assert_allowed(); self.inner.byte_len() }
}


impl Library {
    /// Removes a media file in either layout; never one outside the new tree.
    pub(super) fn remove_media_file(&self, blocking: &crate::rt::Blocking, path: &Path) {
        if self.paths.can_modify_media(path) { let _ = self.paths.remove_media(blocking, path); }
    }

    pub(super) fn cache_path(&self, song: &Song, variant: &str) -> PathBuf {
        let name = format!("{}.{}.{}", safe(&song.id), variant, extension(song, variant));
        self.paths.sharded(crate::shard::Store::Songs, name.as_ref())
    }

    /// Starts fetching a song into the cache (or joins a fetch under way),
    /// with a hold that keeps it going; `playback` marks a player stream.
    pub(super) fn start_fetch(self: &Arc<Self>, song: &Song, variant: &str, playback: bool) -> Result<(PathBuf, Arc<Progress>, Option<TransferUse>), LibError> {
        self.fetch_in_burst(song, variant, playback, 0)
    }

    pub(super) fn fetch_in_burst(self: &Arc<Self>, song: &Song, variant: &str, playback: bool, burst: u64) -> Result<(PathBuf, Arc<Progress>, Option<TransferUse>), LibError> {
        assert!(crate::rt::may_wait_for_tasks(), "await stream preparation on runtime/pool callers");
        let lib = self.clone(); let song = song.clone(); let variant = variant.to_owned();
        shelf_browse::wait(async move { lib.fetch_in_burst_async(song, variant, playback, burst).await })
    }

    /// Called only after an owned cache snapshot and finite file inspection,
    /// with the publication permit retained until the slot is joined/installed.
    pub(super) fn start_fetch_prepared(self: &Arc<Self>, blocking: &crate::rt::Blocking, song: &Song, variant: &str, playback: bool, burst: u64, request: u64,
        cached: Option<(PathBuf, u64)>, expected_generation: u32, _gate: offline_art::PublicationPermit) -> Result<(PathBuf, Arc<Progress>, Option<TransferUse>), LibError> {
        self.start_fetch_prepared_using(blocking, song, variant, playback, burst, request, cached,
            expected_generation, _gate, |path| std::fs::File::create_new(path))
    }

    pub(super) fn start_fetch_prepared_using(self: &Arc<Self>, blocking: &crate::rt::Blocking, song: &Song, variant: &str, playback: bool, burst: u64, request: u64,
        cached: Option<(PathBuf, u64)>, expected_generation: u32, _gate: offline_art::PublicationPermit,
        create: impl FnOnce(&Path) -> std::io::Result<std::fs::File>) -> Result<(PathBuf, Arc<Progress>, Option<TransferUse>), LibError> {
        let key = (song.id.clone(), variant.to_string());
        let mut inflight = self.inflight.lock().unwrap();
        if self.client_generation() != expected_generation { return Err(LibError::Offline); }
        if let Some(joined) = self.join_fetch_request(&mut inflight, &key, playback, burst, request, expected_generation) { return Ok(joined); }
        if let Some((path, size)) = cached {
            let progress = Progress::new();
            progress.advance(size);
            progress.finish_cached(CachePublication::Hit);
            self.refill.record(RefillEvent::Resolved { burst, request, transfer: 0, kind: 3 });
            return Ok((path, progress, None));
        }
        // Async callers also take this registry mutex. Card creation and the
        // client snapshot must never extend its critical section. The owned
        // publication permit still excludes another preparation/publication.
        drop(inflight);
        let (client, generation) = {
            if self.offline_mode() || (!playback && !self.maintenance_link_ready()) { return Err(LibError::Offline); }
            let slot = self.client.read().unwrap();
            if self.client_generation() != expected_generation { return Err(LibError::Offline); }
            (slot.clone().ok_or(LibError::NotConfigured)?, expected_generation)
        };
        let dest = self.cache_path(song, variant);
        // Each transfer writes its own partial file: a cancelled one may
        // still be removing its file when the song is requested again. The
        // partial directory stays small; publication renames into the shard.
        let part = self.paths.stream_partials().join(format!("{}.{:08x}.part",
            dest.file_name().expect("cache file name").to_string_lossy(), fastrand::u32(..)));
        let output = transfer_io::FileOutput::new(create(&part).map_err(|e| LibError::Server(format!("card: {e}")))?);
        let mut inflight = self.inflight.lock().unwrap();
        // Configuration admission can invalidate this account during card I/O;
        // an old opener must neither publish a slot nor leak its unused partial.
        let decision = if self.client_generation() != expected_generation {
            Err(LibError::Offline)
        } else if let Some(joined) = self.join_fetch_request(&mut inflight, &key, playback, burst, request, expected_generation) {
            Ok(Some(joined))
        } else if !playback && !self.maintenance_link_ready() {
            Err(LibError::Offline)
        } else {
            Ok(None)
        };
        match decision {
            Ok(None) => {}
            completed => {
                drop(inflight);
                drop(output);
                self.remove_media_file(blocking, &part);
                return completed.map(Option::unwrap);
            }
        }
        let slot = inflight.get(&key).map(Transfer::slot);
        let progress = Progress::new();
        let cancel = Arc::new(transfer_io::Cancel::default());
        let trace = self.trace();
        let trace_id = trace.transfer_start(playback);
        // Nothing joinable (no slot, or a cancelled transfer still winding
        // down): the new transfer takes the slot with its first hold.
        let (started, effect) = xfer::transition(slot, xfer::Event::Started { playback });
        debug_assert_eq!(effect, xfer::Effect::Spawn);
        let started = started.unwrap_or(xfer::Slot { users: 1, orphaned: false, cancelled: false, playback });
        inflight.insert(key.clone(), Transfer { generation, part: part.clone(), progress: progress.clone(), cancel: cancel.clone(), trace_id, journal_id: request,
            users: started.users, orphaned: started.orphaned, playback: started.playback });
        drop(inflight);
        self.refill.record(RefillEvent::Resolved { burst, request, transfer: request, kind: 1 });
        let hold = TransferUse { lib: self.clone(), key: key.clone(), cancel: cancel.clone() };
        let lib = self.clone();
        let song = song.clone();
        let variant = variant.to_string();
        let prog = progress.clone();
        let part2 = part.clone();
        crate::rt::io().spawn(async move {
                // A stream the player waits for does not fail on a dozing radio.
                if playback { lib.await_radio_for_playback(&cancel).await; }
                let mut req = client.stream_request(&song.id, &stream_options(&variant));
                if variant != "raw" {
                    req = req.param("estimateContentLength", true);
                }
                // A song the player waits for is foreground; fetch-ahead is background.
                let priority = if playback { q2_machines::server::Priority::Foreground } else { q2_machines::server::Priority::Background };
                let yielding = (!playback).then(|| transfer_io::fetch_ahead_priority(lib.clone()));
                let result = lib.fetch_file_async(client.clone(), req, output, prog.clone(), cancel.clone(), priority, yielding).await.and_then(|size| {
                    let declared = prog.snapshot().total;
                    if transfer_complete(&variant, size, declared) { Ok(size) }
                    else { Err(LibError::Server(format!("stream ended at {size} of {} bytes", declared.unwrap_or(0)))) }
                });
                lib.complete_cache(cache_storage::Completion {
                    song, variant, key, part: part2, dest, progress: prog, cancel,
                    trace, trace_id, request, burst, generation,
                }, result).await;
            });
        Ok((part, progress, Some(hold)))
    }

    /// A player stream is still downloading: fetch-ahead waits for it, so the
    /// playing song keeps the radio and the card to itself.
    pub(super) fn playback_transfer_running(&self) -> bool {
        self.spoken_playback_active() || self.inflight.lock().unwrap().values().any(|t| t.playback && !t.cancel.is_cancelled() && !t.progress.snapshot().done)
    }

    /// Stops orphaned transfers below RETAIN_PERCENT except those `keep`
    /// names: the player opened another song, or fetch-ahead moved on.
    pub(super) fn cancel_orphans(&self, keep: impl Fn(&(String, String)) -> bool) -> usize {
        let mut n = 0;
        for (key, t) in self.inflight.lock().unwrap().iter() {
            let fill = t.progress.snapshot();
            let sweep = xfer::Event::Sweep { keep: keep(key), retain: retain_obsolete(&fill) };
            if xfer::transition(Some(t.slot()), sweep).1 == xfer::Effect::Cancel && t.cancel.cancel() {
                self.trace().event(crate::playtrace::Kind::XferCancel, t.trace_id, fill_percent(&fill), (fill.written / 1024) as u32);
                n += 1;
            }
        }
        if n > 0 {
            self.background_changed();
            log::info!("stopped {n} obsolete stream(s)");
        }
        n
    }

    /// Fetch-ahead waits while a player stream is still downloading, up to
    /// PLAYBACK_PRIORITY_WAIT; it gives up on a newer queue or when the
    /// link is unavailable. The UI replans the refill on address readiness.
    /// Leave the latest queue unconsumed for the caller. Register
    /// before checking readiness so completion/offline cannot lose a wake.
    pub(super) async fn wait_playback_priority(&self, queue: &mut tokio::sync::watch::Receiver<Option<Vec<Song>>>) -> PriorityWait {
        let until = tokio::time::Instant::now() + PLAYBACK_PRIORITY_WAIT;
        loop {
            let changed = self.priority_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            if queue.has_changed().unwrap_or(true) { return PriorityWait::Newer; }
            if !self.is_online() || !self.maintenance_link_ready() { return PriorityWait::Offline; }
            if !self.playback_transfer_running() { return PriorityWait::Clear; }
            if tokio::time::Instant::now() >= until { return PriorityWait::Timeout; }
            tokio::select! {
                biased;
                _ = queue.changed() => return PriorityWait::Newer,
                () = changed => {},
                () = tokio::time::sleep_until(until) => return PriorityWait::Timeout,
            }
        }
    }

    /// Partial files left by an earlier run (killed mid-transfer); never one
    /// a transfer of this run owns (each run's names carry a fresh nonce).
    /// Earlier versions kept partial files beside the cache files.
    pub(super) fn sweep_partials(&self, blocking: &crate::rt::Blocking) -> Vec<PathBuf> {
        let mut candidates: Vec<PathBuf> = Vec::new();
        for directory in [self.paths.stream_partials(), self.paths.stream_cache()] {
            let Ok(entries) = std::fs::read_dir(directory) else { continue };
            candidates.extend(entries.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|e| e == "part")));
        }
        let stale: Vec<PathBuf> = {
            let inflight = self.inflight.lock().unwrap();
            candidates.into_iter().filter(|p| !inflight.values().any(|t| &t.part == p)).collect()
        };
        let mut deletions = Vec::new();
        for path in stale {
            if path.file_name().is_some_and(|name| name.as_encoded_bytes().starts_with(b"delete-")) {
                deletions.push(path);
            } else {
                let _ = crate::shard::remove(blocking, &path);
            }
        }
        deletions
    }

    /// Owns the asynchronous media request and joins every bounded card write.
    pub(super) async fn fetch_file_async(
        self: &Arc<Self>,
        client: Arc<Client>,
        request: Request,
        output: transfer_io::FileOutput,
        progress: Arc<Progress>,
        cancel: Arc<transfer_io::Cancel>,
        priority: q2_machines::server::Priority,
        yielding: Option<transfer_io::Priority>,
    ) -> Result<u64, LibError> {
        let response = tokio::select! {
            biased;
            () = cancel.cancelled() => Err(LibError::Server("cancelled".into())),
            response = client.open_async(&request, 0) => {
                // Configuration guards belong to finite work, including the
                // completion of an otherwise asynchronous media request.
                let lib = self.clone(); let selected = client.clone();
                crate::rt::spawn_blocking(move |_| {
                    let slot = lib.client.read().unwrap();
                    if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &selected)) {
                        lib.observe(&response, priority);
                    }
                    response.map_err(LibError::from)
                }).await.map_err(|_| LibError::Server("Stream response worker stopped".into()))?
            },
        };
        let response = match response {
            Ok(response) => response,
            Err(error) => {
                crate::rt::spawn_blocking(move |_| drop(output)).await
                    .map_err(|_| LibError::Server("Unable to close stream output".into()))?;
                return Err(error);
            },
        };
        progress.set_total(response.content_length);
        let result = transfer_io::copy_file(response.response.into_decoded(), output, progress, cancel, Some(self.trace()),None,None,yielding).await;
        match result {
            Ok(0) => Err(LibError::Server("empty response".into())),
            Ok(written) => Ok(written),
            Err(transfer_io::Failure::Cancelled) => Err(LibError::Server("cancelled".into())),
            Err(transfer_io::Failure::Length) => Err(LibError::Server("incomplete stream".into())),
            Err(transfer_io::Failure::Card(error) | transfer_io::Failure::Sync(error)) => Err(LibError::Server(format!("card: {error}"))),
            Err(transfer_io::Failure::Network(error)) => {
                let lib = self.clone();
                let message = format!("stream: {error}");
                crate::rt::spawn_blocking(move |_| {
                    let slot = lib.client.read().unwrap();
                    if slot.as_ref().is_some_and(|current| Arc::ptr_eq(current, &client)) { lib.observe_failure(&subsonic::Error::Io(error), priority); }
                }).await.map_err(|_| LibError::Server("Stream response worker stopped".into()))?;
                Err(LibError::Server(message))
            },
        }
    }
}

impl Tracks {
    pub(super) fn open_track(&self, blocking: &crate::rt::Blocking, song: &Song, interrupt: &Arc<crate::audio::interrupt::Interrupt>, complete: bool) -> Result<OpenedTrack, String> {
        let lib = &self.0;
        // Opening another song makes an orphaned stream obsolete (a skip);
        // reopening the same one (seek, pause, probe retry) rejoins it.
        lib.cancel_orphans(|k| k.0 == song.id);
        if let Some(spoken)=&song.spoken {
            let id = song.id.clone();
            let downloaded = lib.db_worker.run_blocking(move |blocking, db| db.spoken_download(blocking, &id))
                .map_err(|_| "Unable to read downloaded audiobook index")?;
            if let Some(path) = downloaded.filter(|path| lib.paths.can_modify_media(path)) {
                match lib.paths.open_media(blocking, &path) {
                    Ok(source) => return Ok(OpenedTrack {source:Box::new(source),ext:song.suffix.clone(),origin:Origin::Offline,radio_metadata:None}),
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                    Err(_) => return Err("Downloaded audiobook file unavailable".into()),
                }
            }
            return match &spoken.source {
                subsonic::SpokenSource::Local { relative_path } => {
                    let file=std::fs::File::open(lib.local_book_path(blocking, relative_path)?).map_err(|_|"Audiobook file unavailable")?;
                    Ok(OpenedTrack {source:Box::new(file),ext:Some("m4b".into()),origin:Origin::Offline,radio_metadata:None})
                },
                subsonic::SpokenSource::Audiobookshelf {namespace,content_url,..} => {
                    if lib.offline_mode() {return Err("Audiobookshelf streaming needs a connection".into());}
                    let (client,cached)={let slot=lib.shelf.read().unwrap();(slot.client.clone().ok_or("Set up Audiobookshelf and its selected connection")?,slot.namespace.clone())};
                    if client.cached_namespace().is_some_and(|account|account!=namespace) || (client.cached_namespace().is_none() && cached!=*namespace){return Err("Audiobookshelf account changed; select this book again".into());}
                    let total=song.size.filter(|n|*n>0).ok_or("Audiobookshelf did not provide file size")?;
                    let url=content_url.clone();let current=lib.clone();let account=namespace.clone();
                    let window=crate::audio::source::spoken_window_bytes(total,song.duration);
                    let source=crate::audio::source::RangeReader::with_async_window(total,interrupt.clone(),Arc::new(move |offset,length| {
                        let (current,client,url,account)=(current.clone(),client.clone(),url.clone(),account.clone());
                        Box::pin(async move {
                            let _priority=current.begin_spoken_playback();
                            let changed=||std::io::Error::other("Audiobookshelf settings changed; select the item again");
                            let check=|| {
                                let (current,client)=(current.clone(),client.clone());
                                crate::rt::spawn_blocking(move |_| !current.offline_mode() && current.shelf_current(&client))
                            };
                            if !check().await.map_err(|_|changed())? {return Err(changed());}
                            if client.namespace_async().await.map_err(|_|changed())?!=account {return Err(changed());}
                            let bytes=client.range_async(&url,offset,length,total).await.map_err(|e|std::io::Error::other(e.to_string()))?;
                            if !check().await.map_err(|_|changed())? {return Err(changed());}
                            Ok(Some(bytes))
                        })
                    }),window);
                    Ok(OpenedTrack {source:Box::new(source),ext:song.suffix.clone(),origin:Origin::Streaming,radio_metadata:None})
                }, _=>Err("Unknown spoken media source".into())
            };
        }
        if let Some(url) = &song.radio_url {
            if lib.offline_mode() { return Err("Radio needs a connection".into()); }
            // Public stations connect directly, even with Tailscale enabled:
            // relaying them through tailscaled only adds CPU and copies.
            return Ok(OpenedTrack::radio(crate::audio::radio::Radio::open(url.clone(), None, interrupt.clone())));
        }
        if let Some((path, origin, ext)) = lib.find_local(song) {
            // A file found flat may have moved to its shard since.
            let f = lib.paths.open_media(blocking, &path).map_err(|e| format!("{}: {e}", path.display()))?;
            let source=crate::audio::readahead::ReadAhead::source(blocking, f,interrupt.clone()).map_err(|e|format!("Unable to buffer playback: {e}"))?;
            return Ok(OpenedTrack { source, ext: Some(ext), origin, radio_metadata: None });
        }
        // The stream request itself establishes reachability. An old offline
        // observation must not add a ping round trip before the first byte.
        // Transfer admission still enforces the owner's explicit Offline Mode,
        // and the response publishes online state only for its current account.
        let v = variant(song, lib.settings.read().unwrap().stream_quality);
        let (part, progress, hold) = lib.start_fetch(song, &v, true).map_err(|e| e.to_string())?;
        // How far a fetch-ahead got before the player needed this song.
        { let fill = progress.snapshot(); crate::ux::record(crate::ux::Mark::StreamOpen, crate::ux::stream_open_value(fill.written, fill.total)); }
        // The buffering worker has its own lifetime. A song's old blocked read
        // must not retain the player's command interrupt across a new song.
        let read_cancel=if song.spoken.is_none() {Arc::new(crate::audio::interrupt::Interrupt::new(false))}else{interrupt.clone()};
        let reader = match ProgressiveReader::open(blocking, &part, progress.clone(), read_cancel.clone()) {
            Ok(reader) => reader,
            // A very small download can finish and be renamed between open
            // and reader construction. Reuse the published completed file.
            Err(e) => match lib.find_local(song) {
                Some((path,origin,ext)) => return Ok(OpenedTrack {
                    source:crate::audio::readahead::ReadAhead::source(blocking, lib.paths.open_media(blocking, &path).map_err(|e|e.to_string())?,interrupt.clone()).map_err(|e|e.to_string())?,ext:Some(ext),origin,radio_metadata:None,
                }),
                None => return Err(e.to_string()),
            }
        };
        let reader = if v == "raw" {
            let client = lib.client().map_err(|e| e.to_string())?;
            let request = client.stream_request(&song.id, &stream_options(&v));
            reader.with_async_ranges(Arc::new(move |offset, length| {
                let response=client.read_range_async(&request, offset, length);
                Box::pin(async move {response.await
                    .map_err(|_| std::io::Error::other("Unable to read the requested song range"))})
            }))
        } else { reader };
        if complete { progress.wait_complete(blocking, interrupt).map_err(|e| if crate::audio::source::is_cut(&e) { "interrupted".into() } else { e.to_string() })?; }
        if song.spoken.is_none() {
            let reader=crate::audio::readahead::ReadAhead::progressive(reader,progress,interrupt.clone(),read_cancel).map_err(|e|format!("Unable to buffer playback: {e}"))?;
            // Keep the download hold on the front end: dropping/Next retires
            // ownership immediately, even while the worker finishes card I/O.
            return Ok(OpenedTrack {source:Box::new(HeldSource {inner:reader,_hold:hold}),ext:Some(extension(song,&v)),origin:Origin::Streaming,radio_metadata:None});
        }
        Ok(OpenedTrack { source: Box::new(HeldSource { inner: reader, _hold: hold }), ext: Some(extension(song, &v)), origin: Origin::Streaming, radio_metadata: None })
    }
}

#[cfg(test)]
mod spoken_priority_tests {
    use super::*;
    #[test]
    fn foreground_book_priority_releases_on_completion_or_cancel_and_counts_overlap() {
        let (lib,_)=Library::create(crate::config::Paths::new(super::super::generated_test_directory("book-priority")),&crate::config::Config::default(),Arc::new(|_|{}));
        assert!(!lib.playback_transfer_running());
        let first=lib.begin_spoken_playback();let second=lib.begin_spoken_playback();
        assert!(lib.playback_transfer_running());drop(first);assert!(lib.playback_transfer_running());
        drop(second);assert!(!lib.playback_transfer_running());
    }
}
