//! The file shelf: a temporary landing place for files while you move them between windows.
//! Drop files on the notch to put them here; drag a tile out (to Explorer, a chat, a mail window)
//! to hand the original file to whatever is under the cursor; click a tile to open it.
//!
//! The shelf holds *references*, never copies and never takes ownership: removing a tile only
//! forgets it, and dragging out offers copy/link (never move), so nothing a user owns can be moved
//! or deleted from here. Files arrive as `FileDropped` events from the platform's OLE drop target
//! (or, in phase 11, from the iPhone listener — same event, same rendering, no tag).

use std::sync::Arc;

use crate::config::{Config, ShelfCfg};
use crate::draw::{Align, Canvas, CursorKind, DrawCmd, HitId, ImageId, TextStyle};
use crate::events::{Event, EventKind, EventMask, FileEntry, Kind, Source};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{Command, Cx, DrawCx, Module, ModuleId, ShelfCmd, Visibility};
use crate::spring::{Spring, SpringParams};

pub const COLS: usize = 5;
pub const VISIBLE_ROWS: usize = 2;
pub const HEADER_H: f32 = 28.0;
pub const TILE_H: f32 = 78.0;
const GAP: f32 = 8.0;
/// Pointer travel (DIPs) before a press on a tile becomes a drag-out.
const DRAG_START: f32 = 6.0;

const HIT_CLEAR: HitId = HitId(1);
const HIT_ALL: HitId = HitId(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Tile(usize),
    Remove(usize),
}

fn hit(p: Part) -> HitId {
    HitId(match p {
        Part::Tile(i) => 100 + i as u32,
        Part::Remove(i) => 2_000 + i as u32,
    })
}

fn part_of(h: HitId) -> Option<Part> {
    match h.0 {
        100..=1_999 => Some(Part::Tile(h.0 as usize - 100)),
        2_000..=3_999 => Some(Part::Remove(h.0 as usize - 2_000)),
        _ => None,
    }
}

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.shelf
        .enabled
        .then(|| Box::new(Shelf::new(cfg.shelf.clone())) as Box<dyn Module>)
}

