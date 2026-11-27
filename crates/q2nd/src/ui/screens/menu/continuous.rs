//! Music lists use the same bounded history window as books. Sources decide
//! whether evicted pages can be revisited and retained pages can be refreshed.
use super::*;
use crate::ui::paging::{Applied, Window};

const RETRY_KEY: &str = "__q2_music_page_retry";
#[derive(Default)]
pub(super) struct Pages {
    window: Window,
    error: Option<String>,
    account: Option<u32>,
    refresh_queued: bool,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    jump_cancel: Option<tokio::sync::oneshot::Sender<()>>,
    jump_sequence: u64,
    jump_delta: i32,
}
impl Pages { pub(super) fn jump_pending(&self) -> bool { self.jump_cancel.is_some() } }
pub(super) struct JumpLoaded { sequence: u64, account: u32, result: Result<Option<(u32, String, WindowPage)>, LibError> }
pub(super) struct WindowLoaded {
    token: u64,
    account: u32,
    rows: Result<WindowPage, LibError>,
    final_reply: bool,
}
impl Menu {
    pub(super) fn reset_window(&mut self) {
        if let Some(pages) = self.pages.as_mut() {
            pages.window.reset(); pages.error = None; pages.refresh_queued = false;
            pages.cancel = None; pages.jump_cancel = None; pages.jump_sequence = pages.jump_sequence.wrapping_add(1); pages.jump_delta = 0;
        }
    }
    fn bind_window_account(&mut self, account: u32) {
        let pages = self.pages.as_mut().unwrap();
        if pages.account != Some(account) {
            pages.window.reset(); pages.error = None; pages.refresh_queued = false;
            pages.cancel = None; pages.jump_cancel = None; pages.jump_sequence = pages.jump_sequence.wrapping_add(1); pages.jump_delta = 0;
            pages.account = Some(account);
            self.list.set_global_letters(false);
            self.list.set_rows(Vec::new()); self.state = State::Loading;
            self.badge_pending = None; self.warmed = None;
        }
    }
    pub(super) fn reload_window(&mut self, ctx: &mut Ctx) {
        self.bind_window_account(ctx.lib.client_generation());
        let pages = self.pages.as_mut().unwrap();
        if !self.source.refresh_retained() && !pages.window.rows().is_empty() { return; }
        // Coalesce reconnect notifications behind the in-flight refresh instead
        // of opening concurrent page owners or discarding visible history.
        if pages.window.pending().is_some() { pages.refresh_queued = true; return; }
        let page = self.list.selected_row().and_then(|row| pages.window.page_for_key(&row.key))
            .or_else(|| self.list.rows.iter().find_map(|row| pages.window.page_for_key(&row.key))).unwrap_or(0);
        self.load_window(page, ctx);
    }
    fn rebuild_window(&mut self) {
        let pages = self.pages.as_ref().unwrap();
        let mut rows = pages.window.rows();
        if let Some(error) = pages.error.as_ref().filter(|_| pages.window.retry_visible()) {
            let waiting = pages.window.pending().is_some_and(|request| Some(request.page) == pages.window.failed());
            let retry = Row::new("Retry", RowAction::None).key(RETRY_KEY)
                .detail(if waiting { "Loading…" } else { error });
            if pages.window.failure_before_content() { rows.insert(0, retry); }
            else { rows.push(retry); }
        }
        self.list.set_rows_anchored(rows);
        self.state = if pages.window.pending().is_some() && self.list.rows.is_empty() { State::Loading } else { State::Ready };
        self.warmed = None; self.badges_stale = true;
    }
    fn load_window(&mut self, page: u32, ctx: &mut Ctx) {
        let pages = self.pages.as_mut().unwrap();
        let Some(request) = pages.window.begin(page) else { return; };
        let account = pages.account.unwrap();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        pages.cancel = Some(cancel);
        // UXI-019: a list the listener is waiting on with nothing shown does
        // not queue behind background page loads.
        let waiting = self.list.rows.is_empty();
        self.rebuild_window();
        let source = self.source.clone(); let lib = ctx.lib.clone();
        ctx.page_task(PAGE_TOKEN, waiting, move |reply| async move {
            let work = async move {
                if lib.client_generation() != account { return; }
                let cached = source.clone().window_page(lib.clone(), page, Policy::CacheOnly).await;
                if lib.offline_mode() {
                    reply.send(WindowLoaded { token: request.token, account, rows: cached, final_reply: true });
                    return;
                }
                if cached.is_ok() {
                    reply.send(WindowLoaded { token: request.token, account, rows: cached, final_reply: false });
                }
                let rows = source.window_page(lib, page, Policy::NetworkOnly).await;
                reply.send(WindowLoaded { token: request.token, account, rows, final_reply: true });
            };
            tokio::select! { biased; _ = cancelled => {}, _ = work => {} }
        });
    }
    pub(super) fn scroll_window(&mut self, direction: i32, ctx: &mut Ctx) {
        if self.pages.as_ref().unwrap().account != Some(ctx.lib.client_generation()) {
            self.reload_window(ctx); return;
        }
        self.cancel_jump();
        if direction < 0 && !self.source.refetch_history() { return; }
        if let Some(page) = self.pages.as_ref().unwrap().window.next(self.list.selected, direction) {
            self.load_window(page, ctx);
        }
    }
    pub(super) fn cancel_jump(&mut self) {
        let pages = self.pages.as_mut().unwrap();
        pages.jump_cancel = None; pages.jump_sequence = pages.jump_sequence.wrapping_add(1); pages.jump_delta = 0;
    }
    pub(super) fn jump_window(&mut self, delta: i32, ctx: &mut Ctx) {
        let pages = self.pages.as_mut().unwrap();
        if pages.account != Some(ctx.lib.client_generation()) { return; }
        let Some(position) = self.list.selected_row().and_then(|row| pages.window.position_for_key(&row.key, 100)) else { return; };
        pages.jump_delta = pages.jump_delta.saturating_add(delta);
        let delta = pages.jump_delta;
        if delta == 0 {
            self.cancel_jump();
            return;
        }
        pages.jump_sequence = pages.jump_sequence.wrapping_add(1);
        let sequence = pages.jump_sequence; let account = pages.account.unwrap();
        let (cancel, cancelled) = tokio::sync::oneshot::channel(); pages.jump_cancel = Some(cancel);
        let source = self.source.clone(); let lib = ctx.lib.clone();
        ctx.task(JUMP_TOKEN, move |reply| async move {
            let work = async move {
                let result = async {
                    let Some((target, label)) = source.clone().alphabet_jump(lib.clone(), position, delta).await? else { return Ok(None); };
                    let policy = if lib.offline_mode() { Policy::CacheOnly } else { Policy::NetworkOnly };
                    let page = source.window_page(lib, target / 100, policy).await?;
                    Ok(Some((target, label, page)))
                }.await;
                reply.send(JumpLoaded { sequence, account, result });
            };
            tokio::select! { biased; _ = cancelled => {}, _ = work => {} }
        });
    }
    pub(super) fn apply_jump(&mut self, loaded: JumpLoaded, ctx: &mut Ctx) {
        let pages = self.pages.as_mut().unwrap();
        if pages.account != Some(loaded.account) || loaded.account != ctx.lib.client_generation() || pages.jump_sequence != loaded.sequence { return; }
        pages.jump_cancel = None; pages.jump_delta = 0;
        match loaded.result {
            Ok(Some((target, label, WindowPage { rows, more, letters }))) => {
                if target as usize % 100 >= rows.len() { ctx.notice_task(async { "Unable to find that album group".into() }); return; }
                pages.cancel = None;
                let request = pages.window.begin_jump(target / 100);
                if pages.window.apply(request.token, rows, more, true, None) != Applied::Content { return; }
                pages.error = None;
                self.list.set_global_letters(letters);
                self.rebuild_window();
                self.list.select((target % 100) as usize);
                self.list.show_alphabet_jump(label);
            }
            Ok(None) => (),
            Err(error) => { let message = error.to_string(); ctx.notice_task(async move { message }); }
        }
    }
    /// A window whose only row is Retry has no content: the page shows the
    /// shared state instead (Loading while the retry waits), with Retry
    /// selected so Center still retries.
    pub(super) fn lone_retry(&self) -> Option<crate::ui::state::View<'_>> {
        let row = self.list.rows.first().filter(|row| self.list.rows.len() == 1 && row.key == RETRY_KEY)?;
        let waiting = self.pages.as_ref().is_some_and(|pages| pages.window.pending().is_some());
        Some(if waiting { crate::ui::state::View::Loading }
            else { crate::ui::state::View::Failed { text: row.detail.as_deref().unwrap_or(crate::ux::Code::Unknown.message()), retry: true } })
    }
    /// Retry of a window with no content abandons any request still waiting
    /// and starts again from the first page.
    pub(super) fn restart_window(&mut self, ctx: &mut Ctx) {
        let pages = self.pages.as_mut().unwrap();
        pages.window.reset(); pages.error = None; pages.refresh_queued = false; pages.cancel = None;
        self.list.set_rows(Vec::new());
        self.deadline.restart(ctx.now);
        self.load_window(0, ctx);
    }
    pub(super) fn retry_window(&mut self, key: &str, ctx: &mut Ctx) -> bool {
        if key != RETRY_KEY || self.pages.is_none() { return false; }
        if self.pages.as_ref().unwrap().account != Some(ctx.lib.client_generation()) {
            self.reload_window(ctx); return true;
        }
        let pages = self.pages.as_ref().unwrap();
        if pages.window.retry_visible() {
            if let Some(page) = pages.window.failed() { self.deadline.restart(ctx.now); self.load_window(page, ctx); }
        }
        true
    }
    pub(super) fn apply_window(&mut self, loaded: WindowLoaded, ctx: &mut Ctx) {
        if loaded.account != ctx.lib.client_generation() {
            if self.pages.as_ref().unwrap().account != Some(ctx.lib.client_generation()) { self.reload_window(ctx); }
            return;
        }
        if self.accept_window(loaded) { self.reload_window(ctx); }
    }
    /// Returns whether a coalesced refresh should now be dispatched. Pure
    /// application of a reply also permits long deterministic traversal tests.
    fn accept_window(&mut self, loaded: WindowLoaded) -> bool {
        let pages = self.pages.as_mut().unwrap();
        if pages.account != Some(loaded.account) { return false; }
        let anchor = self.list.selected_row().map(|row| row.key.clone());
        match loaded.rows {
            Ok(WindowPage { rows, more, letters }) => match pages.window.apply(loaded.token, rows, more, loaded.final_reply, anchor.as_deref()) {
                Applied::Stale => return false,
                Applied::NoProgress => pages.error = Some("The server repeated this list; try again".into()),
                Applied::Content => {
                    self.list.set_global_letters(letters);
                    if pages.window.failed().is_none() { pages.error = None; }
                    // The opening is over when rows are on screen, as for a
                    // list loaded whole (`Menu::apply`). Nothing is pending
                    // for a later page.
                    crate::ux::end(crate::ux::Journey::OpenPage, crate::ux::Outcome::Shown);
                }
            },
            Err(error) => {
                if !pages.window.fail(loaded.token) { return false; }
                // Plain words on screen, the fixed code in the report (UX-006).
                pages.error = Some(crate::ux::present_in(&error.to_string(), crate::ux::Journey::OpenPage));
            }
        }
        let refresh = loaded.final_reply && std::mem::take(&mut pages.refresh_queued);
        self.rebuild_window();
        refresh
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};
    struct Generated;
    impl Source for Generated {
        fn title(&self) -> String { "Generated continuous list".into() }
        fn windowed(&self) -> bool { true }
        fn paged(&self) -> bool { true }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask { ready(Ok(rows(0))) }
    }
    fn rows(page: u32) -> Vec<Row> {
        (page * 20..page * 20 + 20).map(|i| Row::new(format!("Generated {i}"), RowAction::None).key(format!("k{i}"))).collect()
    }
    fn begin(menu: &mut Menu, page: u32) -> u64 { menu.pages.as_mut().unwrap().window.begin(page).unwrap().token }
    fn reply(menu: &mut Menu, token: u64, page: u32, final_reply: bool) -> bool {
        menu.accept_window(WindowLoaded { token, account: 1, rows: Ok((rows(page), page < 299).into()), final_reply })
    }
    #[test]
    fn a_windowed_list_ends_its_open_page_journey_when_rows_are_shown() {
        // Without the end, an album list left open is reported as stuck
        // after 3 s and timed out after 30 s, and adds nothing to the
        // page-open latency metric.
        let mut menu = Menu::new(Generated); menu.bind_window_account(1);
        let token = begin(&mut menu, 0);
        let _ = crate::ux::take_samples();
        crate::ux::begin(crate::ux::Journey::OpenPage);
        reply(&mut menu, token, 0, true);
        let samples = crate::ux::take_samples();
        assert_eq!(samples.iter().map(|(journey, _)| *journey as u8).collect::<Vec<_>>(), [crate::ux::Journey::OpenPage as u8], "{} rows shown", menu.list.rows.len());
        // A later page of the same list is not another opening.
        let token = begin(&mut menu, 1); reply(&mut menu, token, 1, true);
        assert!(crate::ux::take_samples().is_empty());
    }
    #[test]
    fn music_menu_browses_beyond_twenty_pages_and_refetches_history_with_bounded_rows() {
        let mut menu = Menu::new(Generated); menu.bind_window_account(1);
        for page in 0..300 {
            let anchor = menu.list.selected_row().map(|row| row.key.clone());
            let token = begin(&mut menu, page); reply(&mut menu, token, page, true);
            assert!(menu.list.rows.len() <= 100);
            if let Some(anchor) = anchor { assert_eq!(menu.list.selected_row().unwrap().key, anchor); }
            menu.list.selected = menu.list.rows.len() - 1;
            assert_eq!(menu.pages.as_ref().unwrap().window.next(menu.list.selected, 1), (page < 299).then_some(page + 1));
        }
        for page in (0..295).rev() {
            menu.list.selected = 0;
            let anchor = menu.list.selected_row().unwrap().key.clone();
            assert_eq!(menu.pages.as_ref().unwrap().window.next(0, -1), Some(page));
            let token = begin(&mut menu, page); reply(&mut menu, token, page, true);
            assert_eq!(menu.list.selected_row().unwrap().key, anchor);
            assert!(menu.list.rows.len() <= 100);
        }
        assert_eq!(menu.list.rows.first().unwrap().key, "k0");
        assert_eq!(menu.pages.as_ref().unwrap().window.next(0, -1), None);
    }
    /// UX-006: a server failure says what happened in plain words beside
    /// Retry, and the fixed code goes to the report.
    #[test]
    fn page_failure_is_plain_and_reported() {
        crate::ux::reset_for_test();
        let mut menu = Menu::new(Generated); menu.bind_window_account(1);
        let token = begin(&mut menu, 0);
        menu.accept_window(WindowLoaded { token, account: 1, rows: Err(LibError::Server("HTTP 503: <html>Service Unavailable</html>".into())), final_reply: true });
        let retry = menu.list.rows.iter().find(|row| row.key == RETRY_KEY).expect("retry row");
        assert_eq!(retry.detail.as_deref(), Some("The server had a problem"));
        let sent = crate::ux::take_sent();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].code, crate::ux::Code::ServerError as u16);
        // With no content, the page shows the shared failed state with Retry;
        // while the retry waits, the shared loading state.
        assert_eq!(menu.lone_retry(), Some(crate::ui::state::View::Failed { text: "The server had a problem", retry: true }));
        begin(&mut menu, 0);
        menu.rebuild_window();
        assert_eq!(menu.lone_retry(), Some(crate::ui::state::View::Loading));
    }

    #[test]
    fn cached_refresh_retry_overlap_account_and_backward_failures_keep_context() {
        let mut menu = Menu::new(Generated); menu.bind_window_account(1);
        let first = begin(&mut menu, 0); assert!(!reply(&mut menu, first, 0, false));
        assert!(menu.pages.as_mut().unwrap().window.begin(1).is_none());
        menu.pages.as_mut().unwrap().refresh_queued = true;
        assert!(reply(&mut menu, first, 0, true));
        menu.list.selected = 19;
        let failed = begin(&mut menu, 1);
        menu.accept_window(WindowLoaded { token: failed, account: 1, rows: Err(LibError::Offline), final_reply: true });
        assert_eq!(menu.list.selected_row().unwrap().key, "k19");
        assert_eq!(menu.list.rows.last().unwrap().key, RETRY_KEY);
        assert_eq!(menu.pages.as_ref().unwrap().window.next(19, 1), None);
        let retry = begin(&mut menu, 1);
        assert!(!reply(&mut menu, failed, 1, true));
        let mut overlapping = rows(0); overlapping.extend(rows(1)); overlapping.extend(rows(1));
        menu.accept_window(WindowLoaded { token: retry, account: 1, rows: Ok((overlapping, true).into()), final_reply: true });
        assert_eq!(menu.list.rows.len(), 40, "overlap and duplicate rows are not repeated");
        assert_eq!(menu.list.selected_row().unwrap().key, "k19");
        let stale = begin(&mut menu, 2);
        menu.bind_window_account(2);
        assert!(!reply(&mut menu, stale, 2, true)); assert!(menu.list.rows.is_empty());
        menu.bind_window_account(1);
        for page in 4..9 { let token = begin(&mut menu, page); reply(&mut menu, token, page, true); menu.list.selected = menu.list.rows.len()-1; }
        menu.list.selected = 0;
        let token = begin(&mut menu, 3);
        menu.accept_window(WindowLoaded { token, account: 1, rows: Err(LibError::Offline), final_reply: true });
        assert_eq!(menu.list.rows[0].key, RETRY_KEY, "backward retry is beside the failed edge");
        assert_eq!(menu.list.selected_row().unwrap().key, "k80");
    }

    struct Indexed {
        called: tokio::sync::mpsc::UnboundedSender<()>,
        held: Mutex<std::collections::VecDeque<tokio::sync::oneshot::Receiver<Result<Option<(u32, String)>, LibError>>>>,
    }
    impl Source for Indexed {
        fn title(&self) -> String { "Generated indexed albums".into() }
        fn alphabetical(&self) -> bool { true }
        fn global_alphabet(&self) -> bool { true }
        fn paged(&self) -> bool { true }
        fn windowed(&self) -> bool { true }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask { ready(Ok(rows(0))) }
        fn window_page(self: Arc<Self>, _: Arc<Library>, page: u32, _: Policy) -> WindowPageTask {
            Box::pin(async move {
                let rows = (page*100..page*100+100).map(|i| Row::new(format!("Album {i}"), RowAction::None).key(format!("album-{i}"))).collect();
                Ok(WindowPage { rows, more: true, letters: true })
            })
        }
        fn alphabet_jump(self: Arc<Self>, _: Arc<Library>, _: u32, _: i32) -> JumpTask {
            let receiver = self.held.lock().unwrap().pop_front().unwrap();
            let _ = self.called.send(());
            Box::pin(async move { receiver.await.unwrap_or(Err(LibError::Offline)) })
        }
    }
    #[test]
    fn actual_app_indexed_jump_keeps_selection_on_failure_and_cancels_on_lift_or_back() {
        let mut app = crate::ui::settings_tests::fixture(); app.player.shutdown(); app.lib.set_offline_mode(true);
        let runtime = crate::rt::Ui::new().unwrap(); let (tx, mut inbox) = crate::ui::channel(); app.tx = tx;
        let (success, first) = tokio::sync::oneshot::channel();
        let (failure, second) = tokio::sync::oneshot::channel();
        let (mut lift, third) = tokio::sync::oneshot::channel();
        let (mut back, fourth) = tokio::sync::oneshot::channel();
        let (called, mut calls) = tokio::sync::mpsc::unbounded_channel();
        let source = Indexed { called, held: Mutex::new([first, second, third, fourth].into_iter().collect()) };
        let parent = app.top_id(); runtime.block_on(async { app.push(Menu::boxed(source)); });
        let screen = app.top_id(); let mut platform = crate::platform::Headless { last: None };
        let consume = |app: &mut crate::ui::App, inbox: &mut crate::ui::UiRx, platform: &mut crate::platform::Headless, token| {
            runtime.block_on(async { loop {
                let msg = tokio::time::timeout(LIVENESS, inbox.recv()).await.unwrap().unwrap();
                let ours = matches!(&msg, crate::ui::UiMsg::Loaded { screen: owner, token: found, .. } if *owner == screen && *found == token);
                app.handle_async(msg, platform).await; if ours { break; }
            }});
        };
        consume(&mut app, &mut inbox, &mut platform, PAGE_TOKEN);
        let spin = |app: &mut crate::ui::App, platform: &mut crate::platform::Headless, start: Instant| {
            runtime.block_on(async { for i in 0..=18 {
                app.input_time = Some(start + Duration::from_millis(i*45)); app.event(Event::Scroll(1), platform);
            }});
        };
        let before_frame = app.render().pixels.clone();
        let now = Instant::now(); spin(&mut app, &mut platform, now);
        runtime.block_on(async {
            app.input_time = Some(now + Duration::from_secs(1));
            app.event(Event::Scroll(1), &mut platform);
        });
        success.send(Ok(Some((19_050, "Z".into())))).unwrap();
        consume(&mut app, &mut inbox, &mut platform, JUMP_TOKEN);
        let surface = app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows()[surface.selected()].key, "album-19050");
        assert_ne!(app.render().pixels, before_frame, "the distant selection is rendered");
        runtime.block_on(async {
            app.input_async(crate::input::Raw::WheelEnd, now, &mut platform).await;
        });
        assert!(app.needs_draw(), "raw finger lift must present a frame without the letter");
        runtime.block_on(async {
            app.input_time = Some(now + Duration::from_secs(2)); app.event(Event::Scroll(1), &mut platform);
        });
        let surface = app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows()[surface.selected()].key, "album-19051");
        runtime.block_on(async {
            app.input_time = Some(now + Duration::from_secs(3)); app.event(Event::Scroll(-1), &mut platform);
        });
        let surface = app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows()[surface.selected()].key, "album-19050");
        runtime.block_on(async { app.event(Event::WheelEnd, &mut platform); });
        spin(&mut app, &mut platform, now + Duration::from_secs(5));
        let surface = app.stack.last().unwrap().1.audit();
        let before = surface.rows().iter().map(|row|row.title.clone()).collect::<Vec<_>>();
        let selection = surface.rows()[surface.selected()].key.clone();
        failure.send(Err(LibError::Offline)).unwrap(); consume(&mut app, &mut inbox, &mut platform, JUMP_TOKEN);
        let surface = app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows().iter().map(|row|row.title.clone()).collect::<Vec<_>>(), before);
        assert_eq!(surface.rows()[surface.selected()].key, selection);
        runtime.block_on(async { app.event(Event::WheelEnd, &mut platform); });
        spin(&mut app, &mut platform, now + Duration::from_secs(8));
        let surface = app.stack.last().unwrap().1.audit();
        let before_lift = surface.rows()[surface.selected()].key.clone();
        runtime.block_on(async {
            // Wait for the held lookup to own its response before releasing.
            for _ in 0..3 {
                tokio::time::timeout(LIVENESS, calls.recv()).await.unwrap().unwrap();
            }
            app.input_async(crate::input::Raw::WheelEnd, now + Duration::from_secs(9), &mut platform).await;
            tokio::time::timeout(LIVENESS, lift.closed()).await.unwrap();
        });
        let surface = app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows()[surface.selected()].key, before_lift);
        spin(&mut app, &mut platform, now + Duration::from_secs(10));
        runtime.block_on(async { app.event(Event::Back, &mut platform); });
        assert_eq!(app.top_id(), parent);
        runtime.block_on(async { tokio::time::timeout(LIVENESS, back.closed()).await.unwrap(); });
    }

    struct Signalled { called: std::sync::mpsc::Sender<u32> }
    impl Source for Signalled {
        fn title(&self) -> String { "Generated albums".into() }
        fn paged(&self) -> bool { true }
        fn windowed(&self) -> bool { true }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask { ready(Ok(rows(0))) }
        fn window_page(self: Arc<Self>, _: Arc<Library>, page: u32, _: Policy) -> WindowPageTask {
            Box::pin(async move { let _ = self.called.send(page); Ok((rows(page), true).into()) })
        }
    }
    /// Finding 26 (code review round 2): a windowed list's first page took
    /// a background page slot, so Albums showed Loading while Cover Flow's
    /// refresh or reconnect reloads held all three.
    #[test]
    fn an_empty_windowed_list_loads_its_first_page_while_every_background_slot_is_held() {
        let mut app = crate::ui::settings_tests::fixture(); app.player.shutdown(); app.lib.set_offline_mode(true);
        let runtime = crate::rt::Ui::new().unwrap(); let (tx, _inbox) = crate::ui::channel(); app.tx = tx;
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (holding, held) = std::sync::mpsc::channel();
        for _ in 0..crate::ui::PAGE_TASKS {
            let (release, holding) = (release.clone(), holding.clone());
            crate::ui::spawn_page(async move { holding.send(()).unwrap(); let _ = release.acquire().await; });
        }
        for _ in 0..crate::ui::PAGE_TASKS { held.recv_timeout(LIVENESS).unwrap(); }
        let (called, calls) = std::sync::mpsc::channel();
        runtime.block_on(async { app.push(Menu::boxed(Signalled { called })); });
        let page = calls.recv_timeout(LIVENESS);
        release.add_permits(crate::ui::PAGE_TASKS);
        assert_eq!(page, Ok(0), "the first page loads beside the held slots");
    }

    /// How long a test waits for an event that must come. Not a timing
    /// assertion: every order is fixed by a channel, and this only ends a
    /// run that would otherwise hang.
    const LIVENESS: Duration = Duration::from_secs(120);

    struct Delayed {
        calls: Arc<Mutex<Vec<u32>>>,
        retries: AtomicUsize,
        release: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
        /// Fired when the page-2 load has begun: the test's Back must find
        /// it in flight, whenever the I/O thread gets to poll it.
        entered: Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    }
    impl Source for Delayed {
        fn title(&self) -> String { "Generated asynchronous list".into() }
        fn paged(&self) -> bool { true }
        fn windowed(&self) -> bool { true }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask { ready(Ok(rows(0))) }
        fn window_page(self: Arc<Self>, _: Arc<Library>, page: u32, policy: Policy) -> WindowPageTask {
            Box::pin(async move {
                assert_eq!(policy, Policy::CacheOnly, "generated offline browsing uses its cached pages");
                self.calls.lock().unwrap().push(page);
                if page == 1 && self.retries.fetch_add(1, Ordering::SeqCst) == 0 { return Err(LibError::Offline); }
                if page == 2 {
                    let receiver = self.release.lock().unwrap().take().unwrap();
                    let _ = self.entered.lock().unwrap().take().unwrap().send(());
                    receiver.await.unwrap();
                }
                Ok((rows(page), true).into())
            })
        }
    }
    #[test]
    fn actual_menu_scroll_retry_and_back_own_one_pending_page() {
        let mut app = crate::ui::settings_tests::fixture(); app.player.shutdown(); app.lib.set_offline_mode(true);
        let runtime = crate::rt::Ui::new().unwrap();
        let (tx, mut inbox) = crate::ui::channel(); app.tx = tx;
        let calls = Arc::new(Mutex::new(Vec::new())); let (mut release, waiting) = tokio::sync::oneshot::channel();
        let (entered, page_two_began) = tokio::sync::oneshot::channel();
        let parent = app.top_id();
        runtime.block_on(async { app.push(Menu::boxed(Delayed { calls: calls.clone(), retries: AtomicUsize::new(0), release: Mutex::new(Some(waiting)), entered: Mutex::new(Some(entered)) })); });
        let screen = app.top_id(); let mut platform = crate::platform::Headless { last: None };
        let consume = |app: &mut crate::ui::App, inbox: &mut crate::ui::UiRx, platform: &mut crate::platform::Headless| {
            runtime.block_on(async {
                loop {
                    let message = tokio::time::timeout(LIVENESS, inbox.recv()).await.unwrap().unwrap();
                    let ours = matches!(&message, crate::ui::UiMsg::Loaded { screen: owner, token: PAGE_TOKEN, .. } if *owner == screen);
                    app.handle_async(message, platform).await;
                    if ours { break; }
                }
            });
        };
        consume(&mut app, &mut inbox, &mut platform);
        assert_eq!(app.stack.last().unwrap().1.audit().rows().len(), 20);
        runtime.block_on(async { app.event(Event::Scroll(20), &mut platform); });
        consume(&mut app, &mut inbox, &mut platform);
        assert_eq!(app.stack.last().unwrap().1.audit().rows().last().unwrap().title, "Retry");
        runtime.block_on(async { app.event(Event::Scroll(1), &mut platform); app.event(Event::Center, &mut platform); });
        consume(&mut app, &mut inbox, &mut platform);
        assert_eq!(app.stack.last().unwrap().1.audit().rows().len(), 40);
        runtime.block_on(async { app.event(Event::Scroll(100), &mut platform); });
        // The page-2 load runs on the I/O thread. Wait until it holds its
        // page slot and is parked: a Back that arrives before its first poll
        // cancels it unrecorded, which is a different (and legal) history.
        runtime.block_on(async { tokio::time::timeout(LIVENESS, page_two_began).await.unwrap().unwrap(); });
        runtime.block_on(async {
            for _ in 0..10 { app.event(Event::Scroll(1), &mut platform); }
            app.event(Event::Back, &mut platform);
        });
        assert_eq!(app.top_id(), parent);
        runtime.block_on(async { tokio::time::timeout(LIVENESS, release.closed()).await.unwrap(); });
        // A completion already queued before cancellation must also be ignored.
        runtime.block_on(app.handle_async(crate::ui::UiMsg::Loaded { screen, token:PAGE_TOKEN,
            data:Box::new(WindowLoaded { token:0, account:1, rows:Ok((rows(2),true).into()), final_reply:true }) }, &mut platform));
        assert_eq!(app.top_id(), parent, "late history rows cannot reopen a dismissed screen");
        assert_eq!(*calls.lock().unwrap(), [0, 1, 1, 2]);
    }

    struct Draws { calls: Arc<Mutex<Vec<u32>>> }
    impl Source for Draws {
        fn title(&self) -> String { "Generated random history".into() }
        fn paged(&self) -> bool { true }
        fn windowed(&self) -> bool { true }
        fn refetch_history(&self) -> bool { false }
        fn refresh_retained(&self) -> bool { false }
        fn load(self: Arc<Self>, _: Arc<Library>, _: Policy) -> RowsTask { ready(Ok(rows(0))) }
        fn window_page(self: Arc<Self>, _: Arc<Library>, page: u32, _: Policy) -> WindowPageTask {
            Box::pin(async move { self.calls.lock().unwrap().push(page); Ok((rows(page),true).into()) })
        }
    }
    #[test]
    fn actual_app_random_history_retains_five_pages_without_backward_or_reconnect_refetch() {
        let mut app = crate::ui::settings_tests::fixture(); app.player.shutdown(); app.lib.set_offline_mode(true);
        let runtime = crate::rt::Ui::new().unwrap(); let (tx, mut inbox) = crate::ui::channel(); app.tx=tx;
        let calls=Arc::new(Mutex::new(Vec::new()));
        runtime.block_on(async { app.push(Menu::boxed(Draws { calls:calls.clone() })); });
        let screen=app.top_id(); let mut platform=crate::platform::Headless {last:None};
        for page in 0..9 {
            if page>0 { runtime.block_on(async { app.event(Event::Scroll(1000), &mut platform); }); }
            runtime.block_on(async { loop {
                let message=tokio::time::timeout(LIVENESS, inbox.recv()).await.unwrap().unwrap();
                let ours=matches!(&message,crate::ui::UiMsg::Loaded {screen:owner,token:PAGE_TOKEN,..} if *owner==screen);
                app.handle_async(message,&mut platform).await;
                if ours {break;}
            }});
            assert_eq!(app.stack.last().unwrap().1.audit().rows().len(), ((page+1).min(5)*20) as usize);
        }
        let retained=app.stack.last().unwrap().1.audit().rows().iter().map(|r|r.title.clone()).collect::<Vec<_>>();
        assert_eq!(retained.first().unwrap(),"Generated 80"); assert_eq!(retained.last().unwrap(),"Generated 179");
        runtime.block_on(async {
            app.event(Event::WheelEnd,&mut platform);
            for _ in 0..5 { app.event(Event::Scroll(-1000),&mut platform); }
            app.handle_async(crate::ui::UiMsg::Lib(LibEvent::Online(true)),&mut platform).await;
        });
        assert_eq!(app.stack.last().unwrap().1.audit().rows().iter().map(|r|r.title.clone()).collect::<Vec<_>>(),retained);
        // Give any erroneously spawned request an executor turn before checking.
        runtime.block_on(async { tokio::time::sleep(Duration::from_millis(30)).await; });
        assert_eq!(*calls.lock().unwrap(),(0..9).collect::<Vec<_>>());
        assert!(!app.render().pixels.is_empty());
        runtime.block_on(async { app.event(Event::Back,&mut platform); });
        assert_ne!(app.top_id(),screen);
    }
}
