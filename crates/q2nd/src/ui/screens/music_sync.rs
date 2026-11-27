use super::controls::Confirm;
use crate::ui::{
    Action, Ctx, Draw, Screen,
    list::{Badge, ListView, Row, RowAction, index_letter},
    state::View,
};
use crate::{
    gfx::Rect,
    input::Event,
    library::{LibError, music_reconcile::Review},
};
use std::{any::Any, sync::Arc, time::Instant};

pub(super) struct FullSync {
    list: ListView,
    review: Option<Arc<Review>>,
    state: String,
    error: Option<String>,
    started: bool,
    selected: Vec<bool>,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
}

impl FullSync {
    pub(super) fn new() -> Self {
        Self {
            list: ListView::settings(false),
            review: None,
            state: "Checking library…".into(),
            error: None,
            started: false,
            selected: Vec::new(),
            cancelled: Arc::default(),
        }
    }
    fn start(&mut self, ctx: &Ctx) {
        self.started = true;
        self.error = None;
        self.state = "Checking library…".into();
        let lib = ctx.lib.clone();
        let cancelled = self.cancelled.clone();
        ctx.task(0, move |reply| async move {
            let result = lib
                .review_downloads(index_letter, &cancelled, |n| reply.send(n))
                .await;
            reply.send(result);
        });
    }
}

impl Drop for FullSync {
    fn drop(&mut self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
    }
}

impl Screen for FullSync {

    fn clickable(&self, x: i32, y: i32) -> bool {
        self.list.retry_hit(x, y) || self.list.hit(x, y).is_some()
    }

    fn click(&mut self, x: i32, y: i32, ctx: &mut Ctx) -> Action {
        if self.list.retry_hit(x, y) || self.list.select_at(x, y) {
            self.event(Event::Center, ctx)
        } else {
            Action::None
        }
    }

    #[cfg(test)]
    fn audit(&self) -> crate::ui::hierarchy::Surface {
        crate::ui::hierarchy::Surface::list(&self.list)
    }
    fn title(&self) -> String {
        "Full Sync".into()
    }
    fn enter(&mut self, ctx: &mut Ctx) {
        if !self.started {
            self.start(ctx);
        }
    }
    fn script_ready(&self) -> bool {
        self.error.is_some() || self.review.is_some()
    }
    fn loaded(&mut self, _: u64, data: Box<dyn Any + Send>, _: &mut Ctx) {
        if let Some(n) = data.downcast_ref::<usize>() {
            self.state = format!("Checked {n} downloads…");
            return;
        }
        let Ok(result) = data.downcast::<Result<Review, LibError>>() else {
            return;
        };
        match *result {
            Ok(review) => {
                let mut rows = vec![
                    Row::new("Library checked", RowAction::None)
                        .detail(format!("{} upgrades queued", review.upgrades)),
                ];
                if review.missing.is_empty() {
                    rows.push(Row::new("No missing downloads", RowAction::None));
                } else {
                    self.selected = vec![true; review.missing.len()];
                    rows.push(
                        Row::screen("Remove Selected…", 1).detail("Uncheck any downloads to keep"),
                    );
                    for entry in &review.missing {
                        rows.push(
                            Row::new(entry.song.title.clone(), RowAction::Cmd(2))
                                .key(entry.song.id.clone())
                                .badge(Badge::Check)
                                .detail(format!("{} · {}", entry.song.artist, entry.song.album)),
                        );
                    }
                }
                self.list.set_rows(rows);
                self.review = Some(Arc::new(review));
            }
            Err(error) => {
                self.error = Some(crate::ux::present_in(
                    &error.to_string(),
                    crate::ux::Journey::Download,
                ))
            }
        }
    }
    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::Scroll(n) => {
                self.list.wheel(n, ctx.now);
            }
            Event::WheelEnd => self.list.end_spin(),
            Event::Center if self.error.is_some() => {
                self.start(ctx);
            }
            Event::Center if self.list.selected == 1 => {
                let Some(review) = self
                    .review
                    .as_ref()
                    .filter(|r| !r.missing.is_empty())
                    .cloned()
                else {
                    return Action::None;
                };
                let missing: Vec<_> = review
                    .missing
                    .iter()
                    .zip(&self.selected)
                    .filter(|(_, keep)| **keep)
                    .map(|(song, _)| song.clone())
                    .collect();
                let n = missing.len();
                if n == 0 {
                    return Action::Toast("No downloads selected".into());
                }
                let review = Arc::new(Review {
                    generation: review.generation,
                    upgrades: review.upgrades,
                    missing,
                });
                return Action::Push(Box::new(Confirm::new(
                    "Remove Missing Downloads?",
                    format!("Remove up to {n} listed tracks\nfrom this card?"),
                    "Remove",
                    move |ctx| {
                        let result = ctx.lib.confirm_missing(review.clone());
                        ctx.notice_task(async move {
                            match result.await {
                                Ok(n) => format!("Removed {n} downloads"),
                                Err(error) => crate::ux::present_in(
                                    &error.to_string(),
                                    crate::ux::Journey::Download,
                                ),
                            }
                        });
                        Action::Many(vec![Action::Pop, Action::Pop])
                    },
                )));
            }
            Event::Center if self.list.selected >= 2 => {
                let i = self.list.selected;
                self.selected[i - 2] = !self.selected[i - 2];
                self.list.rows[i].badge = if self.selected[i - 2] {
                    Badge::Check
                } else {
                    Badge::None
                };
            }
            _ => {}
        }
        Action::None
    }
    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
    fn draw(&mut self, d: &mut Draw, area: Rect) {
        if let Some(error) = &self.error {
            self.list.draw_state(d,
                area,
                View::Failed {
                    text: error,
                    retry: true,
                },
            );
        } else if self.review.is_some() {
            self.list.draw(d, area);
        } else {
            self.list.draw_state(d,area, View::Empty(&self.state));
        }
    }
}
