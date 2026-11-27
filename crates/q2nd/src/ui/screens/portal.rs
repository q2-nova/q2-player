//! Accept guest Wi-Fi terms, sign in to Delta or prepare an account over a local phone link.

use super::text::{Edit, TextEntry};
use crate::gfx::{Rect, Weight, qr::Code};
use crate::input::Event;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::portal::State;
use crate::ui::state::View;
use crate::ui::{Action, Ctx, Draw, Screen};
use std::time::Instant;
use zeroize::Zeroizing;

const ACCOUNT: u32 = 3;
const USE_PREPARED: u32 = 4;
const START_PHONE: u32 = 5;
const CLEAR_PREPARED: u32 = 6;
const DONE: u32 = 7;
const PHONE_ENTRY: u32 = 8;
const AGREE: u32 = 9;
const USE_SAVED: u32 = 10;

enum Entry {
    Account(TextEntry),
    Password {
        user: Zeroizing<String>,
        field: TextEntry,
    },
}

impl Entry {
    fn field(&self) -> &TextEntry {
        match self {
            Self::Account(field) | Self::Password { field, .. } => field,
        }
    }

    fn field_mut(&mut self) -> &mut TextEntry {
        match self {
            Self::Account(field) | Self::Password { field, .. } => field,
        }
    }
}

struct PairCodes {
    wifi: Code,
    page: Code,
    show_page: bool,
}

impl PairCodes {
    fn new(wifi: &str, page: &str) -> Option<Self> {
        Some(Self {
            wifi: Code::for_url(wifi)?,
            page: Code::for_url(page)?,
            show_page: false,
        })
    }

    fn draw(&self, d: &mut Draw, area: Rect) {
        let (code, title, next) = if self.show_page {
            (
                &self.page,
                "Scan to open account setup",
                "Center shows the Wi-Fi code",
            )
        } else {
            (
                &self.wifi,
                "Scan to join the player's Wi-Fi",
                "Center shows the setup code",
            )
        };
        let center = area.x + area.w / 2;
        d.fonts.draw_centered(
            d.c,
            title,
            center,
            area.y + 10,
            16,
            Weight::Regular,
            d.theme.text,
        );
        let max = (area.h - 104).min(area.w - 32);
        let side = code.side(max);
        code.draw(d.c, center - side / 2, area.y + 36 + (max - side) / 2, max);
        d.fonts.draw_centered(
            d.c,
            next,
            center,
            area.bottom() - 51,
            16,
            Weight::Bold,
            d.theme.text,
        );
        d.fonts.draw_centered(
            d.c,
            "Return cancels setup",
            center,
            area.bottom() - 26,
            14,
            Weight::Regular,
            d.theme.muted,
        );
    }
}

pub(super) struct PortalScreen {
    phone: bool,
    check_on_enter: bool,
    pairing: bool,
    state: State,
    list: ListView,
    codes: Option<PairCodes>,
    entry: Option<Entry>,
    wheel_login: bool,
    guest_attempted: bool,
}

impl PortalScreen {
    pub(super) fn wifi() -> Self {
        Self::new(false)
    }
    pub(super) fn phone() -> Self {
        Self::new(true)
    }

    fn new(phone: bool) -> Self {
        Self {
            phone,
            check_on_enter: !phone,
            pairing: false,
            state: State::Idle,
            list: ListView::settings(true),
            codes: None,
            entry: None,
            wheel_login: false,
            guest_attempted: false,
        }
    }

    fn update_offer(&mut self, ctx: &mut Ctx) {
        if !self.phone && !self.check_on_enter {
            if ctx.portal.state == State::Checking {
                self.guest_attempted = false;
            }
            if ctx.portal.state == State::Guest && !self.guest_attempted {
                ctx.portal.observe(&ctx.net.state());
                if ctx.portal.state == State::Guest {
                    self.guest_attempted = true;
                    ctx.portal.guest();
                }
            }
        }
        self.refresh(ctx);
    }

