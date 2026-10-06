//! The clipboard page: recent text, links and images. Click an entry to put it back on the clipboard,
//! pin it to keep it, delete it, open a link, scroll for older ones.
//!
//! The page is a *view*: the Windows clipboard service owns the content (see `clipstore`) and tells
//! this module what exists through `ClipboardItem` / `ClipboardRemoved` events — the same events
//! whether the item came from this PC or the iPhone, so both render identically (a small source tag
//! is the only difference). Nothing polls: scrolling animates only while the scroll spring moves.

use std::sync::Arc;
use std::time::Duration;

use crate::config::{ClipboardCfg, Config};
use crate::draw::{Align, Canvas, CursorKind, HitId, ImageId, TextStyle};
use crate::events::{ClipKind, ClipboardItem, Event, EventKind, EventMask, Kind, Source};
use crate::geom::{Rect, Size};
use crate::icons::Icon;
use crate::input::Input;
use crate::module::{ClipCmd, Command, Cx, DrawCx, Module, ModuleId, Visibility};
use crate::spring::{Spring, SpringParams};

pub const ROW_H: f32 = 40.0;
pub const HEADER_H: f32 = 28.0;
pub const VISIBLE_ROWS: usize = 4;
const BTN: f32 = 22.0;
const FLASH_SECS: f64 = 1.1;

const HIT_CLEAR: HitId = HitId(1);

/// What a hit region id means.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Part {
    Row(usize),
    Pin(usize),
    Delete(usize),
    Open(usize),
}

fn hit(part: Part) -> HitId {
    HitId(match part {
        Part::Row(i) => 100 + i as u32,
        Part::Pin(i) => 1_000 + i as u32,
        Part::Delete(i) => 2_000 + i as u32,
        Part::Open(i) => 3_000 + i as u32,
    })
}

fn part_of(h: HitId) -> Option<Part> {
    let (range, i) = (h.0 / 1_000, (h.0 % 1_000) as usize);
    match (range, h.0) {
        (0, 100..=999) => Some(Part::Row(h.0 as usize - 100)),
        (1, _) => Some(Part::Pin(i)),
        (2, _) => Some(Part::Delete(i)),
        (3, _) => Some(Part::Open(i)),
        _ => None,
    }
}

pub fn create(cfg: &Config) -> Option<Box<dyn Module>> {
    cfg.clipboard
        .enabled
        .then(|| Box::new(Clipboard::new(cfg.clipboard.clone())) as Box<dyn Module>)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    pub id: u64,
    pub kind: ClipKind,
    pub preview: Arc<str>,
    pub thumb: u64,
    pub pinned: bool,
    pub source: Source,
    /// Monotonic seconds when it arrived (for the "2 min ago" label).
    pub at: f64,
}

/// "now", "12 s", "5 min", "3 h", "2 d".
pub fn fmt_age(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=4 => "now".into(),
        5..=59 => format!("{s} s"),
        60..=3599 => format!("{} min", s / 60),
        3600..=86_399 => format!("{} h", s / 3600),
        _ => format!("{} d", s / 86_400),
    }
}

fn kind_label(k: ClipKind) -> &'static str {
    match k {
        ClipKind::Text => "Text",
        ClipKind::Link => "Link",
        ClipKind::Image => "Image",
        ClipKind::Files => "Files",
    }
}

fn kind_icon(k: ClipKind) -> Icon {
    match k {
        ClipKind::Text => Icon::Doc,
        ClipKind::Link => Icon::Globe,
        ClipKind::Image => Icon::Image,
        ClipKind::Files => Icon::Doc,
    }
}

pub struct Clipboard {
    cfg: ClipboardCfg,
    /// Store order: newest first.
    rows: Vec<Row>,
    /// Row ids in the order they were drawn (index = the number encoded in hit ids).
    drawn: Vec<u64>,
    hover: Option<HitId>,
    /// `(row id, until)`: the entry that was just copied.
    flash: Option<(u64, f64)>,
    scroll: Spring,
    last_frame: f64,
    list: Rect,
}

