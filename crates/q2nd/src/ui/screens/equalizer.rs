//! Independent taste and headphone correction controls. The graphic EQ is
//! the same on every output; the headphone profile belongs to the output in
//! use: the one pinned for the jack, or the Bluetooth device's own (matched
//! automatically from the AutoEQ catalog, or chosen here), see `ui::outputs`.
use crate::audio::{autoeq, eq, player::Cmd};
use crate::bluetooth::EqChoice;
use crate::ui::outputs::EqState;
use crate::gfx::{Rect, Weight};
use crate::input::Event;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::{Action, Ctx, Draw, Screen};
use super::controls::{Choice, Confirm};
use super::text::{Kind, TextEntry};
use std::time::Instant;

fn apply(ctx: &mut Ctx) {
    ctx.player.send(Cmd::SetEq(ctx.outputs.eq(&ctx.config.playback.eq)));
    *ctx.config_changed=true;
}
fn on(b: bool) -> &'static str { if b { "On" } else { "Off" } }

pub struct Equalizer { list: ListView }
impl Equalizer { pub fn new() -> Self { Self {list:ListView::settings(false)} } }
impl Screen for Equalizer {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::list(&self.list) }
    fn title(&self) -> String { "Equalizer".into() }
    fn enter(&mut self, ctx: &mut Ctx) {
        let s=&ctx.config.playback.eq;
        let mut rows=vec![
            Row::new("Graphic EQ",RowAction::Cmd(0)).value(on(s.graphic_enabled)),
            Row::screen("Presets",5).value(s.preset_name()),
            Row::screen("Adjust Bands",1),
            Row::screen("Reset Bands…",2),
        ];
        match &ctx.outputs.bluetooth {
            None => {
                let valid=s.headphone.as_ref().is_some_and(eq::Profile::valid);
                rows.extend([
                    Row::new("Output",RowAction::None).value("Headphone Jack").separated().key("output"),
                    Row { chevron:!valid, ..Row::new("Headphone EQ",RowAction::Cmd(3)).value(on(s.headphone_enabled)) },
                    Row::screen("Choose Headphones",4),
                    Row { chevron:valid,
                        ..Row::new(s.headphone.as_ref().map_or("No headphones selected",|p|p.name.as_str()),
                            if valid {RowAction::Cmd(6)} else {RowAction::None}).value(if valid {"Pinned"} else {""}).key("headphone-profile") },
                ]);
            }
            Some(device) => {
                let ready=device.profile().is_some() || matches!(device.eq_state,EqState::Ready(_));
                let name=device.eq.as_ref().filter(|eq|!eq.name.is_empty()).map_or("No headphones selected",|eq|eq.name.as_str());
                let on_value=device.enabled() && !matches!(device.eq_state,EqState::NoMatch);
                rows.extend([
                    Row::new("Output",RowAction::None).value(device.name.as_str()).separated().key("output"),
                    Row { chevron:!ready && device.eq.as_ref().is_none_or(|eq|eq.path.is_empty()), ..Row::new("Headphone EQ",RowAction::Cmd(3)).value(on(on_value)) },
                    Row::screen("Choose Headphones",4),
                    Row { chevron:ready, ..Row::new(name,if ready {RowAction::Cmd(6)} else {RowAction::None}).value(device.eq_reason()).key("headphone-profile") },
                ]);
                // A choice of the listener's, or no match: matching can be asked for again.
                if device.eq.as_ref().is_some_and(|eq|eq.choice==EqChoice::Chosen) || !matches!(device.eq_state,EqState::Ready(_)|EqState::Matching|EqState::Loading) {
                    rows.push(Row::new("Match Automatically",RowAction::Cmd(7)).key("match"));
                }
            }
        }
        self.list.set_rows(rows);
    }
    fn bluetooth_changed(&mut self, ctx: &mut Ctx) { self.enter(ctx); }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => { self.list.wheel(t,ctx.now); }
            Event::WheelEnd => self.list.end_spin(),
            Event::Center => {
                let Some(RowAction::Cmd(cmd))=self.list.selected_row().map(|r|r.action.clone()) else { return Action::None; };
                let s=&mut ctx.config.playback.eq;
                match cmd {
                    0 => s.graphic_enabled=!s.graphic_enabled,
                    1 => return Action::Push(Box::new(Graphic::new())),
                    2 => return Action::Push(Box::new(Confirm::reset_bands(|ctx| {
                            ctx.config.playback.eq.bands=[0;10];
                            apply(ctx);
                            Action::Pop
                        },
                    ))),
                    3 if ctx.outputs.bluetooth.is_some() => {
                        let device=ctx.outputs.bluetooth.as_ref().unwrap();
                        let nothing=!matches!(device.eq_state,EqState::Ready(_)) && device.eq.as_ref().is_none_or(|eq|eq.path.is_empty());
                        if nothing { return Action::Push(Box::new(Headphones::new(""))); }
                        let enabled=!device.enabled();
                        if let Some((addr,eq))=ctx.outputs.set_enabled(enabled) { ctx.bluetooth.set_device_eq(addr,Some(eq)); }
                    }
                    3 => {
                        if s.headphone.as_ref().is_none_or(|p|!p.valid()) { return Action::Push(Box::new(Headphones::new(""))); }
                        s.headphone_enabled=!s.headphone_enabled;
                    }
                    4 => return Action::Push(Box::new(Headphones::new(""))),
                    5 => return Action::Push(Box::new(presets(s))),
                    6 => return Action::Push(Box::new(super::eq_curves::Curves::new())),
                    7 => {
                        // Forget the choice; the Bluetooth thread's update starts a new match.
                        if let Some(device)=ctx.outputs.bluetooth.as_mut() { device.eq_state=EqState::Matching; ctx.bluetooth.set_device_eq(device.addr,None); }
                        self.enter(ctx);
                        return Action::Stay;
                    }
                    _ => return Action::None,
                }
                apply(ctx); self.enter(ctx);
            }
            _ => {},
        }
        Action::None
    }
    fn draw(&mut self,d:&mut Draw,area:Rect) {
        self.list.draw(d, Rect::new(area.x, area.y, area.w, area.h - 30));
        d.fonts.draw_centered(d.c,"Automatic headroom is always on",area.x+area.w/2,
            area.bottom()-24,14,Weight::Regular,d.theme.muted);
    }
    fn animate(&mut self,now:Instant)->bool { self.list.animate(now) }
}