    fn refresh(&mut self, ctx: &Ctx) {
        let state = if self.phone && !self.pairing {
            if ctx.portal.has_prepared() {
                State::Prepared
            } else {
                State::Idle
            }
        } else {
            ctx.portal.state.clone()
        };
        if self.state != state {
            self.codes = match &state {
                State::Pairing { ap, page } if self.phone => PairCodes::new(ap, page),
                _ => None,
            };
            self.state = state;
        }
        if self.wheel_login && self.state == State::Credentials {
            self.entry.get_or_insert_with(|| {
                Entry::Account(TextEntry::capture_secret("SkyMiles Account"))
            });
        } else {
            self.entry = None;
            if matches!(self.state, State::Checking | State::Delta | State::Online) {
                self.wheel_login = false;
            }
        }
        let rows = self.rows(ctx.portal.has_prepared(), saved_account(ctx).is_some());
        let first_action = rows
            .iter()
            .position(|row| !matches!(row.action, RowAction::None));
        self.list.set_rows(rows);
        if self
            .list
            .selected_row()
            .is_some_and(|row| matches!(row.action, RowAction::None))
        {
            if let Some(index) = first_action {
                self.list.select(index);
            }
        }
    }

    fn rows(&self, prepared: bool, saved: bool) -> Vec<Row> {
        match (&self.state, self.phone) {
            (State::Idle, true) => vec![
                Row::new("Start Phone Setup", RowAction::Cmd(START_PHONE))
                    .detail("Join the player's private Wi-Fi on your phone"),
            ],
            (State::Prepared, true) => vec![
                Row::new("Account Ready", RowAction::None)
                    .detail("Available for 30 minutes on this player"),
                Row::new("Done", RowAction::Cmd(DONE)),
                Row::new("Clear Prepared Account", RowAction::Cmd(CLEAR_PREPARED)),
            ],
            (State::Online, false) => vec![
                Row::new("Internet Available", RowAction::None),
                Row::new("Done", RowAction::Cmd(DONE)),
            ],
            (State::Terms, false) => vec![
                Row::new("Terms & Conditions apply", RowAction::None),
                Row::new("Agree", RowAction::Cmd(AGREE)),
            ],
            (State::Delta, false) => {
                let mut rows = Vec::new();
                if prepared {
                    rows.push(Row::new(
                        "Use Prepared Account",
                        RowAction::Cmd(USE_PREPARED),
                    ));
                }
                if saved {
                    rows.push(Row::new("Use Saved Account", RowAction::Cmd(USE_SAVED))
                        .detail("From config.toml on the card"));
                }
                if !prepared {
                    rows.push(Row::screen("Enter Details on Phone", PHONE_ENTRY));
                }
                rows.push(Row::screen("Enter SkyMiles Details", ACCOUNT).detail("Use the wheel"));
                rows
            }
            _ => Vec::new(),
        }
    }

    fn failure(&self) -> Option<&str> {
        match &self.state {
            State::Failed(message) => Some(message),
            State::Guest if self.guest_attempted => Some("Wi-Fi still needs sign-in"),
            State::Unsupported => Some("This Wi-Fi sign-in needs a browser"),
            State::Unknown => Some("Couldn't verify internet access"),
            State::Pairing { .. } if self.codes.is_none() => {
                Some("Couldn't show the phone setup codes")
            }
            State::Idle | State::Prepared if !self.phone => Some("Wi-Fi sign-in stopped"),
            _ => None,
        }
    }

    fn retry(&mut self, ctx: &mut Ctx) {
        if self.phone {
            ctx.portal.pair();
            self.pairing = true;
        } else {
            self.guest_attempted = false;
            ctx.portal.check();
        }
    }

    fn return_to_delta(&mut self, ctx: &mut Ctx) {
        self.wheel_login = false;
        self.entry = None;
        ctx.portal.check();
        self.refresh(ctx);
    }

