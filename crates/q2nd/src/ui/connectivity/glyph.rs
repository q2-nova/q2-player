//! One connectivity badge, ordered by the dependency that failed.
use crate::gfx::{Canvas, Color};
use crate::ui::icons;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Badge { OfflineMode, WifiUnavailable, TailscaleUnavailable, NavidromeUnavailable, Healthy }

pub fn select(offline_mode: bool, wifi_online: bool, tailscale_enabled: bool,
    tailscale_online: bool, navidrome_online: bool) -> Badge {
    if offline_mode { Badge::OfflineMode }
    else if !wifi_online { Badge::WifiUnavailable }
    else if tailscale_enabled && !tailscale_online { Badge::TailscaleUnavailable }
    else if !navidrome_online { Badge::NavidromeUnavailable }
    else { Badge::Healthy }
}

pub fn draw(c: &mut Canvas, badge: Badge, x: f32, y: f32, bars: u8, ink: Color, background: Color) {
    match badge {
        Badge::OfflineMode => icons::airplane(c, x, y, ink),
        Badge::WifiUnavailable => icons::offline(c, x, y, Color(0x7A7A7A)),
        Badge::TailscaleUnavailable => icons::vpn_unavailable(c, x, y, ink, background),
        Badge::NavidromeUnavailable => icons::navidrome_unavailable(c, x, y, background),
        Badge::Healthy => icons::wifi(c, x + 11.0, y + 14.0, bars, ink, Color(0xA0A0A0)),
    }
}