fn presets(settings: &eq::Settings) -> Choice {
    let current = eq::PRESETS.iter().position(|p| p.bands == settings.bands);
    Choice::new("EQ Presets", eq::PRESETS.iter().map(|p| p.name.into()).collect(), current, Box::new(|i, ctx| {
        if ctx.config.playback.eq.set_preset(i) { apply(ctx); }
        Action::Pop
    }))
}

struct Graphic { selected: usize, editing: bool, bands:[i8;10], enabled:bool, targets: [Rect; 10], confirm: Rect }
impl Graphic { fn new()->Self { Self {selected:0,editing:false,bands:[0;10],enabled:false,targets:[Rect::default();10],confirm:Rect::default()} } }
impl Screen for Graphic {
    fn clickable(&self, x: i32, y: i32) -> bool {
        super::hits(self.confirm, x, y) || self.targets.iter().any(|&target| super::hits(target, x, y))
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if super::hits(self.confirm, x, y) {
            return self.event(Event::Center, ctx);
        }
        let Some(index) = self.targets.iter().position(|&target| super::hits(target, x, y)) else {
            return Action::None;
        };
        self.selected = index;
        self.editing = true;
        let target = self.targets[index];
        let mid = target.y + target.h / 2;
        let level = ((mid - y) as f32 / 5.0).round() as i32;
        let delta = level.clamp(-12, 12) - i32::from(self.bands[index]);
        if delta == 0 { Action::Stay } else { self.event(Event::Scroll(delta), ctx) }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface { crate::ui::hierarchy::Surface::widget("Graphic", format!("band={};editing={};enabled={}", self.selected, self.editing, self.enabled)) }
    fn title(&self)->String { "Graphic EQ".into() }
    fn enter(&mut self,ctx:&mut Ctx) { self.bands=ctx.config.playback.eq.bands.map(|b|b.clamp(-12,12)); self.enabled=ctx.config.playback.eq.graphic_enabled; }
    fn event(&mut self,ev:Event,ctx:&mut Ctx)->Action {
        match ev {
            Event::Scroll(t) => {
                if self.editing {
                    self.bands[self.selected]=(self.bands[self.selected] as i32).saturating_add(t).clamp(-12,12) as i8;
                    ctx.config.playback.eq.bands=self.bands;
                    // Adjusting a band is an explicit intent to audition it.
                    ctx.config.playback.eq.graphic_enabled=true; self.enabled=true;
                    apply(ctx);
                } else { self.selected=(self.selected as i32).saturating_add(t).clamp(0,9) as usize; }
                Action::Stay
            }
            Event::Center => { self.editing=!self.editing; Action::Stay }
            Event::Back if self.editing => { self.editing=false; Action::Stay }
            _ => Action::None,
        }
    }
    fn draw(&mut self,d:&mut Draw,area:Rect) {
        let cx=area.x+area.w/2;
        let top=area.y+67;
        let bottom=top+120;
        let mid=(top+bottom)/2;
        d.fonts.draw_centered(d.c,&format!("{} Hz   {:+} dB",eq::FREQUENCIES[self.selected].round() as u32,self.bands[self.selected]),cx,area.y+18,22,Weight::Bold,d.theme.text);
        d.c.fill_rect(Rect::new(area.x+35,mid,area.w-70,1),d.theme.faint);
        for i in 0..10 {
            let x=area.x+48+i as i32*31;
            self.targets[i] = Rect::new(x - 11, top - 4, 24, bottom - top + 9);
            let color=if i==self.selected { d.theme.sel_bottom } else { d.theme.muted };
            d.c.fill_rect(Rect::new(x,top,2,bottom-top),d.theme.track);
            let y=mid-self.bands[i] as i32*5;
            d.c.fill_round_rect(Rect::new(x-7,y-4,16,9),3,color);
            if i==self.selected && self.editing { d.c.fill_rect(Rect::new(x-10,bottom+25,22,3),color); }
            d.fonts.draw_centered(d.c,eq::LABELS[i],x+1,bottom+9,12,Weight::Regular,color);
        }
        let help=if self.editing { "Wheel: level · Center: done" } else { "Wheel: band · Center: adjust" };
        d.fonts.draw_centered(d.c,help,cx,area.y+230,16,Weight::Regular,d.theme.muted);
        self.confirm = super::center_target(d, help, cx, area.y + 230);
        d.fonts.draw_centered(d.c,if self.enabled { "±12 dB · Automatic headroom" } else { "Off · Adjust a band to enable" },cx,area.y+253,14,Weight::Regular,d.theme.muted);
    }
}

struct CatalogRows(Result<Vec<autoeq::Entry>, String>);
struct Headphones { list:ListView, entries:Vec<autoeq::Entry>, query:String, loading:bool,
    catalog_loading:bool, catalog_loaded:bool, catalog_failed:bool, token:u64,
    /// The catalog path of the profile being loaded.
    path:String }
impl Headphones {
    fn new(query:&str)->Self {
        let mut list=ListView::new(false);list.alphabetical=true;
        Self {list,entries:Vec::new(),query:query.into(),loading:false,
            catalog_loading:false,catalog_loaded:false,catalog_failed:false,token:0,path:String::new()}
    }
    fn rows(&mut self,ctx:&Ctx) {
        let mut rows=vec![Row::screen("Search Headphones…",u32::MAX).key("search")];
        if self.catalog_loading { rows.push(Row::new("Loading headphones…",RowAction::None)); }
        else if self.catalog_failed {
            rows.push(Row::new("Headphone catalog unavailable",RowAction::None));
            rows.push(Row::new("Retry",RowAction::Cmd(u32::MAX-1)).key("retry"));
        } else if self.entries.is_empty() { rows.push(Row::new("No matching headphones",RowAction::None)); }
        rows.extend(self.entries.iter().enumerate().map(|(i,e)| {
            let row=Row::new(e.name(),RowAction::Cmd(i as u32)).key(e.path());
            // The profile of the output in use: the jack's pin, or the Bluetooth device's choice.
            let current=match &ctx.outputs.bluetooth {
                Some(device) => device.eq.as_ref().is_some_and(|eq|eq.path==e.path()),
                None => ctx.config.playback.eq.headphone.as_ref().is_some_and(|p|p.source==e.url()),
            };
            if current { row.badge(crate::ui::list::Badge::Check) } else {row}
        }));
        self.list.set_rows(rows);
    }
}
impl Screen for Headphones {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    fn script_ready(&self) -> bool { !self.loading && !self.catalog_loading }
    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        crate::ui::hierarchy::Surface::list(&self.list)
            .with_state(self.list.letter_label().unwrap_or_default().into())
    }
    #[cfg(test)]
    fn audit_settle(&mut self,ctx:&mut Ctx) {
        if self.catalog_loading {
            // Map only: feed the real completion handler a deterministic catalog.
            // Delayed/error/Back behavior is exercised by runtime controls.
            self.loaded(self.token,Box::new(CatalogRows(Ok(autoeq::search(&self.query)))),ctx);
        }
    }
    fn title(&self)->String { if self.loading { "Downloading…".into() } else { "Headphone EQ".into() } }
    fn enter(&mut self,ctx:&mut Ctx) {
        if !self.catalog_loaded && !self.catalog_loading {
            self.catalog_loading=true;self.catalog_failed=false;self.token+=1;
            let query=self.query.clone();
            ctx.task(self.token,move|reply|async move {reply.send(CatalogRows(autoeq::search_async(query).await));});
        }
        self.rows(ctx);
    }
    fn event(&mut self,ev:Event,ctx:&mut Ctx)->Action {
        match ev {
            Event::Scroll(t) => {self.list.wheel(t,ctx.now);}
            Event::WheelEnd => {
                if self.list.finish_letters() { return Action::Stay; }
            }
            Event::Center if !self.loading=> {
                if self.list.finish_letters() { return Action::Stay; }
                let Some(RowAction::Cmd(cmd))=self.list.selected_row().map(|r|r.action.clone()) else {return Action::None;};
                if cmd==u32::MAX {
                    return Action::Push(Box::new(TextEntry::new("Find Headphones",&self.query,Kind::Search,false,Box::new(|query,_|
                        Action::Many(vec![Action::Pop, Action::Pop, Action::Push(Box::new(Headphones::new(&query)))])))));
                }
                if cmd==u32::MAX-1 { self.catalog_loaded=false;self.enter(ctx);return Action::Stay; }
                let Some(entry)=self.entries.get(cmd as usize).cloned() else {return Action::None;};
                self.loading=true;self.token+=1;self.path=entry.path().into();
                let root=ctx.paths.root.clone();
                let offline=ctx.lib.offline_mode();
                // Public AutoEQ data connects directly, not through Tailscale.
                ctx.task(self.token,move|reply| async move { reply.send(entry.load_async(root,offline,None).await); });
                return Action::Stay;
            }
            _=>{},
        }
        Action::None
    }
    fn loaded(&mut self,token:u64,data:Box<dyn std::any::Any+Send>,ctx:&mut Ctx) {
        if token!=self.token {return;}
        if data.is::<CatalogRows>() {
            let Ok(result)=data.downcast::<CatalogRows>() else {return;};
            let result=result.0;
            self.catalog_loading=false;self.catalog_loaded=true;
            match result {
                Ok(entries)=> {self.entries=entries;self.catalog_failed=false;}
                Err(error)=> {self.entries.clear();self.catalog_failed=true;ctx.requests.push(crate::ui::Request::Toast(error));}
            }
            self.rows(ctx);return;
        }
        let Ok(result)=data.downcast::<Result<eq::Profile,String>>() else {return;};
        self.loading=false;
        match *result {
            Ok(profile)=> {
                if ctx.outputs.bluetooth.is_some() {
                    // Remembered for this Bluetooth device; the jack's pin stays.
                    if let Some((addr,eq))=ctx.outputs.choose(&self.path,profile) { ctx.bluetooth.set_device_eq(addr,Some(eq)); }
                } else {
                    ctx.config.playback.eq.headphone=Some(profile);
                    ctx.config.playback.eq.headphone_enabled=true;
                }
                apply(ctx);self.rows(ctx);
                ctx.requests.push(crate::ui::Request::Toast("Headphone EQ applied".into()));
            }
            Err(e)=>ctx.requests.push(crate::ui::Request::Toast(e)),
        }
    }
    fn draw(&mut self,d:&mut Draw,area:Rect) {self.list.draw(d,area);}
    fn animate(&mut self,now:Instant)->bool {self.list.animate(now)}
}

