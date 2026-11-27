//! Bounded readable metadata. Preview mode starts playback only on an explicit tap.
use crate::gfx::Rect;
use crate::ui::info_text::InfoText;
use crate::input::Event;
use crate::library::book_info::{BookInfo, BookInfoTarget};
use crate::library::{LibError, LibEvent, Policy};
use crate::ui::{Action, Ctx, Draw, Screen};
use std::sync::Arc;

pub(crate) struct Loaded {
    pub(crate) result: Result<BookInfo, LibError>,
    pub(crate) configuration: Option<Arc<()>>,
    /// When `result` failed: what the background sync's lists hold (UXI-016).
    pub(crate) synced: Option<BookInfo>,
}
pub struct BookInfoScreen {
    playback: Option<super::spoken::Selection>,
    target: BookInfoTarget,
    info: Option<BookInfo>,
    text: InfoText,
    center_target: Option<Rect>,
    back_target: Option<Rect>,
    error: Option<String>,
    loading: bool,
    invalidated: bool,
    token: u64,
    /// The deadline of the load the listener waits on (UX-006).
    deadline: crate::ui::state::Deadline,
}
impl BookInfoScreen {
    pub fn new(target: BookInfoTarget) -> Self {
        Self {
            playback:None,
            target,
            info: None,
            text: InfoText::default(),
            center_target: None,
            back_target: None,
            error: None,
            loading: false,
            invalidated: false,
            token: 0,
            deadline: crate::ui::state::Deadline::new(std::time::Instant::now()),
        }
    }
    pub(crate) fn preview(target:BookInfoTarget,selection:super::spoken::Selection)->Self {
        let mut screen=Self::new(target);
        screen.playback=Some(selection);
        screen
    }
    fn load(&mut self, ctx: &mut Ctx, policy: Policy) {
        let target = self.target.clone();
        let lib = ctx.lib.clone();
        let configuration = target.remote().then(|| lib.shelf_generation());
        self.token = self.token.wrapping_add(1);
        self.loading = true;
        self.error = None;
        self.deadline.restart(ctx.now);
        // The listener waits on this page: never behind background loads.
        ctx.foreground_task(self.token, move |reply| async move {
            let result = lib.book_info_async(target.clone(), policy).await;
            // Never opened online: show what the sync stored beside the failure.
            let synced = if result.is_err() { lib.synced_book_info_async(&target).await } else { None };
            reply.send(Loaded { result, configuration, synced })
        });
    }
    fn invalidate(&mut self) {
        self.invalidated = true;
        self.loading = false;
        self.info = None;
        self.text.clear();
        self.error = Some("Audiobookshelf changed. Return and select the book again.".into());
    }
}
/// The background refresh's reply token; ordinary loads count up from 1.
const REFRESH: u64 = u64::MAX;

impl Screen for BookInfoScreen {
    fn clickable(&self, x: i32, y: i32) -> bool {
        self.center_target.is_some_and(|target| super::hits(target, x, y))
            || self.back_target.is_some_and(|target| super::hits(target, x, y))
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.center_target.is_some_and(|target| super::hits(target, x, y)) {
            self.event(Event::Center, ctx)
        } else if self.back_target.is_some_and(|target| super::hits(target, x, y)) {
            Action::Pop
        } else {
            Action::None
        }
    }

