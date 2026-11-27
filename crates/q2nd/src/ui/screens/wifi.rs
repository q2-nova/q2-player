//! Wi-Fi: on or off, the network joined, and the ones around to join.

use super::controls::Confirm;
use super::portal::PortalScreen;
use super::text::{Kind, TextEntry};
use crate::config::WifiNetwork;
use crate::gfx::Rect;
use crate::input::Event;
use crate::ui::list::{ListView, Row, RowAction};
use crate::ui::portal::State as PortalState;
use crate::ui::{Action, Ctx, Draw, Request, Screen};
use std::time::Instant;

const TOGGLE: u32 = 1;
const SCAN: u32 = 2;
const FORGET: u32 = 3;
const PORTAL: u32 = 4;
const PREPARE_DELTA: u32 = 5;
/// Networks are commands from here up, by index into the scan.
const NETWORK: u32 = 100;

pub struct Wifi {
    list: ListView,
    /// Commands refer to the displayed network, even if a newer scan is
    /// published before its UI notification reaches this screen.
    displayed: crate::net::WifiState,
}

impl Wifi {
    pub fn new() -> Wifi {
        Wifi { list: ListView::settings(true), displayed: crate::net::WifiState::default() }
    }

    fn rows(ctx: &Ctx, st: &crate::net::WifiState) -> Vec<Row> {
        let mut rows = vec![Row::new("Wi-Fi", RowAction::Cmd(TOGGLE)).key("#toggle").value(if st.enabled { "On" } else { "Off" })];
        if let Some(error) = &st.error { rows[0] = rows[0].clone().detail(error); }
        if !st.enabled {
            return rows;
        }
        let status = match (&st.ssid, &st.ip) {
            (Some(s), Some(ip)) => format!("{s} · {ip}"),
            (Some(s), None) => format!("{s} · getting an address"),
            _ if st.wpa_state.is_empty() || st.wpa_state == "OFF" => "Starting…".into(),
            _ => "Not connected".into(),
        };
        rows.push(Row::new("Connected To", RowAction::None).key("#status").detail(status));
        if st.ssid.is_some() && st.ip.is_some() {
            let detail = match ctx.portal.state {
                PortalState::Guest | PortalState::Delta => "Sign-in required",
                PortalState::Unsupported => "A browser is required",
                _ => "",
            };
            rows.push(Row::screen("Wi-Fi Sign-In", PORTAL).key("#portal").detail(detail));
        }
        rows.push(Row::screen("Prepare Delta Sign-In", PREPARE_DELTA).key("#prepare-delta"));
        if st.ssid.is_some() {
            rows.push(Row::screen("Forget This Network…", FORGET).key("#forget"));
        }
        rows.push(Row::new("Scan", RowAction::Cmd(SCAN)).key("#scan"));
        for (i, n) in st.scan.iter().enumerate() {
            let known = st.known.contains(&n.ssid);
            let bars = match n.signal {
                s if s >= -60 => "Strong",
                s if s >= -72 => "Good",
                _ => "Weak",
            };
            let lock = if n.secured { "Secured" } else { "Open" };
            let detail = if known { format!("Saved · {lock}") } else { lock.to_string() };
            let needs_password = n.secured && !known && !ctx.config.wifi.networks.iter().any(|k| k.ssid == n.ssid);
            let row = if needs_password { Row::screen(n.ssid.clone(), NETWORK + i as u32) }
                else { Row::new(n.ssid.clone(), RowAction::Cmd(NETWORK + i as u32)) };
            rows.push(
                row
                    .key(format!("net:{}", n.ssid))
                    .detail(detail)
                    .value(bars),
            );
        }
        rows
    }

    fn refresh_rows(&mut self, ctx: &Ctx) {
        let state = ctx.net.state();
        self.list.set_rows(Self::rows(ctx, &state));
        self.displayed = state;
    }

}

impl Screen for Wifi {

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
    fn title(&self) -> String {
        "Wi-Fi".into()
    }

    fn enter(&mut self, ctx: &mut Ctx) {
        ctx.net.scan();
        self.refresh_rows(ctx);
    }

    fn network_changed(&mut self, ctx: &mut Ctx) { self.refresh_rows(ctx); }
    fn portal_changed(&mut self, ctx: &mut Ctx) { self.refresh_rows(ctx); }

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(t) => {
                self.list.wheel(t, ctx.now);
                Action::None
            }
            Event::WheelEnd => { self.list.end_spin(); Action::None }
            Event::Center => {
                let Some(RowAction::Cmd(cmd)) = self.list.selected_row().map(|r| r.action.clone()) else {
                    return Action::None;
                };
                let st = &self.displayed;
                let a = match cmd {
                    TOGGLE => {
                        ctx.config.wifi.enabled = !st.enabled;
                        *ctx.config_changed = true;
                        ctx.requests.push(Request::Wifi(!st.enabled));
                        Action::None
                    }
                    SCAN => {
                        ctx.net.scan();
                        Action::Toast("Scanning…".into())
                    }
                    PORTAL => return Action::Push(Box::new(PortalScreen::wifi())),
                    PREPARE_DELTA => return Action::Push(Box::new(PortalScreen::phone())),
                    FORGET => {
                        if let Some(ssid) = st.ssid.clone() {
                            return Action::Push(Box::new(Confirm::forget_network(move |ctx| {
                                    ctx.config.wifi.networks.retain(|n| n.ssid != ssid);
                                    *ctx.config_changed = true;
                                    ctx.net.forget(&ssid);
                                    Action::Pop
                                },
                            )));
                        }
                        Action::None
                    }
                    n if n >= NETWORK => {
                        let Some(net) = st.scan.get((n - NETWORK) as usize).cloned() else { return Action::None };
                        let saved = st.known.contains(&net.ssid);
                        let from_config = ctx.config.wifi.networks.iter().find(|k| k.ssid == net.ssid).cloned();
                        match (net.secured, saved, from_config) {
                            (_, _, Some(k)) => {
                                ctx.net.join(k);
                                Action::Toast(format!("Joining {}", net.ssid))
                            }
                            (_, true, None) => {
                                // Saved by wpa_supplicant (here or in the stock player): the
                                // password is there already.
                                ctx.net.join_saved(&net.ssid);
                                Action::Toast(format!("Joining {}", net.ssid))
                            }
                            (false, false, None) => {
                                ctx.net.join(WifiNetwork { ssid: net.ssid.clone(), psk: String::new(), security: net.security });
                                Action::Toast(format!("Joining {}", net.ssid))
                            }
                            (true, false, None) => {
                                let ssid = net.ssid.clone();
                                Action::Push(Box::new(TextEntry::new(
                                    format!("Password for {}", net.ssid),
                                    "",
                                    Kind::Full,
                                    true,
                                    Box::new(move |psk, ctx| {
                                        ctx.net.join(WifiNetwork { ssid: ssid.clone(), psk, security: net.security });
                                        Action::Many(vec![Action::Pop, Action::Toast(format!("Joining {ssid}"))])
                                    }),
                                )))
                            }
                        }
                    }
                    _ => Action::None,
                };
                self.refresh_rows(ctx);
                a
            }
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        self.list.draw(d, area);
    }

    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}