#[cfg(test)]
mod resource_tests {
    use super::*;
    use crate::input::{Key, Raw};
    use crate::ui::UiMsg;
    use std::time::Duration;

    #[test]
    fn populated_autoeq_picker_keeps_short_strokes_precise_and_letters_deliberate() {
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown();
        app.background = false;
        let mut platform = crate::platform::Headless { last: None };
        app.push(Box::new(Equalizer::new()));
        let parent = app.top_id();
        app.event(Event::Scroll(5), &mut platform);
        app.event(Event::Center, &mut platform);
        let picker = app.top_id();
        assert_ne!(picker, parent);
        app.handle(UiMsg::Loaded { screen: picker, token: 1,
            data: Box::new(CatalogRows(Ok(autoeq::search("")))) }, &mut platform);

        let check = |app: &crate::ui::App, model: &str, letter: &str| {
            assert_eq!(app.top_id(), picker);
            let screen = &app.stack.last().unwrap().1;
            assert_eq!(screen.title(), "Headphone EQ");
            let state = screen.audit();
            assert_eq!(state.rows()[state.selected()].title, model);
            assert_eq!(state.state(), letter);
        };
        let catalog = app.stack.last().unwrap().1.audit();
        let first_a = catalog.rows().iter().position(|row| row.title == "A-Audio Elite (ANC on)").unwrap();
        app.event(Event::Scroll(first_a as i32), &mut platform);
        app.event(Event::WheelEnd, &mut platform);
        check(&app, "A-Audio Elite (ANC on)", "");

        let mut now = Instant::now();
        let motion = |app: &mut crate::ui::App, platform: &mut crate::platform::Headless,
                      now: &mut Instant, ticks, step_ms, frames| {
            for _ in 0..frames {
                *now += Duration::from_millis(step_ms);
                app.input(Raw::WheelMotion { ticks, at: *now }, *now, platform);
            }
        };
        // Nine detents in 256 ms used to trigger an alphabet jump.
        motion(&mut app, &mut platform, &mut now, 1, 32, 9);
        check(&app, "AAW A3H V2", "");
        app.input(Raw::WheelEnd, now, &mut platform);

        // The first frame establishes the timestamp. Fifteen measured detents
        // at 50 ms intervals are 1.5 turns at two revolutions per second.
        motion(&mut app, &mut platform, &mut now, 1, 50, 15);
        check(&app, "AAW x Custom Art Project 4+2 (bass filter)", "");
        motion(&mut app, &mut platform, &mut now, 1, 50, 1);
        check(&app, "AAW x Custom Art Project 4+2 (bass filter)", "A");

        now += Duration::from_secs(3);
        app.stack.last_mut().unwrap().1.animate(now);
        check(&app, "AAW x Custom Art Project 4+2 (bass filter)", "A");
        motion(&mut app, &mut platform, &mut now, 1, 200, 2);
        check(&app, "AAW x Custom Art Project 4+2 (bass filter)", "A");
        motion(&mut app, &mut platform, &mut now, 1, 200, 1);
        check(&app, "BASN Bmaster", "B");
        motion(&mut app, &mut platform, &mut now, -1, 200, 3);
        check(&app, "A-Audio Elite (ANC on)", "A");
        motion(&mut app, &mut platform, &mut now, 1, 200, 3);
        check(&app, "BASN Bmaster", "B");

        app.input(Raw::WheelEnd, now, &mut platform);
        check(&app, "BASN Bmaster", "");
        motion(&mut app, &mut platform, &mut now, 1, 200, 1);
        check(&app, "BASN Bmaster5", "");

        app.input(Raw::WheelEnd, now, &mut platform);
        motion(&mut app, &mut platform, &mut now, 1, 50, 16);
        let press = |app: &mut crate::ui::App, platform: &mut crate::platform::Headless,
                     now: &mut Instant, key| {
            app.input(Raw::Key { key, down: true }, *now, platform);
            *now += Duration::from_millis(40);
            app.input(Raw::Key { key, down: false }, *now, platform);
        };
        check(&app, "BGVP DMS", "B");
        press(&mut app, &mut platform, &mut now, Key::Center);
        check(&app, "BGVP DMS", "");
        press(&mut app, &mut platform, &mut now, Key::Center);
        assert_eq!(app.stack.last().unwrap().1.title(), "Downloading…");
        press(&mut app, &mut platform, &mut now, Key::Back);
        assert_eq!(app.top_id(), parent);
        let state = app.stack.last().unwrap().1.audit();
        assert_eq!(state.rows()[state.selected()].title, "Choose Headphones");
    }