    fn script_ready(&self) -> bool { !self.loading }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        let (scroll, lines) = self.text.audit();
        crate::ui::hierarchy::Surface::widget("BookInfoScreen", format!("target={};loading={};error={};scroll={};play={};rendered-lines={:?}",
            crate::ui::hierarchy::info_target(&self.target), self.loading, self.error.is_some(), scroll, self.playback.is_some(), lines)).contextual()
    }
    #[cfg(test)]
    fn audit_settle(&mut self, ctx: &mut Ctx) {
        if !self.loading || self.invalidated {
            return;
        }
        let result = ctx.lib.book_info(&self.target, Policy::CacheOnly);
        let configuration = self.target.remote().then(|| ctx.lib.shelf_generation());
        self.loaded(
            self.token,
            Box::new(Loaded {
                result,
                configuration,
                synced: None,
            }),
            ctx,
        );
    }
    fn title(&self) -> String {
        if matches!(self.target,BookInfoTarget::Shelf {episode:Some(_),..}) {"Episode Info"} else {"Book Info"}.into()
    }
    fn enter(&mut self, ctx: &mut Ctx) {
        if self.token == 0 {
            crate::ux::begin(crate::ux::Journey::OpenInfo);
            self.load(ctx, Policy::PreferCache);
            // A stored copy shows at once; the server's copy replaces it if it differs.
            if self.target.remote() && ctx.lib.is_online_for_shelf() {
                let (lib, target) = (ctx.lib.clone(), self.target.clone());
                let configuration = target.remote().then(|| lib.shelf_generation());
                ctx.task(REFRESH, move |reply| async move {
                    reply.send(Loaded { result: lib.book_info_async(target, Policy::NetworkOnly).await, configuration, synced: None })
                });
            }
        }
    }
    fn event(&mut self, event: Event, ctx: &mut Ctx) -> Action {
        match event {
            Event::Scroll(ticks) => {
                self.text.scroll_by(ticks);
                Action::Stay
            }
            Event::Center if !self.loading && !self.invalidated => {
                if self.error.is_none() && self.info.is_some() {
                    if let Some(selection)=self.playback.clone() {
                        if let BookInfoTarget::Shelf {namespace,..}=&self.target {
                            if !ctx.lib.shelf_namespace_current(namespace) {self.invalidate();return Action::Stay;}
                        }
                        return match super::spoken::play_here(selection,self.info.as_ref().unwrap().title.clone(),ctx) {
                            Action::Push(opening) if opening.opening_spoken_in_place() => Action::Push(opening),
                            Action::Push(opening)=>Action::Replace(opening),
                            Action::NowPlaying=>Action::Many(vec![Action::Pop,Action::NowPlaying]),
                            action=>action,
                        };
                    }
                }
                self.load(
                    ctx,
                    if self.info.is_some() {
                        Policy::NetworkOnly
                    } else {
                        Policy::PreferCache
                    },
                );
                self.text.relayout();
                Action::Stay
            }
            _ => Action::None,
        }
    }
    fn refresh(&mut self, event: &LibEvent, _ctx: &mut Ctx) {
        if self.target.remote() && matches!(event, LibEvent::SpokenChanged) {
            self.invalidate();
        }
    }
    fn loaded(&mut self, token: u64, data: Box<dyn std::any::Any + Send>, ctx: &mut Ctx) {
        if token == REFRESH {
            // Only a newer, different copy changes a shown page; failures are silent.
            if self.invalidated || self.loading { return; }
            let Ok(data) = data.downcast::<Loaded>() else { return };
            if data.configuration.as_ref().is_some_and(|generation| !ctx.lib.shelf_generation_current(generation)) { return; }
            if let Ok(info) = data.result {
                if self.info.as_ref() != Some(&info) { self.info = Some(info); self.error = None; self.text.relayout(); }
            }
            return;
        }
        if token != self.token || self.invalidated {
            return;
        }
        crate::ux::end(crate::ux::Journey::OpenInfo, crate::ux::Outcome::Shown);
        let Ok(data) = data.downcast::<Loaded>() else {
            return;
        };
        if data
            .configuration
            .as_ref()
            .is_some_and(|generation| !ctx.lib.shelf_generation_current(generation))
        {
            self.invalidate();
            return;
        }
        self.loading = false;
        match data.result {
            Ok(info) => {
                self.info = Some(info);
                self.error = None;
            }
            Err(error) => {
                // Plain words on screen, the fixed code in the report (UX-006).
                self.error = Some(crate::ux::present_in(&error.to_string(), crate::ux::Journey::OpenInfo));
                self.text.reset_scroll(); // A failed refresh must be visible even deep in a summary.
                // Under the failure, what the sync stored (UXI-016).
                if self.info.is_none() { self.info = data.synced; }
            }
        }
        self.text.relayout();
    }
    fn tick(&mut self, ctx: &mut Ctx) -> bool {
        if self.loading && self.info.is_none() && self.deadline.expired(ctx.now, crate::ux::Journey::OpenInfo) {
            // A late answer still replaces this (its token is current).
            self.loading = false;
            self.error = Some(crate::ux::Code::Stuck.message().into());
            self.text.relayout();
            return true;
        }
        false
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let footer = if self.loading {
            "Loading… · Return: back"
        } else if self.invalidated {
            "Return: back"
        } else if self.error.is_some() {
            "Center: retry · Return: back"
        } else if let Some(selection) = &self.playback {
            // UXI-020: Center on the item already playing leaves it playing here.
            let current = d.status.current().is_some_and(|song| selection.matches(song, |namespace| d.lib.shelf_namespace_current(namespace)));
            match d.status.state {
                crate::audio::player::PlayState::Paused if current => "Wheel: read · Center: resume · Return: back",
                crate::audio::player::PlayState::Stopped => "Wheel: read · Center: play · Return: back",
                _ if current => "Wheel: read · Playing now · Return: back",
                _ => "Wheel: read · Center: play · Return: back",
            }
        } else {
            "Wheel: read · Center: refresh · Return: back"
        };
        self.center_target = footer.split(" · ").find(|label| label.starts_with("Center:"))
            .and_then(|label| InfoText::footer_target(d.fonts, area, footer, label));
        self.back_target = InfoText::footer_target(d.fonts, area, footer, "Return: back");
        let Self { text, error, info, loading, target, .. } = self;
        text.draw(d, area, footer, || {
            let mut content = Vec::new();
            if let Some(error) = error {
                content.extend([(error.clone(), false), (String::new(), false)]);
            }
            if let Some(info) = info {
                content.extend(paragraphs(info));
            } else if *loading {
                content.push((loading_message(target).into(), false));
            }
            content
        });
    }
}

