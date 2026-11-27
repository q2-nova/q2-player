use super::*;
use crate::{ItemKind, gfx::HEIGHT, ui::theme::ROW_H};

type Handler = Box<dyn Fn(&mut Ctx) -> Action>;

pub struct Popup {
    title: String,
    list: ListView,
    handlers: Vec<Handler>,
}

impl Popup {
    pub fn new(title: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            list: ListView::new(false),
            handlers: Vec::new(),
        }
    }

    pub fn add(mut self, label: &str, handler: impl Fn(&mut Ctx) -> Action + 'static) -> Self {
        let index = self.handlers.len() as u32;
        let mut rows = std::mem::take(&mut self.list.rows);
        rows.push(Row::new(label, RowAction::Cmd(index)).key(index.to_string()));
        self.list.set_rows(rows);
        self.handlers.push(Box::new(handler));
        self
    }

    pub fn report_problem(self) -> Self {
        self.add("Report a Problem", |_| {
            Action::Toast("Reporting is simulated".into())
        })
    }
}

impl Screen for Popup {
    fn title(&self) -> String {
        self.title.clone()
    }
    fn is_overlay(&self) -> bool {
        true
    }
    fn selected_label(&self) -> Option<&str> {
        self.list.selected_row().map(|row| row.title.as_str())
    }

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

    fn event(&mut self, ev: Event, ctx: &mut Ctx) -> Action {
        match ev {
            Event::WheelEnd => {
                self.list.end_spin();
                Action::None
            }
            Event::Scroll(ticks) => {
                self.list.wheel(ticks, ctx.now);
                Action::None
            }
            Event::Center => {
                let Some(RowAction::Cmd(index)) = self.list.selected_row().map(|row| &row.action)
                else {
                    return Action::None;
                };
                match (self.handlers[*index as usize])(ctx) {
                    Action::Push(screen) => Action::Replace(screen),
                    Action::None | Action::Pop | Action::Stay => Action::Pop,
                    action => Action::Many(vec![Action::Pop, action]),
                }
            }
            Event::CenterLong => Action::Pop,
            _ => Action::None,
        }
    }

    fn draw(&mut self, d: &mut Draw, area: Rect) {
        let rows = self.list.rows.len().min(6) as i32;
        let head = 34;
        let h = head + rows * ROW_H + 8;
        let w = WIDTH - 64;
        let r = Rect::new((WIDTH - w) / 2, area.y + (area.h - h) / 2, w, h);
        d.c.set_clip(area);
        let saved = d.theme;
        d.c.fill_round_rect_alpha(r, 12, saved.hud, saved.hud_alpha);
        let title = d
            .fonts
            .ellipsize(&self.title, 15, Weight::Bold, w - 32)
            .into_owned();
        let line_height = d.fonts.line_height(15);
        d.fonts.draw_centered(
            d.c,
            &title,
            WIDTH / 2,
            r.y + (head - line_height) / 2 + 2,
            15,
            Weight::Bold,
            Color(0xB8B8B8),
        );
        let list_area = Rect::new(r.x + 6, r.y + head, r.w - 12, rows * ROW_H);
        d.theme.text = Color::WHITE;
        d.theme.muted = Color(0xB8B8B8);
        d.theme.chevron = Color(0xB8B8B8);
        d.theme.scroll_track = Color(0x2A2A2A);
        d.theme.scroll_line = Color(0x444444);
        let old = d.c.set_clip(list_area.intersect(&area));
        self.list.draw(d, list_area);
        d.c.set_clip(old.intersect(&Rect::new(0, 0, WIDTH, HEIGHT)));
        d.theme = saved;
    }

    fn animate(&mut self, now: Instant) -> bool {
        self.list.animate(now)
    }
}

pub fn for_row(row: &Row, ctx: &mut Ctx) -> Option<Box<dyn Screen>> {
    if let Some(song) = row.song_item() {
        let song = song.clone();
        let id = song.id.clone();
        let loved = ctx.lib.loved(&song);
        let songs = Arc::new(vec![song]);
        let next = songs.clone();
        let last = songs.clone();
        return Some(Box::new(
            Popup::new(&row.title)
                .add("Play", move |_| Action::Play {
                    songs: songs.clone(),
                    index: 0,
                    shuffle: false,
                })
                .add("Play Next", move |ctx| {
                    ctx.player.send(Cmd::Enqueue {
                        songs: next.clone(),
                        next: true,
                    });
                    Action::Toast("Added to Up Next".into())
                })
                .add("Add to Up Next", move |ctx| {
                    ctx.player.send(Cmd::Enqueue {
                        songs: last.clone(),
                        next: false,
                    });
                    Action::Toast("Added to Up Next".into())
                })
                .add(if loved { "Unlove" } else { "Love" }, move |ctx| {
                    ctx.lib.star(ItemKind::Song, &id, !loved);
                    Action::None
                }),
        ));
    }
    match &row.action {
        RowAction::Open(dest @ Dest::Album { id, .. }) => {
            let songs = selected_songs(ctx, |song| song.album_id.as_ref() == Some(id));
            let play = songs.clone();
            let next = songs.clone();
            let last = songs.clone();
            let download = if ctx.lib.album_downloaded(id) {
                "Remove Download"
            } else {
                "Download Album"
            };
            let id = id.clone();
            let dest = dest.clone();
            Some(Box::new(
                Popup::new(&row.title)
                    .add("Play", move |_| Action::Play {
                        songs: play.clone(),
                        index: 0,
                        shuffle: false,
                    })
                    .add("Shuffle", move |_| Action::Play {
                        songs: songs.clone(),
                        index: 0,
                        shuffle: true,
                    })
                    .add("Play Next", move |ctx| {
                        ctx.player.send(Cmd::Enqueue {
                            songs: next.clone(),
                            next: true,
                        });
                        Action::Toast("Added to Up Next".into())
                    })
                    .add("Add to Up Next", move |ctx| {
                        ctx.player.send(Cmd::Enqueue {
                            songs: last.clone(),
                            next: false,
                        });
                        Action::Toast("Added to Up Next".into())
                    })
                    .add(download, move |ctx| toggle_album_download(&id, ctx))
                    .add("Browse Album", move |ctx| {
                        Action::Push(open(dest.clone(), ctx))
                    }),
            ))
        }
        _ => None,
    }
}