    #[test]
    fn power_handoff_ends_a_held_autoeq_gesture_before_input_closes() {
        let mut app = crate::ui::settings_tests::fixture();
        let mut platform = crate::platform::Headless { last: None };
        app.push(Box::new(Headphones::new("")));
        let picker = app.top_id();
        app.handle(UiMsg::Loaded { screen: picker, token: 1,
            data: Box::new(CatalogRows(Ok(autoeq::search("")))) }, &mut platform);
        let start = Instant::now();
        for tick in 0..16 {
            let at = start + Duration::from_millis(tick * 45);
            app.input(Raw::WheelMotion { ticks: 1, at }, at, &mut platform);
        }
        assert!(!app.stack.last().unwrap().1.audit().state().is_empty());
        app.prepare_power_saved(false);
        assert!(app.power.pending());
        assert!(app.stack.last().unwrap().1.audit().state().is_empty());
    }

    #[test]
    fn on_bluetooth_the_page_shows_the_devices_profile_and_why_and_a_choice_stays_off_the_jack() {
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown();
        app.background = false;
        let mut platform = crate::platform::Headless { last: None };
        let output = crate::bluetooth::Output { addr: [9; 6], name: "Generated Headphones".into(), class: 0x24_0418,
            saved: Default::default(), volume_revision: 0, absolute: true };
        app.outputs.sync(Some(&output), 50);
        let token = app.outputs.plan().unwrap().0;
        let matched = crate::bluetooth::DeviceEq { choice: EqChoice::Automatic, enabled: true, path: "generated/over-ear/Generated%20Model".into(), name: "Generated Model".into() };
        let profile = eq::Profile { name: "Generated Model".into(), source: "generated".into(), preamp: -1.0,
            filters: vec![eq::Filter { kind: eq::Kind::Peak, hz: 100.0, gain: 1.0, q: 1.0 }] };
        app.outputs.resolved(token, [9; 6], Some(matched), Ok(Some(profile.clone())));
        app.push(Box::new(Equalizer::new()));
        let rows = |app: &crate::ui::App| app.stack.last().unwrap().1.audit().rows().iter().map(|r| (r.title.clone(), r.value.clone().unwrap_or_default())).collect::<Vec<(String, String)>>();
        let shown = rows(&app);
        assert!(shown.contains(&("Output".into(), "Generated Headphones".into())), "{shown:?}");
        assert!(shown.contains(&("Headphone EQ".into(), "On".into())));
        assert!(shown.contains(&("Generated Model".into(), "Matched automatically".into())));
        assert!(!shown.iter().any(|r| r.0 == "Match Automatically"), "a match in use needs no second match");
        // Choosing another profile: remembered for the device, not pinned to the jack.
        app.push(Box::new(Headphones::new("")));
        let screen = app.top_id();
        let chosen = eq::Profile { name: "Other Model".into(), ..profile };
        app.handle(crate::ui::UiMsg::Loaded { screen, token: 1, data: Box::new(Ok::<eq::Profile, String>(chosen)) }, &mut platform);
        assert!(app.config.playback.eq.headphone.is_none(), "the jack keeps its own pin");
        let device = app.outputs.bluetooth.as_ref().unwrap();
        assert_eq!((device.eq.as_ref().map(|e| e.choice), device.eq_reason()), (Some(EqChoice::Chosen), "Chosen"));
        app.event(Event::Back, &mut platform);
        let shown = rows(&app);
        assert!(shown.contains(&("Other Model".into(), "Chosen".into())) && shown.iter().any(|r| r.0 == "Match Automatically"), "{shown:?}");
    }