/// `1_234_567` → `"1.2 MB"` (binary units, one decimal from KB up).
pub fn fmt_size(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

#[derive(Clone, Debug, PartialEq)]
pub struct Item {
    pub id: u64,
    pub name: Arc<str>,
    pub path: Arc<str>,
    pub size: u64,
    pub thumb: u64,
    pub is_dir: bool,
    pub source: Source,
}

impl Item {
    fn from_entry(e: &FileEntry, source: Source) -> Item {
        Item {
            id: e.id,
            name: e.name.clone(),
            path: e.path.clone(),
            size: e.size,
            thumb: e.thumb,
            is_dir: e.is_dir,
            source,
        }
    }
}

pub struct Shelf {
    cfg: ShelfCfg,
    /// Newest first.
    items: Vec<Item>,
    /// A drag is over the notch's drop target.
    drag_over: bool,
    hover: Option<HitId>,
    /// A drag-out already started for the current press.
    dragging: bool,
    scroll: Spring,
    last_frame: f64,
    grid: Rect,
}

impl Shelf {
    pub fn new(cfg: ShelfCfg) -> Shelf {
        Shelf {
            cfg,
            items: Vec::new(),
            drag_over: false,
            hover: None,
            dragging: false,
            scroll: Spring::new(0.0, SpringParams::new(24.0, 1.0)),
            last_frame: 0.0,
            grid: Rect::default(),
        }
    }

    pub fn items(&self) -> &[Item] {
        &self.items
    }

    fn rows(&self) -> usize {
        self.items.len().div_ceil(COLS)
    }

    fn max_scroll(&self) -> f32 {
        self.rows().saturating_sub(VISIBLE_ROWS) as f32
    }

    fn scroll_by(&mut self, rows: f32) {
        let t = (self.scroll.target() + rows).clamp(0.0, self.max_scroll());
        self.scroll.set_target(t);
    }

    /// Thumbnails of items that are no longer shown, to be freed by the platform.
    fn release(&self, removed: &[Item], cx: &mut Cx) {
        let ids: Vec<u64> = removed
            .iter()
            .map(|i| i.thumb)
            .filter(|t| *t != 0)
            .collect();
        if !ids.is_empty() {
            cx.command(Command::Shelf(ShelfCmd::Release(ids)));
        }
    }

    fn remove_where(&mut self, keep: impl Fn(&Item) -> bool, cx: &mut Cx) {
        let (kept, removed): (Vec<Item>, Vec<Item>) =
            std::mem::take(&mut self.items).into_iter().partition(keep);
        self.items = kept;
        self.release(&removed, cx);
        let t = self.scroll.target().min(self.max_scroll());
        self.scroll.set_target(t);
    }

    /// Tile rectangle for grid position `i` before scrolling.
    fn tile_rect(grid: Rect, i: usize) -> Rect {
        let tw = (grid.w - GAP * (COLS as f32 - 1.0)) / COLS as f32;
        let (c, r) = (i % COLS, i / COLS);
        Rect::new(
            grid.x + c as f32 * (tw + GAP),
            grid.y + r as f32 * TILE_H,
            tw,
            TILE_H - 4.0,
        )
    }

    fn draw_tile(&self, cv: &mut Canvas, r: Rect, i: usize, item: &Item, visible: Rect) {
        let th = *cv.theme;
        let hovered = matches!(
            self.hover.and_then(part_of),
            Some(Part::Tile(k) | Part::Remove(k)) if k == i
        );
        if hovered {
            cv.round_rect(r, 10.0, th.surface);
        }
        let art = Rect::new(r.center().x - 22.0, r.y + 6.0, 44.0, 44.0);
        if item.thumb != 0 {
            cv.image(ImageId(item.thumb), art, 9.0);
        } else {
            cv.squircle(art, 9.0, th.surface_hi);
            cv.icon(
                if item.is_dir { Icon::Folder } else { Icon::Doc },
                art.inset(10.0),
                th.text_dim,
            );
        }
        cv.text(
            Rect::new(r.x + 2.0, r.y + 51.0, r.w - 4.0, 13.0),
            &item.name,
            TextStyle::new(11.0, crate::draw::Weight::Medium).align(Align::Center),
            th.text,
        );
        let sub = if item.is_dir {
            "Folder".to_string()
        } else {
            fmt_size(item.size)
        };
        cv.text(
            Rect::new(r.x + 2.0, r.y + 62.0, r.w - 4.0, 11.0),
            sub,
            TextStyle::new(10.0, crate::draw::Weight::Regular).align(Align::Center),
            th.text_faint,
        );
        // The tile is a drag source; a small ✕ (on hover) forgets it.
        let body = r.intersect(&visible);
        if let Some(b) = body {
            cv.hit_drag(b, hit(Part::Tile(i)));
        }
        if hovered {
            let x = Rect::new(r.right() - 17.0, r.y + 1.0, 16.0, 16.0);
            cv.circle(x.center(), 8.0, th.surface_hi);
            cv.icon(Icon::Close, x.inset(4.0), th.text);
            if let Some(h) = x.intersect(&visible) {
                // Registered after the tile: it wins hit tests where they overlap.
                cv.hit(h, hit(Part::Remove(i)), CursorKind::Hand);
            }
        }
    }
}

impl Module for Shelf {
    fn id(&self) -> ModuleId {
        "shelf"
    }

    fn title(&self) -> &'static str {
        "Shelf"
    }

    fn icon(&self) -> Icon {
        Icon::Tray
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::FileDropped, Kind::DragHover])
    }

    fn wants_frames(&self) -> bool {
        !self.scroll.is_settled()
    }

    fn expanded_size(&self) -> Size {
        Size::new(
            424.0,
            14.0 + HEADER_H + 4.0 + TILE_H * VISIBLE_ROWS as f32 + 26.0,
        )
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::FileDropped(files) => {
                // Newest first; dropping a file that is already shelved moves it to the front.
                for f in files.iter().rev() {
                    if let Some(pos) = self.items.iter().position(|i| i.path == f.path) {
                        let old = self.items.remove(pos);
                        if old.thumb != f.thumb && old.thumb != 0 {
                            cx.command(Command::Shelf(ShelfCmd::Release(vec![old.thumb])));
                        }
                    }
                    self.items.insert(0, Item::from_entry(f, ev.source));
                }
                let over = self.items.len().saturating_sub(self.cfg.max_items as usize);
                if over > 0 {
                    let dropped: Vec<Item> = self.items.split_off(self.cfg.max_items as usize);
                    self.release(&dropped, cx);
                }
                self.drag_over = false;
                self.scroll.set_target(0.0);
                cx.request_redraw();
            }
            EventKind::DragHover(on) if self.drag_over != *on => {
                self.drag_over = *on;
                cx.request_redraw();
            }
            _ => {}
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.shelf.clone();
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, _cx: &mut Cx) {
        if v != Visibility::Expanded {
            self.hover = None;
            self.dragging = false;
            self.scroll.snap(0.0);
        }
    }

    fn on_suspend(&mut self, _cx: &mut Cx) {
        self.hover = None;
        self.drag_over = false;
    }

    fn on_input(&mut self, hit_id: Option<HitId>, input: &Input, cx: &mut Cx) -> bool {
        match input {
            Input::Move(_) => {
                if hit_id != self.hover {
                    self.hover = hit_id;
                    cx.request_redraw();
                }
                false
            }
            Input::Leave => {
                if self.hover.take().is_some() {
                    cx.request_redraw();
                }
                false
            }
            Input::Down(_) => {
                self.dragging = false;
                false
            }
            Input::Drag { start, pos } => {
                if self.dragging || (*pos - *start).length() < DRAG_START {
                    return self.dragging;
                }
                // `hit_id` is the region the press began on.
                let paths: Vec<Arc<str>> = match hit_id {
                    Some(HIT_ALL) => self.items.iter().map(|i| i.path.clone()).collect(),
                    Some(h) => match part_of(h) {
                        Some(Part::Tile(i)) => self
                            .items
                            .get(i)
                            .map(|it| vec![it.path.clone()])
                            .unwrap_or_default(),
                        _ => Vec::new(),
                    },
                    None => Vec::new(),
                };
                if paths.is_empty() {
                    return false;
                }
                self.dragging = true;
                cx.command(Command::Shelf(ShelfCmd::DragOut(paths)));
                true
            }
            Input::Up(_) => std::mem::take(&mut self.dragging),
            Input::Wheel { pos, dy, .. } => {
                if self.max_scroll() <= 0.0 || !self.grid.contains(*pos) {
                    return false;
                }
                self.scroll_by(-dy / 120.0);
                cx.request_redraw();
                true
            }
            Input::Click(_) => {
                let Some(h) = hit_id else { return false };
                if h == HIT_CLEAR {
                    self.remove_where(|_| false, cx);
                    cx.request_redraw();
                    return true;
                }
                match part_of(h) {
                    Some(Part::Tile(i)) => {
                        if let Some(it) = self.items.get(i) {
                            cx.command(Command::Shelf(ShelfCmd::Open(it.path.clone())));
                        }
                        true
                    }
                    Some(Part::Remove(i)) => {
                        if let Some(id) = self.items.get(i).map(|it| it.id) {
                            self.remove_where(|it| it.id != id, cx);
                            cx.request_redraw();
                        }
                        true
                    }
                    None => false,
                }
            }
            _ => false,
        }
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let dt = (dx.now - self.last_frame).clamp(0.0, 0.25) as f32;
        self.last_frame = dx.now;
        if !self.scroll.is_settled() {
            self.scroll.step(dt);
        }
        let (header, rest) = area.split_top(HEADER_H);
        cv.text(
            Rect::new(header.x, header.y, 120.0, header.h),
            "Shelf",
            TextStyle::title(),
            th.text,
        );
        let mut right = header.right();
        if !self.items.is_empty() {
            let clear = Rect::new(right - 52.0, header.y + 3.0, 52.0, header.h - 6.0);
            if self.hover == Some(HIT_CLEAR) {
                cv.capsule(clear, th.surface);
            }
            cv.text(
                clear,
                "Clear",
                TextStyle::label().align(Align::Center),
                th.text_dim,
            );
            cv.hit(clear, HIT_CLEAR, CursorKind::Hand);
            right -= 60.0;
            // "Take all": a handle that drags every shelved file at once.
            let all = Rect::new(right - 76.0, header.y + 3.0, 76.0, header.h - 6.0);
            let hot = self.hover == Some(HIT_ALL);
            cv.capsule(all, if hot { th.surface_hi } else { th.surface });
            cv.text(
                all,
                "Drag all",
                TextStyle::label().align(Align::Center),
                th.text_dim,
            );
            cv.hit_drag(all, HIT_ALL);
            right -= 84.0;
            let n = self.items.len();
            let label = if n == 1 {
                "1 file".to_string()
            } else {
                format!("{n} files")
            };
            cv.text(
                Rect::new(right - 70.0, header.y, 70.0, header.h),
                label,
                TextStyle::caption().align(Align::End),
                th.text_faint,
            );
        }

        let grid = Rect::new(rest.x, rest.y + 4.0, rest.w, TILE_H * VISIBLE_ROWS as f32);
        self.grid = grid;

        if self.items.is_empty() {
            let c = grid.centered(grid.w, 70.0);
            let tint = if self.drag_over {
                th.accent
            } else {
                th.text_faint
            };
            cv.icon(
                Icon::Tray,
                Rect::new(c.center().x - 16.0, c.y, 32.0, 32.0),
                tint,
            );
            cv.text(
                Rect::new(c.x, c.y + 38.0, c.w, 20.0),
                if self.drag_over {
                    "Release to add to the shelf"
                } else {
                    "Drop files here, then drag them where you need them"
                },
                TextStyle::body().align(Align::Center),
                if self.drag_over {
                    th.accent
                } else {
                    th.text_dim
                },
            );
        } else {
            let offset = self.scroll.value() * TILE_H;
            cv.push_clip(grid, 0.0);
            for (i, item) in self.items.iter().enumerate() {
                let mut r = Self::tile_rect(grid, i);
                r.y -= offset;
                if r.bottom() < grid.y || r.y > grid.bottom() {
                    continue;
                }
                self.draw_tile(cv, r, i, item, grid);
            }
            cv.pop_clip();
            let max = self.max_scroll();
            if max > 0.0 {
                let track = Rect::new(grid.right() + 3.0, grid.y + 2.0, 3.0, grid.h - 4.0);
                let thumb_h =
                    (track.h * VISIBLE_ROWS as f32 / (VISIBLE_ROWS as f32 + max)).max(18.0);
                let y = track.y + (track.h - thumb_h) * (self.scroll.value() / max).clamp(0.0, 1.0);
                cv.capsule(Rect::new(track.x, y, track.w, thumb_h), th.surface_hi);
            }
        }

        if self.drag_over {
            cv.push(DrawCmd::StrokeRoundRect {
                rect: grid.inflate(6.0, 2.0),
                radius: 14.0,
                width: 2.0,
                color: th.accent,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::DrawList;
    use crate::geom::Vec2;
    use crate::module::{Env, ModuleHost, Out};
    use crate::theme::Theme;

    fn file(id: u64, name: &str, size: u64, thumb: u64) -> FileEntry {
        FileEntry {
            id,
            name: name.into(),
            path: format!("C:\\Users\\me\\{name}").into(),
            size,
            thumb,
            is_dir: false,
        }
    }

    struct T {
        m: Shelf,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            T {
                m: Shelf::new(ShelfCfg::default()),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
                now: 10.0,
            }
        }
        fn send(&mut self, kind: EventKind) {
            let ev = Event::new(Source::Local, kind);
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_event(&ev, &mut cx);
        }
        fn drop_files(&mut self, files: Vec<FileEntry>) {
            self.send(EventKind::FileDropped(files));
        }
        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_input(hit, &i, &mut cx)
        }
        fn draw(&mut self) -> DrawList {
            let mut list = DrawList::new();
            let mut cv = Canvas::new(&mut list, &self.theme);
            self.m.draw_expanded(
                &mut cv,
                Rect::new(0.0, 0.0, 340.0, 192.0),
                &DrawCx {
                    now: self.now,
                    env: &self.env,
                    config: &self.cfg,
                },
            );
            list
        }
        fn commands(&mut self) -> Vec<Command> {
            std::mem::take(&mut self.out.commands)
        }
    }

    fn texts(l: &DrawList) -> Vec<String> {
        l.cmds
            .iter()
            .filter_map(|c| {
                if let DrawCmd::Text { text, .. } = c {
                    Some(text.as_str().to_string())
                } else {
                    None
                }
            })
            .collect()
    }

    #[test]
    fn sizes_are_human() {
        assert_eq!(fmt_size(0), "0 B");
        assert_eq!(fmt_size(1023), "1023 B");
        assert_eq!(fmt_size(1024), "1.0 KB");
        assert_eq!(fmt_size(1_234_567), "1.2 MB");
        assert_eq!(fmt_size(5 * 1024 * 1024 * 1024), "5.0 GB");
        assert_eq!(
            fmt_size(u64::MAX),
            "16777216.0 TB",
            "never panics or overflows the unit table"
        );
    }

    #[test]
    fn hit_ids_round_trip() {
        for i in [0usize, 4, 39, 199] {
            for p in [Part::Tile(i), Part::Remove(i)] {
                assert_eq!(part_of(hit(p)), Some(p));
            }
        }
        assert_eq!(part_of(HIT_CLEAR), None);
        assert_eq!(part_of(HIT_ALL), None);
    }

    #[test]
    fn dropped_files_land_newest_first_and_a_repeat_moves_to_the_front() {
        let mut t = T::new();
        t.drop_files(vec![file(1, "a.txt", 10, 0), file(2, "b.txt", 20, 0)]);
        assert_eq!(
            t.m.items().iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![1, 2],
            "a batch keeps its own order"
        );
        t.drop_files(vec![file(3, "c.txt", 30, 0)]);
        assert_eq!(t.m.items()[0].id, 3);
        t.drop_files(vec![file(4, "a.txt", 10, 0)]); // same path as id 1
        assert_eq!(t.m.items().len(), 3, "no duplicate for the same path");
        assert_eq!(t.m.items()[0].id, 4);
    }

    #[test]
    fn the_shelf_is_capped_and_releases_what_falls_off() {
        let mut t = T::new();
        t.m = Shelf::new(ShelfCfg {
            max_items: 3,
            ..Default::default()
        });
        for i in 1..=5 {
            t.drop_files(vec![file(i, &format!("f{i}.txt"), 1, 100 + i)]);
        }
        assert_eq!(t.m.items().len(), 3);
        assert_eq!(
            t.m.items().iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![5, 4, 3]
        );
        let released: Vec<u64> = t
            .commands()
            .into_iter()
            .flat_map(|c| match c {
                Command::Shelf(ShelfCmd::Release(v)) => v,
                _ => vec![],
            })
            .collect();
        assert_eq!(
            released,
            vec![101, 102],
            "thumbnails of the evicted items are freed"
        );
    }

    #[test]
    fn empty_page_invites_a_drop_and_lights_up_during_a_drag() {
        let mut t = T::new();
        let idle = texts(&t.draw());
        assert!(
            idle.iter().any(|s| s.contains("Drop files here")),
            "{idle:?}"
        );
        t.send(EventKind::DragHover(true));
        let l = t.draw();
        assert!(texts(&l).iter().any(|s| s.contains("Release to add")));
        assert!(
            l.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::StrokeRoundRect { .. })),
            "an accent outline"
        );
        t.send(EventKind::DragHover(false));
        assert!(
            !t.draw()
                .cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::StrokeRoundRect { .. }))
        );
        assert!(l.is_balanced());
    }

    #[test]
    fn dropping_clears_the_drag_highlight() {
        let mut t = T::new();
        t.send(EventKind::DragHover(true));
        t.drop_files(vec![file(1, "x.png", 5, 0)]);
        assert!(
            !t.draw()
                .cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::StrokeRoundRect { .. }))
        );
    }

    #[test]
    fn tiles_show_name_size_and_a_thumbnail_or_icon() {
        let mut t = T::new();
        let mut folder = file(2, "Projects", 0, 0);
        folder.is_dir = true;
        t.drop_files(vec![file(1, "photo.jpg", 2_500_000, 77), folder]);
        let l = t.draw();
        let tx = texts(&l);
        assert!(
            tx.contains(&"photo.jpg".to_string()) && tx.contains(&"2.4 MB".to_string()),
            "{tx:?}"
        );
        assert!(tx.contains(&"Folder".to_string()));
        assert!(
            l.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Image { id, .. } if id.0 == 77))
        );
        assert!(l.cmds.iter().any(|c| matches!(
            c,
            DrawCmd::Icon {
                icon: Icon::Folder,
                ..
            }
        )));
        assert!(
            l.hits
                .iter()
                .any(|h| h.id == hit(Part::Tile(0)) && h.draggable),
            "tiles are drag sources"
        );
    }

    #[test]
    fn clicking_a_tile_opens_the_file() {
        let mut t = T::new();
        t.drop_files(vec![file(1, "doc.pdf", 9, 0)]);
        t.draw();
        assert!(t.input(Some(hit(Part::Tile(0))), Input::Click(Vec2::ZERO)));
        assert_eq!(
            t.commands(),
            vec![Command::Shelf(ShelfCmd::Open(
                "C:\\Users\\me\\doc.pdf".into()
            ))]
        );
    }

    #[test]
    fn dragging_a_tile_out_starts_after_a_small_threshold_and_only_once() {
        let mut t = T::new();
        t.drop_files(vec![file(1, "a.txt", 1, 0), file(2, "b.txt", 1, 0)]);
        t.draw();
        let tile = Some(hit(Part::Tile(1)));
        let start = Vec2::new(100.0, 100.0);
        assert!(
            !t.input(
                tile,
                Input::Drag {
                    start,
                    pos: Vec2::new(103.0, 100.0)
                }
            ),
            "below the threshold: still a click"
        );
        assert!(t.commands().is_empty());
        assert!(t.input(
            tile,
            Input::Drag {
                start,
                pos: Vec2::new(120.0, 100.0)
            }
        ));
        assert_eq!(
            t.commands(),
            vec![Command::Shelf(ShelfCmd::DragOut(vec![
                "C:\\Users\\me\\b.txt".into()
            ]))],
            "tile 1 is the second item of the batch"
        );
        assert!(
            t.input(
                tile,
                Input::Drag {
                    start,
                    pos: Vec2::new(200.0, 100.0)
                }
            ),
            "further movement is swallowed"
        );
        assert!(t.commands().is_empty(), "no second drag");
        assert!(
            t.input(None, Input::Up(Vec2::ZERO)),
            "the release that ends the drag is consumed (no click)"
        );
        assert!(!t.input(None, Input::Up(Vec2::ZERO)));
    }

    #[test]
    fn drag_all_takes_every_file() {
        let mut t = T::new();
        t.drop_files(vec![
            file(1, "a.txt", 1, 0),
            file(2, "b.txt", 1, 0),
            file(3, "c.txt", 1, 0),
        ]);
        t.draw();
        t.input(
            Some(HIT_ALL),
            Input::Drag {
                start: Vec2::ZERO,
                pos: Vec2::new(30.0, 0.0),
            },
        );
        let cmds = t.commands();
        match &cmds[..] {
            [Command::Shelf(ShelfCmd::DragOut(paths))] => assert_eq!(paths.len(), 3),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_drag_that_starts_on_nothing_does_nothing() {
        let mut t = T::new();
        t.drop_files(vec![file(1, "a.txt", 1, 0)]);
        t.draw();
        assert!(!t.input(
            None,
            Input::Drag {
                start: Vec2::ZERO,
                pos: Vec2::new(50.0, 0.0)
            }
        ));
        assert!(!t.input(
            Some(HIT_CLEAR),
            Input::Drag {
                start: Vec2::ZERO,
                pos: Vec2::new(50.0, 0.0)
            }
        ));
        assert!(t.commands().is_empty());
    }

    #[test]
    fn removing_forgets_only_the_reference_and_frees_the_thumbnail() {
        let mut t = T::new();
        t.drop_files(vec![file(1, "a.txt", 1, 11), file(2, "b.txt", 1, 22)]);
        t.draw();
        assert!(t.input(Some(hit(Part::Remove(0))), Input::Click(Vec2::ZERO)));
        assert_eq!(
            t.m.items().iter().map(|i| i.id).collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(
            t.commands(),
            vec![Command::Shelf(ShelfCmd::Release(vec![11]))]
        );
        assert!(t.input(Some(HIT_CLEAR), Input::Click(Vec2::ZERO)));
        assert!(t.m.items().is_empty());
        assert_eq!(
            t.commands(),
            vec![Command::Shelf(ShelfCmd::Release(vec![22]))]
        );
    }

    #[test]
    fn the_remove_button_wins_over_the_tile_it_sits_on() {
        let mut t = T::new();
        t.drop_files(vec![file(1, "a.txt", 1, 0)]);
        t.input(Some(hit(Part::Tile(0))), Input::Move(Vec2::ZERO));
        let l = t.draw();
        let x = l
            .hits
            .iter()
            .find(|h| h.id == hit(Part::Remove(0)))
            .expect("shown on hover");
        assert_eq!(
            l.hit_test(x.rect.center()).unwrap().id,
            hit(Part::Remove(0))
        );
    }

    #[test]
    fn many_files_scroll_by_rows_and_stop_asking_for_frames() {
        let mut t = T::new();
        let files: Vec<FileEntry> = (1..=17)
            .map(|i| file(i, &format!("f{i}.txt"), 1, 0))
            .collect();
        t.drop_files(files);
        t.draw();
        assert_eq!(t.m.rows(), 4);
        assert_eq!(t.m.max_scroll(), 2.0);
        let inside = t.m.grid.center();
        assert!(t.input(
            None,
            Input::Wheel {
                pos: inside,
                dx: 0.0,
                dy: -120.0
            }
        ));
        assert_eq!(t.m.scroll.target(), 1.0);
        assert!(t.m.wants_frames());
        for _ in 0..200 {
            t.now += 1.0 / 60.0;
            t.draw();
        }
        assert!(!t.m.wants_frames());
        for _ in 0..20 {
            t.input(
                None,
                Input::Wheel {
                    pos: inside,
                    dx: 0.0,
                    dy: -120.0,
                },
            );
        }
        assert_eq!(t.m.scroll.target(), 2.0, "clamped to the last row");
    }

    #[test]
    fn only_visible_tiles_are_interactive() {
        let mut t = T::new();
        let files: Vec<FileEntry> = (1..=20)
            .map(|i| file(i, &format!("f{i}.txt"), 1, 0))
            .collect();
        t.drop_files(files);
        let l = t.draw();
        let tiles = l
            .hits
            .iter()
            .filter(|h| matches!(part_of(h.id), Some(Part::Tile(_))))
            .count();
        assert!(tiles <= COLS * (VISIBLE_ROWS + 1), "{tiles}");
        assert!(tiles >= COLS * VISIBLE_ROWS);
    }

    #[test]
    fn works_inside_the_host_and_is_toggleable() {
        let mut host = ModuleHost::new(
            crate::modules::registry(),
            Arc::new(Config::default()),
            Theme::default(),
        );
        assert!(host.page_ids().contains(&"shelf"), "{:?}", host.page_ids());
        host.dispatch(vec![
            Event::new(Source::Local, EventKind::DragHover(true)),
            Event::new(
                Source::Phone,
                EventKind::FileDropped(vec![file(1, "from-phone.heic", 3, 0)]),
            ),
        ]);
        assert!(
            host.take_out().shell.is_empty(),
            "dropping never interrupts"
        );
        let mut cfg = Config::default();
        cfg.shelf.enabled = false;
        assert!(!cfg.module_active("shelf"));
        host.apply_config(Arc::new(cfg));
        assert!(!host.page_ids().contains(&"shelf"));
    }

    #[test]
    fn phone_files_are_tagged_the_same_way_as_local_ones() {
        let mut t = T::new();
        let ev = Event::new(
            Source::Phone,
            EventKind::FileDropped(vec![file(9, "IMG_0001.heic", 2_000_000, 0)]),
        );
        let mut cx = Cx::for_test(t.now, &t.env, &t.theme, &t.cfg, &mut t.out);
        t.m.on_event(&ev, &mut cx);
        assert_eq!(t.m.items()[0].source, Source::Phone);
    }
}