impl Clipboard {
    pub fn new(cfg: ClipboardCfg) -> Clipboard {
        Clipboard {
            cfg,
            rows: Vec::new(),
            drawn: Vec::new(),
            hover: None,
            flash: None,
            scroll: Spring::new(0.0, SpringParams::new(24.0, 1.0)),
            last_frame: 0.0,
            list: Rect::default(),
        }
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    /// Row indices in display order: pinned first, each group newest first.
    fn view(&self) -> Vec<usize> {
        let pinned = (0..self.rows.len()).filter(|&i| self.rows[i].pinned);
        let rest = (0..self.rows.len()).filter(|&i| !self.rows[i].pinned);
        pinned.chain(rest).collect()
    }

    fn max_scroll(&self) -> f32 {
        self.rows.len().saturating_sub(VISIBLE_ROWS) as f32
    }

    fn scroll_by(&mut self, rows: f32) {
        let t = (self.scroll.target() + rows).clamp(0.0, self.max_scroll());
        self.scroll.set_target(t);
    }

    fn row_by_view_index(&self, i: usize) -> Option<&Row> {
        let id = *self.drawn.get(i)?;
        self.rows.iter().find(|r| r.id == id)
    }

    fn apply_item(&mut self, it: &ClipboardItem, source: Source, now: f64) {
        self.rows.retain(|r| r.id != it.id);
        self.rows.insert(
            0,
            Row {
                id: it.id,
                kind: it.kind,
                preview: it.preview.clone(),
                thumb: it.thumb,
                pinned: it.pinned,
                source,
                at: now,
            },
        );
        // A fresh copy goes to the top of its group: bring it into view.
        self.scroll.set_target(0.0);
    }

    fn draw_row(&self, cv: &mut Canvas, r: Rect, row: &Row, vi: usize, now: f64, visible: Rect) {
        let th = *cv.theme;
        let hovered = matches!(self.hover.and_then(part_of), Some(Part::Row(i) | Part::Pin(i) | Part::Delete(i) | Part::Open(i)) if i == vi);
        let flashing = self
            .flash
            .is_some_and(|(id, until)| id == row.id && now < until);
        if flashing {
            cv.round_rect(r, 10.0, th.accent.with_alpha(0.28));
        } else if hovered {
            cv.round_rect(r, 10.0, th.surface);
        }
        // Thumbnail or kind tile.
        let tile = Rect::new(r.x + 6.0, r.y + (r.h - 28.0) * 0.5, 28.0, 28.0);
        if row.thumb != 0 {
            cv.image(ImageId(row.thumb), tile, 7.0);
        } else {
            cv.squircle(tile, 7.0, th.surface_hi);
            cv.icon(kind_icon(row.kind), tile.inset(6.0), th.text_dim);
        }

        // Buttons on the right: shown on hover (the pin also stays visible while pinned).
        let show_buttons = hovered && !flashing;
        let mut right = r.right() - 6.0;
        let mut button_w = 0.0;
        if flashing {
            let b = Rect::new(right - BTN, r.y + (r.h - BTN) * 0.5, BTN, BTN);
            cv.icon(Icon::Check, b.inset(3.0), th.accent);
            button_w = BTN + 6.0;
        } else {
            let mut place =
                |cv: &mut Canvas, icon: Icon, part: Part, color: crate::color::Color| {
                    let b = Rect::new(right - BTN, r.y + (r.h - BTN) * 0.5, BTN, BTN);
                    if self.hover == Some(hit(part)) {
                        cv.capsule(b, th.surface_hi);
                    }
                    cv.icon(icon, b.inset(4.5), color);
                    // Only the part inside the list is clickable (a row can be half scrolled away).
                    if let Some(h) = b.intersect(&visible) {
                        cv.hit(h, hit(part), CursorKind::Hand);
                    }
                    right -= BTN + 2.0;
                    button_w += BTN + 2.0;
                };
            if show_buttons {
                place(cv, Icon::Trash, Part::Delete(vi), th.text_dim);
                place(
                    cv,
                    Icon::Pin,
                    Part::Pin(vi),
                    if row.pinned { th.accent } else { th.text_dim },
                );
                if row.kind == ClipKind::Link {
                    place(cv, Icon::Open, Part::Open(vi), th.text_dim);
                }
            } else if row.pinned {
                // A quiet marker, not a button (hovering the row reveals the real one).
                let b = Rect::new(right - BTN, r.y + (r.h - BTN) * 0.5, BTN, BTN);
                cv.icon(Icon::Pin, b.inset(5.0), th.accent);
                button_w = BTN + 2.0;
            }
        }

        let x = tile.right() + 10.0;
        let w = (r.right() - x - button_w - 8.0).max(0.0);
        cv.text(
            Rect::new(x, r.y + 3.0, w, 19.0),
            &row.preview,
            TextStyle::body(),
            th.text,
        );
        let mut caption = format!("{} · {}", kind_label(row.kind), fmt_age(now - row.at));
        if row.source == Source::Phone {
            caption.push_str(" · iPhone");
        }
        cv.text(
            Rect::new(x, r.y + 21.0, w, 14.0),
            caption,
            TextStyle::caption(),
            th.text_faint,
        );
    }
}

impl Module for Clipboard {
    fn id(&self) -> ModuleId {
        "clipboard"
    }