    #[test]
    fn catalog_loading_failure_retry_and_back_preserve_page_ownership() {
        let mut app = crate::ui::settings_tests::fixture();
        app.player.shutdown();
        // Deterministic delivery through actual App/Screen handlers. The
        // packaged parser has a separate real-file equivalence/refusal control.
        app.background = false;
        let mut platform = crate::platform::Headless { last: None };
        let parent = app.top_id();
        app.push(Box::new(Headphones::new("custom studio")));
        let screen = app.top_id();
        assert!(!app.stack.last().unwrap().1.script_ready());
        app.handle(UiMsg::Loaded { screen, token: 1,
            data: Box::new(CatalogRows(Err("Generated missing catalog".into()))) }, &mut platform);
        assert!(app.stack.last().unwrap().1.script_ready());
        assert!(app.stack.last().unwrap().1.audit().rows().iter().any(|r| r.title == "Retry"));
        app.event(Event::Scroll(i32::MAX), &mut platform);
        app.event(Event::WheelEnd, &mut platform);
        app.event(Event::Center, &mut platform);
        assert!(!app.stack.last().unwrap().1.script_ready());
        app.handle(UiMsg::Loaded { screen, token: 1,
            data: Box::new(CatalogRows(Ok(autoeq::search("custom studio")))) }, &mut platform);
        assert!(!app.stack.last().unwrap().1.script_ready(), "old result cannot finish Retry");
        app.event(Event::Back, &mut platform);
        app.handle(UiMsg::Loaded { screen, token: 2,
            data: Box::new(CatalogRows(Ok(autoeq::search("custom studio")))) }, &mut platform);
        assert_eq!(app.top_id(), parent, "late catalog must not reopen a cancelled chooser");
        app.push(Box::new(Headphones::new("custom studio")));
        let screen = app.top_id();
        app.handle(UiMsg::Loaded { screen, token: 1,
            data: Box::new(CatalogRows(Ok(autoeq::search("custom studio")))) }, &mut platform);
        assert!(app.stack.last().unwrap().1.script_ready());
        let surface = app.stack.last().unwrap().1.audit();
        assert_eq!(surface.rows().len(), 5);
        assert_eq!(surface.rows()[1].title, "Beyerdynamic Custom Studio (step 1)");
    }
}