fn loading_message(target: &BookInfoTarget) -> &'static str {
    if matches!(target, BookInfoTarget::Shelf { episode: Some(_), .. }) {
        "Loading episode information…"
    } else {
        "Loading book information…"
    }
}

fn time(milliseconds: u64) -> String {
    let seconds = milliseconds / 1000;
    if seconds >= 3600 {
        format!(
            "{}:{:02}:{:02}",
            seconds / 3600,
            seconds / 60 % 60,
            seconds % 60
        )
    } else {
        format!("{}:{:02}", seconds / 60, seconds % 60)
    }
}
fn paragraphs(info: &BookInfo) -> Vec<(String, bool)> {
    let mut result = vec![(info.title.clone(), true)];
    if !info.subtitle.is_empty() {
        result.push((info.subtitle.clone(), false));
    }
    for (label, values) in [
        ("Author", &info.authors),
        ("Narrator", &info.narrators),
        ("Series", &info.series),
    ] {
        if !values.is_empty() {
            result.push((format!("{label}: {}", values.join(", ")), false));
        }
    }
    if info.duration_ms > 0 {
        result.push((format!("Length: {}", time(info.duration_ms)), false));
    }
    for (label, value) in [
        ("Publisher", &info.publisher),
        ("Year", &info.published_year),
        ("Language", &info.language),
    ] {
        if !value.is_empty() {
            result.push((format!("{label}: {value}"), false));
        }
    }
    if !info.genres.is_empty() {
        result.push((format!("Genre: {}", info.genres.join(", ")), false));
    }
    if !info.description.is_empty() {
        result.extend([
            (String::new(), false),
            ("Summary".into(), true),
            (info.description.clone(), false),
        ]);
    }
    if !info.chapters.is_empty() {
        result.extend([
            (String::new(), false),
            (format!("Chapters ({})", info.chapters.len()), true),
        ]);
        result.extend(info.chapters.iter().map(|chapter| {
            (
                format!("{}  {}", time(chapter.start_ms), chapter.title),
                false,
            )
        }));
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_second_tap_reuses_paused_instance_without_replacing_queue() {
        use crate::audio::player::{Cmd,PlayState};
        let mut app=crate::ui::settings_tests::fixture();
        app.background=false;
        let song=subsonic::Song {id:"generated-part".into(),title:"Generated Book".into(),spoken:Some(Box::new(subsonic::Spoken {id:"generated-logical".into(),duration_ms:240000,source:subsonic::SpokenSource::Local {relative_path:"generated.m4b".into()},..Default::default()})),..Default::default()};
        app.player.send(Cmd::Restore {songs:std::sync::Arc::new(vec![song.clone()]),index:0,position_ms:12000});
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
        while app.player.status().state!=PlayState::Paused && std::time::Instant::now()<deadline {std::thread::sleep(std::time::Duration::from_millis(1));}
        let before=app.player.status();assert_eq!(before.state,PlayState::Paused);
        app.push(Box::new(BookInfoScreen::preview(BookInfoTarget::from_song(&song).unwrap(),super::super::spoken::Selection::Local(song))));
        let preview=app.top_id();
        app.handle(crate::ui::UiMsg::Loaded {screen:preview,token:1,data:Box::new(Loaded {result:Ok(BookInfo {title:"Generated Book".into(),..Default::default()}),configuration:None,synced:None})},&mut crate::platform::Headless {last:None});
        assert_eq!(app.player.status().state,PlayState::Paused,"metadata completion is read-only");
        assert_eq!(app.top_id(),preview);
        app.event(Event::Center,&mut crate::platform::Headless {last:None});
        // UXI-020: playing the current item from its info page stays on the page.
        assert_eq!(app.top_id(),preview);
        assert!(!app.stack.last().unwrap().1.is_now_playing());
        // An ordered ordinary command proves Resume has been accepted. A fake
        // path may fail audio opening here; core tests cover retained real PCM.
        app.player.send(Cmd::SetVolume(97));
        let deadline=std::time::Instant::now()+std::time::Duration::from_secs(2);
        while app.player.status().volume!=97 && std::time::Instant::now()<deadline {std::thread::sleep(std::time::Duration::from_millis(1));}
        let after=app.player.status();assert_eq!(after.volume,97);
        assert_eq!(after.generation,before.generation,"Resume never sends Play/Restore");
        assert!(Arc::ptr_eq(&before.queue,&after.queue));
        app.event(Event::Back,&mut crate::platform::Headless {last:None});
        assert_eq!(app.stack.len(),1);
        app.player.shutdown();
    }
    #[test]
    fn loading_copy_matches_episode_or_book_target() {
        for (episode, expected) in [(None, "Loading book information…"), (Some("episode".into()), "Loading episode information…")] {
            let target = BookInfoTarget::Shelf {namespace:"fixture".into(), library:None, item:"item".into(), episode};
            assert_eq!(loading_message(&target), expected);
        }
    }
    #[test]
    fn metadata_includes_summary_and_chapters() {
        let info = BookInfo {
            title: "Generated Book".into(),
            description: "One\n\nTwo".into(),
            narrators: vec!["Narrator".into()],
            duration_ms: 3661000,
            chapters: vec![subsonic::Chapter {
                start_ms: 0,
                title: "Beginning".into(),
            }],
            ..Default::default()
        };
        let content = paragraphs(&info)
            .into_iter()
            .map(|(text, _)| text)
            .collect::<Vec<_>>()
            .join("\n");
        for text in [
            "Generated Book",
            "Narrator: Narrator",
            "Length: 1:01:01",
            "Summary",
            "One\n\nTwo",
            "Chapters (1)",
            "0:00  Beginning",
        ] {
            assert!(content.contains(text), "{text}");
        }
    }
}