    fn edit_event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        let Some(entry) = &mut self.entry else {
            return Action::None;
        };
        match entry.field_mut().edit(ev) {
            Edit::Editing => {}
            Edit::Back => {
                self.return_to_delta(ctx);
                return Action::Stay;
            }
            Edit::Done => {
                let text = Zeroizing::new(entry.field_mut().take_text());
                if text.is_empty() {
                    return Action::Toast("Enter your SkyMiles details".into());
                }
                match self.entry.take() {
                    Some(Entry::Account(_)) => {
                        let user = Zeroizing::new(text.trim().to_owned());
                        self.entry = Some(if user.is_empty() {
                            Entry::Account(TextEntry::capture_secret("SkyMiles Account"))
                        } else {
                            Entry::Password {
                                user,
                                field: TextEntry::capture_secret("SkyMiles Password"),
                            }
                        });
                    }
                    Some(Entry::Password { user, .. }) => {
                        ctx.portal.submit_credentials(user, text);
                        self.refresh(ctx);
                    }
                    None => {}
                }
            }
        }
        Action::None
    }
}

impl Screen for PortalScreen {
    fn clickable(&self, x: i32, y: i32) -> bool {
        if let Some(entry) = &self.entry {
            return entry.field().clickable(x, y);
        }
        self.list.retry_hit(x, y) || self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        let hit = if let Some(entry) = &mut self.entry {
            entry.field_mut().select_cell(x, y)
        } else {
            self.list.retry_hit(x, y) || self.list.select_at(x, y)
        };
        if hit { self.event(Event::Center, ctx) } else { Action::None }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        use crate::ui::hierarchy::Surface;
        if let Some(entry) = &self.entry {
            return entry.field().audit();
        }
        if let Some(codes) = &self.codes {
            return Surface::widget(
                "Phone Setup",
                if codes.show_page {
                    "Setup QR"
                } else {
                    "Wi-Fi QR"
                }
                .into(),
            );
        }
        if self.failure().is_some() {
            return Surface::widget("Portal Status", "Retry".into());
        }
        if matches!(
            self.state,
            State::Checking | State::Working | State::Credentials
        ) {
            return Surface::widget("Portal Status", "Loading".into());
        }
        Surface::list(&self.list)
    }

    fn title(&self) -> String {
        if let Some(entry) = &self.entry {
            return entry.field().title();
        }
        match (&self.state, self.phone) {
            (_, true) => "Prepare Delta Sign-In",
            (State::Delta, false) => "Delta Wi-Fi",
            (State::Terms, false) => "Wi-Fi Agreement",
            _ => "Wi-Fi Sign-In",
        }
        .into()
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        if std::mem::take(&mut self.check_on_enter) {
            ctx.portal.observe(&ctx.net.state());
            if !matches!(ctx.portal.state, State::Guest | State::Checking) {
                ctx.portal.check();
            }
        }
        self.update_offer(ctx);
    }

    fn portal_changed(&mut self, ctx: &mut Ctx) {
        self.update_offer(ctx);
    }

    fn network_changed(&mut self, ctx: &mut Ctx) {
        self.update_offer(ctx);
    }

    fn leave(&mut self, ctx: &mut Ctx) {
        self.wheel_login = false;
        self.guest_attempted = false;
        self.entry = None;
        self.codes = None;
        if !self.phone || self.pairing {
            ctx.portal.cancel();
        }
    }