    fn title(&self) -> &'static str {
        "Clipboard"
    }

    fn icon(&self) -> Icon {
        Icon::Doc
    }

    fn subscriptions(&self) -> EventMask {
        EventMask::of(&[Kind::ClipboardItem, Kind::ClipboardRemoved])
    }

    /// Keeps the "5 min ago" labels fresh while the page is open. (Honoured only while expanded.)
    fn poll_interval(&self) -> Option<Duration> {
        Some(Duration::from_secs(20))
    }

    /// Frames only while the scroll is still moving.
    fn wants_frames(&self) -> bool {
        !self.scroll.is_settled()
    }

    fn expanded_size(&self) -> Size {
        // 28 header + 4 gap + 4 rows, plus the shell's padding (14 top, 26 bottom) and ears.
        Size::new(
            424.0,
            14.0 + HEADER_H + 4.0 + ROW_H * VISIBLE_ROWS as f32 + 26.0,
        )
    }

    fn peek_size(&self) -> Option<Size> {
        Some(Size::new(354.0, 58.0))
    }

    fn on_event(&mut self, ev: &Event, cx: &mut Cx) {
        match &ev.kind {
            EventKind::ClipboardItem(it) => {
                self.apply_item(it, ev.source, cx.now);
                cx.request_redraw();
                if ev.source == Source::Phone && self.cfg.peek_phone_items {
                    cx.peek(2.6);
                }
            }
            EventKind::ClipboardRemoved(id) => {
                let before = self.rows.len();
                self.rows.retain(|r| r.id != *id);
                if self.rows.len() != before {
                    let t = self.scroll.target().min(self.max_scroll());
                    self.scroll.set_target(t);
                    cx.request_redraw();
                }
            }
            _ => {}
        }
    }

    fn on_config(&mut self, cfg: &Config, cx: &mut Cx) {
        self.cfg = cfg.clipboard.clone();
        cx.request_redraw();
    }

    fn on_visibility(&mut self, v: Visibility, _cx: &mut Cx) {
        if v != Visibility::Expanded {
            self.hover = None;
            self.flash = None;
            self.scroll.snap(0.0);
        }
    }

    fn on_suspend(&mut self, _cx: &mut Cx) {
        self.hover = None;
    }