    fn interrupted(&mut self, ctx: &mut Ctx) {
        self.leave(ctx);
        self.pairing = false;
        self.check_on_enter = !self.phone;
        self.state = State::Idle;
    }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        if self.entry.is_some() {
            // Observe the owner snapshot before processing even the final wheel press.
            ctx.portal.observe(&ctx.net.state());
            if ctx.portal.state != State::Credentials {
                self.refresh(ctx);
                return Action::Stay;
            }
            return self.edit_event(ev, ctx);
        }
        if matches!(ev, Event::Back) {
            if self.wheel_login {
                ctx.portal.observe(&ctx.net.state());
                self.return_to_delta(ctx);
                return Action::Stay;
            }
            return Action::Pop;
        }
        if let Some(codes) = &mut self.codes {
            if matches!(ev, Event::Center) {
                codes.show_page = !codes.show_page;
            }
            return Action::None;
        }
        if self.failure().is_some() && matches!(ev, Event::Center) {
            self.retry(ctx);
            self.refresh(ctx);
            return Action::None;
        }
        match ev {
            Event::Scroll(ticks) => {
                self.list.wheel(ticks, ctx.now);
            }
            Event::WheelEnd => self.list.end_spin(),
            Event::Center => {
                match self.list.selected_row().map(|row| &row.action) {
                    Some(RowAction::Cmd(AGREE)) => {
                        ctx.portal.observe(&ctx.net.state());
                        ctx.portal.accept();
                    }
                    Some(RowAction::Cmd(ACCOUNT)) => {
                        ctx.portal.observe(&ctx.net.state());
                        if ctx.portal.state == State::Delta {
                            self.wheel_login = true;
                            ctx.portal.begin_login();
                        }
                    }
                    Some(RowAction::Cmd(USE_PREPARED)) => {
                        // The network owner may have published a roam before its UI message.
                        ctx.portal.observe(&ctx.net.state());
                        if matches!(ctx.portal.state, State::Delta) {
                            ctx.portal.login_prepared();
                        }
                    }
                    Some(RowAction::Cmd(USE_SAVED)) => {
                        ctx.portal.observe(&ctx.net.state());
                        if let (State::Delta, Some((user, password))) = (&ctx.portal.state, saved_account(ctx)) {
                            ctx.portal.login_saved(user, password);
                        }
                    }
                    Some(RowAction::Cmd(PHONE_ENTRY)) => {
                        self.check_on_enter = true;
                        return Action::Push(Box::new(Self::phone()));
                    }
                    Some(RowAction::Cmd(START_PHONE)) => self.retry(ctx),
                    Some(RowAction::Cmd(CLEAR_PREPARED)) => {
                        self.pairing = false;
                        ctx.portal.forget_prepared();
                    }
                    Some(RowAction::Cmd(DONE)) => return Action::Pop,
                    _ => {}
                }
                self.update_offer(ctx);
            }
            _ => {}
        }
        Action::None
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if let Some(entry) = &mut self.entry {
            self.list.clear_hit_targets();
            entry.field_mut().draw(d, area);
        } else if let Some(codes) = &self.codes {
            self.list.clear_hit_targets();
            codes.draw(d, area);
        } else if let Some(text) = self.failure() {
            let target = crate::ui::state::draw_target(d, area, View::Failed { text, retry: true });
            self.list.set_state_target(target);
        } else if matches!(
            self.state,
            State::Checking | State::Working | State::Credentials
        ) {
            self.list.draw_state(d, area, View::Loading);
        } else {
            self.list.draw(d, area);
        }
    }

    fn animate(&mut self, now: Instant) -> bool {
        if let Some(entry) = &mut self.entry {
            entry.field_mut().animate(now)
        } else {
            self.list.animate(now)
        }
    }

    fn script_ready(&self) -> bool {
        if self.entry.is_some() {
            return true;
        }
        match self.state {
            State::Checking | State::Working | State::Credentials => false,
            _ => true,
        }
    }
}

/// The Delta account in config.toml's `[portal.delta]`, when complete.
fn saved_account(ctx: &Ctx) -> Option<(zeroize::Zeroizing<String>, zeroize::Zeroizing<String>)> {
    ctx.config.portal.delta.as_ref().and_then(|account| account.sign_in())
}