    fn on_poll(&mut self, cx: &mut Cx) {
        if !self.rows.is_empty() {
            cx.request_redraw();
        }
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
            Input::Wheel { pos, dy, .. } => {
                if self.max_scroll() <= 0.0 || !self.list.contains(*pos) {
                    return false;
                }
                // 120 = one notch = one row. Natural direction: wheel up shows newer entries.
                self.scroll_by(-dy / 120.0);
                cx.request_redraw();
                true
            }
            Input::Click(_) => {
                let Some(h) = hit_id else { return false };
                if h == HIT_CLEAR {
                    self.rows.retain(|r| r.pinned);
                    self.scroll.snap(0.0);
                    cx.command(Command::Clipboard(ClipCmd::Clear));
                    cx.request_redraw();
                    return true;
                }
                let Some(part) = part_of(h) else { return false };
                let (Part::Row(i) | Part::Pin(i) | Part::Delete(i) | Part::Open(i)) = part;
                let Some(row) = self.row_by_view_index(i).cloned() else {
                    return false;
                };
                match part {
                    Part::Row(_) => {
                        cx.command(Command::Clipboard(ClipCmd::Copy(row.id)));
                        self.flash = Some((row.id, cx.now + FLASH_SECS));
                        cx.redraw_at(cx.now + FLASH_SECS);
                    }
                    Part::Pin(_) => {
                        let pinned = !row.pinned;
                        cx.command(Command::Clipboard(ClipCmd::Pin(row.id, pinned)));
                        if let Some(r) = self.rows.iter_mut().find(|r| r.id == row.id) {
                            r.pinned = pinned;
                        }
                    }
                    Part::Delete(_) => {
                        cx.command(Command::Clipboard(ClipCmd::Remove(row.id)));
                        self.rows.retain(|r| r.id != row.id);
                        let t = self.scroll.target().min(self.max_scroll());
                        self.scroll.set_target(t);
                    }
                    Part::Open(_) => cx.command(Command::Clipboard(ClipCmd::Open(row.id))),
                }
                cx.request_redraw();
                true
            }
            _ => false,
        }
    }

    fn draw_peek(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let Some(row) = self.rows.first() else { return };
        let tile = Rect::new(area.x, area.y, area.h, area.h);
        if row.thumb != 0 {
            cv.image(ImageId(row.thumb), tile, 8.0);
        } else {
            cv.squircle(tile, 8.0, th.surface_hi);
            cv.icon(kind_icon(row.kind), tile.inset(tile.w * 0.22), th.text_dim);
        }
        let x = tile.right() + 12.0;
        let w = (area.right() - x).max(0.0);
        let from = if row.source == Source::Phone {
            "Copied on iPhone"
        } else {
            "Copied"
        };
        cv.text(
            Rect::new(x, area.y, w, area.h * 0.45),
            from,
            TextStyle::caption(),
            th.accent,
        );
        cv.text(
            Rect::new(x, area.y + area.h * 0.42, w, area.h * 0.58),
            &row.preview,
            TextStyle::body(),
            th.text,
        );
        let _ = dx;
    }

    fn draw_expanded(&mut self, cv: &mut Canvas, area: Rect, dx: &DrawCx) {
        let th = *cv.theme;
        let now = dx.now;
        // Advance the scroll spring by the real time since the last frame (exact for any dt).
        let dt = (now - self.last_frame).clamp(0.0, 0.25) as f32;
        self.last_frame = now;
        if !self.scroll.is_settled() {
            self.scroll.step(dt);
        }
        let (header, rest) = area.split_top(HEADER_H);
        cv.text(
            Rect::new(header.x, header.y, 160.0, header.h),
            "Clipboard",
            TextStyle::title(),
            th.text,
        );
        let unpinned = self.rows.iter().filter(|r| !r.pinned).count();
        let mut right = header.right();
        if unpinned > 0 {
            let b = Rect::new(right - 52.0, header.y + 3.0, 52.0, header.h - 6.0);
            if self.hover == Some(HIT_CLEAR) {
                cv.capsule(b, th.surface);
            }
            cv.text(
                b,
                "Clear",
                TextStyle::label().align(Align::Center),
                th.text_dim,
            );
            cv.hit(b, HIT_CLEAR, CursorKind::Hand);
            right -= 60.0;
        }
        if !self.rows.is_empty() {
            let n = self.rows.len();
            let label = if n == 1 {
                "1 item".to_string()
            } else {
                format!("{n} items")
            };
            cv.text(
                Rect::new(right - 90.0, header.y, 90.0, header.h),
                label,
                TextStyle::caption().align(Align::End),
                th.text_faint,
            );
        }

        let list = Rect::new(rest.x, rest.y + 4.0, rest.w, ROW_H * VISIBLE_ROWS as f32);
        self.list = list;
        self.drawn.clear();
        if self.rows.is_empty() {
            let c = list.centered(list.w, 64.0);
            cv.icon(
                Icon::Doc,
                Rect::new(c.center().x - 14.0, c.y, 28.0, 28.0),
                th.text_faint,
            );
            cv.text(
                Rect::new(c.x, c.y + 32.0, c.w, 20.0),
                "Copied text, links and images appear here",
                TextStyle::body().align(Align::Center),
                th.text_dim,
            );
            return;
        }
        let view = self.view();
        let offset = self.scroll.value() * ROW_H;
        cv.push_clip(list, 0.0);
        for (vi, &ri) in view.iter().enumerate() {
            self.drawn.push(self.rows[ri].id);
            let y = list.y + vi as f32 * ROW_H - offset;
            if y + ROW_H < list.y || y > list.bottom() {
                continue;
            }
            let r = Rect::new(list.x, y, list.w - 8.0, ROW_H - 4.0);
            // Hit regions must not extend outside the visible list.
            let before = cv.list.hits.len();
            self.draw_row(cv, r, &self.rows[ri].clone(), vi, now, list);
            let row_hit = r.intersect(&list);
            if let Some(rh) = row_hit {
                cv.hit(rh, hit(Part::Row(vi)), CursorKind::Hand);
            }
            // Buttons were registered before the row's own region, so they win hit tests (topmost =
            // last registered): move the row region underneath them.
            let n = cv.list.hits.len();
            if n - before >= 2 {
                let row_region = cv.list.hits.pop().unwrap();
                cv.list.hits.insert(before, row_region);
            }
        }
        cv.pop_clip();
        // Scroll indicator.
        let max = self.max_scroll();
        if max > 0.0 {
            let track = Rect::new(list.right() - 4.0, list.y + 2.0, 3.0, list.h - 4.0);
            let thumb_h = (track.h * VISIBLE_ROWS as f32 / (VISIBLE_ROWS as f32 + max)).max(18.0);
            let y = track.y + (track.h - thumb_h) * (self.scroll.value() / max).clamp(0.0, 1.0);
            cv.capsule(Rect::new(track.x, y, track.w, thumb_h), th.surface_hi);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::{DrawCmd, DrawList};
    use crate::geom::Vec2;
    use crate::module::{Env, ModuleHost, Out};
    use crate::theme::Theme;

    fn item(id: u64, text: &str, kind: ClipKind) -> ClipboardItem {
        ClipboardItem {
            id,
            kind,
            preview: text.into(),
            thumb: 0,
            pinned: false,
        }
    }

    struct T {
        m: Clipboard,
        theme: Theme,
        cfg: Config,
        env: Env,
        out: Out,
        now: f64,
    }

    impl T {
        fn new() -> T {
            T {
                m: Clipboard::new(ClipboardCfg::default()),
                theme: Theme::default(),
                cfg: Config::default(),
                env: Env::default(),
                out: Out::default(),
                now: 100.0,
            }
        }
        fn send(&mut self, source: Source, kind: EventKind) {
            let ev = Event::new(source, kind);
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_event(&ev, &mut cx);
        }
        fn add(&mut self, id: u64, text: &str, kind: ClipKind) {
            self.send(
                Source::Local,
                EventKind::ClipboardItem(item(id, text, kind)),
            );
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
        fn input(&mut self, hit: Option<HitId>, i: Input) -> bool {
            let mut cx = Cx::for_test(self.now, &self.env, &self.theme, &self.cfg, &mut self.out);
            self.m.on_input(hit, &i, &mut cx)
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

    fn hit_for(l: &DrawList, h: HitId) -> Rect {
        l.hits
            .iter()
            .find(|x| x.id == h)
            .unwrap_or_else(|| panic!("no hit region {h:?}"))
            .rect
    }

    #[test]
    fn hit_ids_round_trip() {
        for i in [0usize, 1, 7, 99] {
            for p in [Part::Row(i), Part::Pin(i), Part::Delete(i), Part::Open(i)] {
                assert_eq!(part_of(hit(p)), Some(p));
            }
        }
        assert_eq!(part_of(HIT_CLEAR), None);
        assert_eq!(part_of(HitId(5)), None);
    }

    #[test]
    fn age_labels() {
        assert_eq!(fmt_age(0.0), "now");
        assert_eq!(fmt_age(4.9), "now");
        assert_eq!(fmt_age(5.0), "5 s");
        assert_eq!(fmt_age(59.0), "59 s");
        assert_eq!(fmt_age(60.0), "1 min");
        assert_eq!(fmt_age(3599.0), "59 min");
        assert_eq!(fmt_age(3600.0), "1 h");
        assert_eq!(fmt_age(90_000.0), "1 d");
        assert_eq!(fmt_age(-5.0), "now");
    }

    #[test]
    fn items_arrive_newest_first_and_a_bump_moves_the_row() {
        let mut t = T::new();
        t.add(1, "one", ClipKind::Text);
        t.add(2, "two", ClipKind::Link);
        assert_eq!(
            t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![2, 1]
        );
        t.add(1, "one", ClipKind::Text); // copied again: same id
        assert_eq!(
            t.m.rows().iter().map(|r| r.id).collect::<Vec<_>>(),
            vec![1, 2],
            "moved, not duplicated"
        );
        t.send(Source::Local, EventKind::ClipboardRemoved(2));
        assert_eq!(t.m.rows().len(), 1);
        t.send(Source::Local, EventKind::ClipboardRemoved(99));
        assert_eq!(t.m.rows().len(), 1, "unknown ids are ignored");
    }

    #[test]
    fn pinned_rows_are_shown_first() {
        let mut t = T::new();
        t.add(1, "old pinned", ClipKind::Text);
        t.add(2, "newer", ClipKind::Text);
        t.add(3, "newest", ClipKind::Text);
        t.m.rows.iter_mut().find(|r| r.id == 1).unwrap().pinned = true;
        let l = t.draw();
        let tx = texts(&l);
        let pos = |s: &str| tx.iter().position(|x| x == s).unwrap();
        assert!(
            pos("old pinned") < pos("newest") && pos("newest") < pos("newer"),
            "{tx:?}"
        );
    }

    #[test]
    fn phone_items_look_the_same_but_carry_a_tag_and_may_peek() {
        let mut t = T::new();
        t.send(
            Source::Phone,
            EventKind::ClipboardItem(item(5, "from the phone", ClipKind::Text)),
        );
        assert_eq!(t.m.rows()[0].source, Source::Phone);
        assert_eq!(t.out.shell.len(), 1, "an item from the phone peeks");
        t.out.shell.clear();
        t.add(6, "from the pc", ClipKind::Text);
        assert!(
            t.out.shell.is_empty(),
            "a local copy does not interrupt anyone"
        );
        let tx = texts(&t.draw());
        assert!(tx.iter().any(|s| s.contains("iPhone")), "{tx:?}");
        let mut quiet = T::new();
        quiet.m = Clipboard::new(ClipboardCfg {
            peek_phone_items: false,
            ..Default::default()
        });
        quiet.send(
            Source::Phone,
            EventKind::ClipboardItem(item(1, "x", ClipKind::Text)),
        );
        assert!(quiet.out.shell.is_empty());
    }

    #[test]
    fn the_empty_page_explains_itself() {
        let mut t = T::new();
        let l = t.draw();
        assert!(texts(&l).iter().any(|s| s.contains("appear here")));
        assert!(l.is_balanced());
        assert!(!t.m.wants_frames());
    }

    #[test]
    fn clicking_a_row_copies_it_and_flashes_a_check() {
        let mut t = T::new();
        t.add(7, "hello", ClipKind::Text);
        let l = t.draw();
        let row = hit_for(&l, hit(Part::Row(0)));
        assert!(t.input(Some(hit(Part::Row(0))), Input::Click(row.center())));
        assert_eq!(t.commands(), vec![Command::Clipboard(ClipCmd::Copy(7))]);
        assert_eq!(
            t.out.redraw_at,
            Some(t.now + FLASH_SECS),
            "a redraw is scheduled to clear the flash"
        );
        let l2 = t.draw();
        assert!(
            l2.cmds.iter().any(|c| matches!(
                c,
                DrawCmd::Icon {
                    icon: Icon::Check,
                    ..
                }
            )),
            "shows the confirmation"
        );
        t.now += FLASH_SECS + 0.1;
        let l3 = t.draw();
        assert!(
            !l3.cmds.iter().any(|c| matches!(
                c,
                DrawCmd::Icon {
                    icon: Icon::Check,
                    ..
                }
            )),
            "and it goes away"
        );
    }

    #[test]
    fn pin_delete_open_and_clear_send_the_right_commands() {
        let mut t = T::new();
        t.add(1, "a link", ClipKind::Link);
        t.add(2, "some text", ClipKind::Text);
        t.draw();
        // View order: newest first -> index 0 is id 2 (text), index 1 is id 1 (link).
        assert!(t.input(Some(hit(Part::Pin(0))), Input::Click(Vec2::ZERO)));
        assert!(t.input(Some(hit(Part::Open(1))), Input::Click(Vec2::ZERO)));
        assert!(t.input(Some(hit(Part::Delete(1))), Input::Click(Vec2::ZERO)));
        assert_eq!(
            t.commands(),
            vec![
                Command::Clipboard(ClipCmd::Pin(2, true)),
                Command::Clipboard(ClipCmd::Open(1)),
                Command::Clipboard(ClipCmd::Remove(1)),
            ]
        );
        assert!(
            t.m.rows().iter().find(|r| r.id == 2).unwrap().pinned,
            "optimistic"
        );
        assert!(t.m.rows().iter().all(|r| r.id != 1), "optimistic removal");
        t.add(3, "another", ClipKind::Text);
        assert!(t.input(Some(HIT_CLEAR), Input::Click(Vec2::ZERO)));
        assert_eq!(t.commands(), vec![Command::Clipboard(ClipCmd::Clear)]);
        assert_eq!(t.m.rows().len(), 1, "only the pinned row survives a clear");
    }

    #[test]
    fn buttons_appear_on_hover_and_win_hit_tests_over_the_row() {
        let mut t = T::new();
        t.add(1, "https://example.com", ClipKind::Link);
        let quiet = t.draw();
        assert!(
            quiet
                .hits
                .iter()
                .all(|h| part_of(h.id).is_none_or(|p| matches!(p, Part::Row(_)))),
            "no buttons until hovered"
        );
        t.input(Some(hit(Part::Row(0))), Input::Move(Vec2::ZERO));
        let hot = t.draw();
        let pin = hit_for(&hot, hit(Part::Pin(0)));
        let hit_here = hot.hit_test(pin.center()).unwrap();
        assert_eq!(
            hit_here.id,
            hit(Part::Pin(0)),
            "the button is on top of the row region"
        );
        assert!(
            hot.hits.iter().any(|h| h.id == hit(Part::Open(0))),
            "links get an open button"
        );
    }

    #[test]
    fn scrolling_clamps_animates_and_stops_asking_for_frames() {
        let mut t = T::new();
        for i in 1..=10 {
            t.add(i, &format!("item {i}"), ClipKind::Text);
        }
        let l = t.draw();
        assert!(
            l.hits
                .iter()
                .filter(|h| matches!(part_of(h.id), Some(Part::Row(_))))
                .count()
                <= VISIBLE_ROWS + 1,
            "only visible rows are interactive"
        );
        // Wheel down 3 notches (negative dy = toward older rows).
        let inside = t.m.list.center();
        for _ in 0..3 {
            assert!(t.input(
                None,
                Input::Wheel {
                    pos: inside,
                    dx: 0.0,
                    dy: -120.0
                }
            ));
        }
        assert_eq!(t.m.scroll.target(), 3.0);
        assert!(t.m.wants_frames(), "the scroll spring is moving");
        for _ in 0..200 {
            t.now += 1.0 / 60.0;
            t.draw();
        }
        assert!(!t.m.wants_frames(), "settled: the frame loop can sleep");
        assert!((t.m.scroll.value() - 3.0).abs() < 0.01);
        for _ in 0..50 {
            t.input(
                None,
                Input::Wheel {
                    pos: inside,
                    dx: 0.0,
                    dy: -120.0,
                },
            );
        }
        assert_eq!(
            t.m.scroll.target(),
            6.0,
            "clamped at the last row (10 rows - 4 visible)"
        );
        t.input(
            None,
            Input::Wheel {
                pos: inside,
                dx: 0.0,
                dy: 120.0 * 100.0,
            },
        );
        assert_eq!(t.m.scroll.target(), 0.0, "and at the top");
    }

    #[test]
    fn wheel_outside_the_list_or_without_overflow_is_left_to_the_shell() {
        let mut t = T::new();
        t.add(1, "only one", ClipKind::Text);
        t.draw();
        let inside = t.m.list.center();
        assert!(
            !t.input(
                None,
                Input::Wheel {
                    pos: inside,
                    dx: 0.0,
                    dy: -120.0
                }
            ),
            "nothing to scroll: the shell may switch pages"
        );
        for i in 2..=9 {
            t.add(i, "x", ClipKind::Text);
        }
        t.draw();
        assert!(
            !t.input(
                None,
                Input::Wheel {
                    pos: Vec2::new(-50.0, -50.0),
                    dx: 0.0,
                    dy: -120.0
                }
            ),
            "pointer outside the list"
        );
    }

    #[test]
    fn a_new_copy_scrolls_back_to_the_top() {
        let mut t = T::new();
        for i in 1..=8 {
            t.add(i, "x", ClipKind::Text);
        }
        t.draw();
        let inside = t.m.list.center();
        t.input(
            None,
            Input::Wheel {
                pos: inside,
                dx: 0.0,
                dy: -120.0 * 3.0,
            },
        );
        assert!(t.m.scroll.target() > 0.0);
        t.add(9, "fresh", ClipKind::Text);
        assert_eq!(t.m.scroll.target(), 0.0);
    }

    #[test]
    fn removing_rows_pulls_the_scroll_back_in_range() {
        let mut t = T::new();
        for i in 1..=8 {
            t.add(i, "x", ClipKind::Text);
        }
        t.draw();
        let inside = t.m.list.center();
        t.input(
            None,
            Input::Wheel {
                pos: inside,
                dx: 0.0,
                dy: -120.0 * 4.0,
            },
        );
        assert_eq!(t.m.scroll.target(), 4.0);
        for i in 1..=6 {
            t.send(Source::Local, EventKind::ClipboardRemoved(i));
        }
        assert_eq!(t.m.scroll.target(), 0.0, "2 rows left: nothing to scroll");
    }

    #[test]
    fn images_draw_their_thumbnail_and_text_gets_a_kind_tile() {
        let mut t = T::new();
        t.send(
            Source::Local,
            EventKind::ClipboardItem(ClipboardItem {
                id: 1,
                kind: ClipKind::Image,
                preview: "Image · 800×600".into(),
                thumb: 42,
                pinned: false,
            }),
        );
        t.add(2, "plain", ClipKind::Text);
        let l = t.draw();
        assert!(
            l.cmds
                .iter()
                .any(|c| matches!(c, DrawCmd::Image { id, .. } if id.0 == 42))
        );
        assert!(l.cmds.iter().any(|c| matches!(
            c,
            DrawCmd::Icon {
                icon: Icon::Doc,
                ..
            }
        )));
        assert!(l.is_balanced());
    }

    #[test]
    fn works_inside_the_host_and_is_toggleable() {
        let mut base = Config::default();
        base.modules.order = vec!["media".into(), "clipboard".into(), "clock".into()];
        let mut host = ModuleHost::new(
            crate::modules::registry(),
            Arc::new(base.clone()),
            Theme::default(),
        );
        assert_eq!(
            host.page_ids(),
            vec!["clipboard", "clock"],
            "always present (empty state); media only once there is a session"
        );
        host.dispatch(vec![Event::new(
            Source::Local,
            EventKind::ClipboardItem(item(1, "x", ClipKind::Text)),
        )]);
        assert!(host.take_out().shell.is_empty());
        let mut cfg = base;
        cfg.clipboard.enabled = false;
        assert!(!cfg.module_active("clipboard"));
        host.apply_config(Arc::new(cfg));
        assert_eq!(host.page_ids(), vec!["clock"]);
    }

    #[test]
    fn the_peek_names_the_origin() {
        let mut t = T::new();
        t.send(
            Source::Phone,
            EventKind::ClipboardItem(item(1, "https://example.com/x", ClipKind::Link)),
        );
        let mut list = DrawList::new();
        {
            let mut cv = Canvas::new(&mut list, &t.theme);
            t.m.draw_peek(
                &mut cv,
                Rect::new(0.0, 0.0, 294.0, 30.0),
                &DrawCx {
                    now: 0.0,
                    env: &t.env,
                    config: &t.cfg,
                },
            );
        }
        let tx = texts(&list);
        assert!(tx.contains(&"Copied on iPhone".to_string()), "{tx:?}");
        assert!(list.is_balanced());
    }
}
