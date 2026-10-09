//! Vector icons on a 24x24 grid, drawn by every backend from the same path data.
//!
//! Style: 2-unit strokes with round caps/joins (Feather-like), a few solid glyphs. No font
//! dependency means icons are crisp at any DPI, identical in the preview PNGs, and cost nothing
//! to load.

use crate::geom::Vec2;
use crate::path::Path;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Icon {
    Play,
    Pause,
    Next,
    Prev,
    Close,
    Check,
    Plus,
    ChevronLeft,
    ChevronRight,
    ChevronUp,
    ChevronDown,
    Clock,
    /// Beamed music notes (placeholder album art).
    Note,
    Pin,
    Trash,
    /// A globe: "this is a link".
    Globe,
    Image,
    /// Text lines: "this is text".
    Doc,
    /// Arrow leaving a box: open externally.
    Open,
    Folder,
    /// An inbox tray with a down arrow: "drop it here".
    Tray,
    Bell,
    Calendar,
    /// A video camera: "join the call".
    Video,
    /// A circular arrow: start over.
    Reset,
    /// A stopwatch.
    Timer,
    Mic,
    /// An arrow down into a tray: a download.
    Download,
    /// A heartbeat line: system activity.
    Pulse,
    /// A lightning bolt: charging.
    Bolt,
    /// Wi-Fi: a dot and three arcs.
    Wifi,
    /// The Bluetooth rune.
    Bluetooth,
    /// A crescent moon: do not disturb.
    Moon,
    /// A speaker with sound waves.
    Speaker,
    /// A speaker with a cross: muted.
    SpeakerMuted,
    /// A sun: brightness.
    Sun,
    /// Corner brackets: a screen snip.
    Snip,
    /// A cog: settings.
    Gear,
}

impl Icon {
    /// Every icon, for tests and the preview tool.
    pub const ALL: [Icon; 38] = [
        Icon::Play,
        Icon::Pause,
        Icon::Next,
        Icon::Prev,
        Icon::Close,
        Icon::Check,
        Icon::Plus,
        Icon::ChevronLeft,
        Icon::ChevronRight,
        Icon::ChevronUp,
        Icon::ChevronDown,
        Icon::Clock,
        Icon::Note,
        Icon::Pin,
        Icon::Trash,
        Icon::Globe,
        Icon::Image,
        Icon::Doc,
        Icon::Open,
        Icon::Folder,
        Icon::Tray,
        Icon::Bell,
        Icon::Calendar,
        Icon::Video,
        Icon::Reset,
        Icon::Timer,
        Icon::Mic,
        Icon::Download,
        Icon::Pulse,
        Icon::Bolt,
        Icon::Wifi,
        Icon::Bluetooth,
        Icon::Moon,
        Icon::Speaker,
        Icon::SpeakerMuted,
        Icon::Sun,
        Icon::Snip,
        Icon::Gear,
    ];
}

#[derive(Clone, Debug)]
pub enum IconOp {
    Fill(Path),
    /// Stroke with round caps and joins; width is in grid units (scaled with the icon).
    Stroke(Path, f32),
}

/// Side length of the design grid.
pub const GRID: f32 = 24.0;

fn p(x: f32, y: f32) -> Vec2 {
    Vec2::new(x, y)
}

fn polyline(pts: &[(f32, f32)], closed: bool) -> Path {
    let mut path = Path::new();
    for (i, &(x, y)) in pts.iter().enumerate() {
        if i == 0 {
            path.move_to(p(x, y));
        } else {
            path.line_to(p(x, y));
        }
    }
    if closed {
        path.close();
    }
    path
}

/// Full circle as four cubic Béziers.
pub fn circle_path(cx: f32, cy: f32, r: f32) -> Path {
    let k = 0.552_284_7 * r;
    let mut path = Path::new();
    path.move_to(p(cx + r, cy));
    path.cubic_to(p(cx + r, cy + k), p(cx + k, cy + r), p(cx, cy + r));
    path.cubic_to(p(cx - k, cy + r), p(cx - r, cy + k), p(cx - r, cy));
    path.cubic_to(p(cx - r, cy - k), p(cx - k, cy - r), p(cx, cy - r));
    path.cubic_to(p(cx + k, cy - r), p(cx + r, cy - k), p(cx + r, cy));
    path.close();
    path
}

/// Axis-aligned ellipse as four cubic Béziers.
pub fn ellipse_path(cx: f32, cy: f32, rx: f32, ry: f32) -> Path {
    let (kx, ky) = (0.552_284_7 * rx, 0.552_284_7 * ry);
    let mut path = Path::new();
    path.move_to(p(cx + rx, cy));
    path.cubic_to(p(cx + rx, cy + ky), p(cx + kx, cy + ry), p(cx, cy + ry));
    path.cubic_to(p(cx - kx, cy + ry), p(cx - rx, cy + ky), p(cx - rx, cy));
    path.cubic_to(p(cx - rx, cy - ky), p(cx - kx, cy - ry), p(cx, cy - ry));
    path.cubic_to(p(cx + kx, cy - ry), p(cx + rx, cy - ky), p(cx + rx, cy));
    path.close();
    path
}

/// Circular arc; angles in degrees, 0 = 12 o'clock, clockwise. Open path.
pub fn arc_path(cx: f32, cy: f32, r: f32, start_deg: f32, sweep_deg: f32) -> Path {
    let mut path = Path::new();
    let segs = ((sweep_deg.abs() / 90.0).ceil() as usize).max(1);
    let step = sweep_deg / segs as f32;
    let at = |deg: f32| {
        let a = deg.to_radians();
        (cx + r * a.sin(), cy - r * a.cos())
    };
    let (sx, sy) = at(start_deg);
    path.move_to(p(sx, sy));
    for i in 0..segs {
        let a0 = start_deg + step * i as f32;
        let a1 = a0 + step;
        let h = 4.0 / 3.0 * ((a1 - a0).to_radians() / 4.0).tan() * r;
        let (x0, y0) = at(a0);
        let (x3, y3) = at(a1);
        // Tangent for clockwise travel at angle a (0 = top): (cos a, sin a).
        let t0 = (a0.to_radians().cos(), a0.to_radians().sin());
        let t1 = (a1.to_radians().cos(), a1.to_radians().sin());
        path.cubic_to(
            p(x0 + h * t0.0, y0 + h * t0.1),
            p(x3 - h * t1.0, y3 - h * t1.1),
            p(x3, y3),
        );
    }
    path
}

/// Rounded rectangle with a uniform circular radius (for icon bodies).
pub fn rrect_path(x: f32, y: f32, w: f32, h: f32, r: f32) -> Path {
    let r = r.min(w * 0.5).min(h * 0.5);
    let k = 0.552_284_7 * r;
    let (x1, y1) = (x + w, y + h);
    let mut path = Path::new();
    path.move_to(p(x + r, y));
    path.line_to(p(x1 - r, y));
    path.cubic_to(p(x1 - r + k, y), p(x1, y + r - k), p(x1, y + r));
    path.line_to(p(x1, y1 - r));
    path.cubic_to(p(x1, y1 - r + k), p(x1 - r + k, y1), p(x1 - r, y1));
    path.line_to(p(x + r, y1));
    path.cubic_to(p(x + r - k, y1), p(x, y1 - r + k), p(x, y1 - r));
    path.line_to(p(x, y + r));
    path.cubic_to(p(x, y + r - k), p(x + r - k, y), p(x + r, y));
    path.close();
    path
}

/// The drawing operations for `icon` on the 24x24 grid.
pub fn ops(icon: Icon) -> Vec<IconOp> {
    use IconOp::{Fill, Stroke};
    match icon {
        Icon::Play => {
            let tri = polyline(&[(8.0, 5.5), (18.5, 12.0), (8.0, 18.5)], true);
            vec![Fill(tri.clone()), Stroke(tri, 2.0)]
        }
        Icon::Pause => vec![
            Fill(rrect_path(6.0, 5.0, 4.2, 14.0, 1.4)),
            Fill(rrect_path(13.8, 5.0, 4.2, 14.0, 1.4)),
        ],
        Icon::Next => {
            let tri = polyline(&[(5.5, 5.5), (15.5, 12.0), (5.5, 18.5)], true);
            vec![
                Fill(tri.clone()),
                Stroke(tri, 2.0),
                Stroke(polyline(&[(19.0, 5.5), (19.0, 18.5)], false), 2.4),
            ]
        }
        Icon::Prev => {
            let tri = polyline(&[(18.5, 5.5), (8.5, 12.0), (18.5, 18.5)], true);
            vec![
                Fill(tri.clone()),
                Stroke(tri, 2.0),
                Stroke(polyline(&[(5.0, 5.5), (5.0, 18.5)], false), 2.4),
            ]
        }
        Icon::Close => vec![
            Stroke(polyline(&[(6.0, 6.0), (18.0, 18.0)], false), 2.0),
            Stroke(polyline(&[(18.0, 6.0), (6.0, 18.0)], false), 2.0),
        ],
        Icon::Check => vec![Stroke(
            polyline(&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)], false),
            2.2,
        )],
        Icon::Plus => vec![
            Stroke(polyline(&[(12.0, 5.0), (12.0, 19.0)], false), 2.0),
            Stroke(polyline(&[(5.0, 12.0), (19.0, 12.0)], false), 2.0),
        ],
        Icon::ChevronLeft => vec![Stroke(
            polyline(&[(14.5, 6.0), (8.5, 12.0), (14.5, 18.0)], false),
            2.2,
        )],
        Icon::ChevronRight => vec![Stroke(
            polyline(&[(9.5, 6.0), (15.5, 12.0), (9.5, 18.0)], false),
            2.2,
        )],
        Icon::ChevronUp => vec![Stroke(
            polyline(&[(6.0, 14.5), (12.0, 8.5), (18.0, 14.5)], false),
            2.2,
        )],
        Icon::ChevronDown => vec![Stroke(
            polyline(&[(6.0, 9.5), (12.0, 15.5), (18.0, 9.5)], false),
            2.2,
        )],
        Icon::Clock => vec![
            Stroke(circle_path(12.0, 12.0, 8.5), 2.0),
            Stroke(
                polyline(&[(12.0, 7.0), (12.0, 12.0), (15.5, 14.0)], false),
                2.0,
            ),
        ],
        Icon::Pin => vec![
            Stroke(
                polyline(
                    &[
                        (9.0, 3.5),
                        (15.0, 3.5),
                        (14.2, 9.5),
                        (18.0, 13.5),
                        (6.0, 13.5),
                        (9.8, 9.5),
                    ],
                    true,
                ),
                2.0,
            ),
            Stroke(polyline(&[(12.0, 13.5), (12.0, 20.5)], false), 2.0),
        ],
        Icon::Trash => vec![
            Stroke(polyline(&[(4.5, 7.0), (19.5, 7.0)], false), 2.0),
            Stroke(
                polyline(&[(6.5, 7.0), (7.5, 19.5), (16.5, 19.5), (17.5, 7.0)], false),
                2.0,
            ),
            Stroke(
                polyline(&[(9.0, 7.0), (9.0, 4.0), (15.0, 4.0), (15.0, 7.0)], false),
                2.0,
            ),
        ],
        Icon::Globe => vec![
            Stroke(circle_path(12.0, 12.0, 8.5), 2.0),
            Stroke(polyline(&[(3.5, 12.0), (20.5, 12.0)], false), 1.8),
            Stroke(ellipse_path(12.0, 12.0, 3.8, 8.5), 1.8),
        ],
        Icon::Image => vec![
            Stroke(rrect_path(3.8, 5.0, 16.4, 14.0, 2.8), 2.0),
            Fill(circle_path(9.0, 10.0, 1.7)),
            Stroke(
                polyline(
                    &[
                        (5.5, 17.5),
                        (10.0, 13.0),
                        (13.5, 16.5),
                        (15.5, 14.5),
                        (18.5, 17.5),
                    ],
                    false,
                ),
                1.9,
            ),
        ],
        Icon::Doc => vec![
            Stroke(polyline(&[(5.0, 7.0), (19.0, 7.0)], false), 2.0),
            Stroke(polyline(&[(5.0, 12.0), (19.0, 12.0)], false), 2.0),
            Stroke(polyline(&[(5.0, 17.0), (13.5, 17.0)], false), 2.0),
        ],
        Icon::Open => vec![
            Stroke(
                polyline(&[(14.0, 4.5), (19.5, 4.5), (19.5, 10.0)], false),
                2.0,
            ),
            Stroke(polyline(&[(19.5, 4.5), (11.0, 13.0)], false), 2.0),
            Stroke(
                polyline(
                    &[
                        (17.5, 14.5),
                        (17.5, 19.5),
                        (4.5, 19.5),
                        (4.5, 6.5),
                        (9.5, 6.5),
                    ],
                    false,
                ),
                2.0,
            ),
        ],
        Icon::Bell => {
            let mut body = Path::new();
            body.move_to(p(5.0, 17.5));
            body.line_to(p(6.5, 16.0));
            body.line_to(p(6.5, 11.0));
            body.cubic_to(p(6.5, 5.5), p(17.5, 5.5), p(17.5, 11.0));
            body.line_to(p(17.5, 16.0));
            body.line_to(p(19.0, 17.5));
            body.close();
            vec![
                Stroke(body, 2.0),
                Stroke(polyline(&[(12.0, 3.2), (12.0, 5.2)], false), 2.0),
                Stroke(polyline(&[(10.0, 20.5), (14.0, 20.5)], false), 2.0),
            ]
        }
        Icon::Folder => vec![Stroke(
            polyline(
                &[
                    (3.5, 18.5),
                    (3.5, 6.0),
                    (9.5, 6.0),
                    (11.5, 8.5),
                    (20.5, 8.5),
                    (20.5, 18.5),
                ],
                true,
            ),
            2.0,
        )],
        Icon::Tray => vec![
            Stroke(
                polyline(
                    &[
                        (3.5, 13.5),
                        (3.5, 19.5),
                        (20.5, 19.5),
                        (20.5, 13.5),
                        (15.5, 13.5),
                        (14.0, 16.0),
                        (10.0, 16.0),
                        (8.5, 13.5),
                    ],
                    true,
                ),
                2.0,
            ),
            Stroke(polyline(&[(12.0, 3.5), (12.0, 11.5)], false), 2.0),
            Stroke(
                polyline(&[(8.5, 8.5), (12.0, 12.0), (15.5, 8.5)], false),
                2.0,
            ),
        ],
        Icon::Calendar => vec![
            Stroke(rrect_path(4.0, 5.5, 16.0, 14.5, 2.8), 2.0),
            Stroke(polyline(&[(4.0, 10.5), (20.0, 10.5)], false), 2.0),
            Stroke(polyline(&[(8.5, 3.5), (8.5, 7.0)], false), 2.0),
            Stroke(polyline(&[(15.5, 3.5), (15.5, 7.0)], false), 2.0),
            Fill(circle_path(8.5, 14.8, 1.2)),
            Fill(circle_path(12.0, 14.8, 1.2)),
            Fill(circle_path(15.5, 14.8, 1.2)),
        ],
        Icon::Video => vec![
            Stroke(rrect_path(3.5, 6.5, 11.5, 11.0, 2.6), 2.0),
            Stroke(
                polyline(
                    &[(15.0, 10.5), (20.5, 7.5), (20.5, 16.5), (15.0, 13.5)],
                    true,
                ),
                2.0,
            ),
        ],
        Icon::Reset => {
            // Counter-clockwise arc from the top-right round to the top, with an arrowhead at its end.
            let arc = arc_path(12.0, 13.0, 7.0, 330.0, -300.0);
            let a = 30.0f32.to_radians();
            let tip = (12.0 + 7.0 * a.sin(), 13.0 - 7.0 * a.cos());
            // Direction of travel at the tip (counter-clockwise) and the two wings pointing back.
            let back = (a.cos(), a.sin());
            let wing = |deg: f32| {
                let (s, c) = deg.to_radians().sin_cos();
                (
                    tip.0 + 4.2 * (back.0 * c - back.1 * s),
                    tip.1 + 4.2 * (back.0 * s + back.1 * c),
                )
            };
            vec![
                Stroke(arc, 2.0),
                Stroke(polyline(&[wing(40.0), tip, wing(-40.0)], false), 2.0),
            ]
        }
        Icon::Mic => vec![
            Stroke(rrect_path(8.5, 3.5, 7.0, 11.5, 3.5), 2.0),
            Stroke(arc_path(12.0, 11.5, 6.6, 90.0, 180.0), 2.0),
            Stroke(polyline(&[(12.0, 18.1), (12.0, 21.0)], false), 2.0),
            Stroke(polyline(&[(8.5, 21.0), (15.5, 21.0)], false), 2.0),
        ],
        Icon::Pulse => vec![Stroke(
            polyline(
                &[
                    (2.5, 12.5),
                    (7.0, 12.5),
                    (9.5, 5.0),
                    (14.0, 19.5),
                    (16.5, 12.5),
                    (21.5, 12.5),
                ],
                false,
            ),
            2.0,
        )],
        Icon::Bolt => vec![Fill(polyline(
            &[
                (13.5, 2.0),
                (5.0, 13.5),
                (11.0, 13.5),
                (10.0, 22.0),
                (19.0, 10.0),
                (13.0, 10.0),
            ],
            true,
        ))],
        Icon::Wifi => {
            let mut v: Vec<IconOp> = [5.0, 9.0, 13.0]
                .iter()
                .map(|&r| Stroke(arc_path(12.0, 19.0, r, -45.0, 90.0), 2.0))
                .collect();
            v.push(Fill(circle_path(12.0, 19.0, 1.7)));
            v
        }
        Icon::Bluetooth => vec![Stroke(
            polyline(
                &[
                    (7.0, 8.0),
                    (17.0, 16.0),
                    (12.0, 21.0),
                    (12.0, 3.0),
                    (17.0, 8.0),
                    (7.0, 16.0),
                ],
                false,
            ),
            2.0,
        )],
        Icon::Moon => {
            // A crescent: the big circle's long arc, then back along a smaller circle's arc.
            let mut path = arc_path(12.0, 12.0, 9.0, 95.0, 260.0);
            let inner = arc_path(16.84, 7.16, 7.0, 306.5, -163.0);
            path.cmds.extend(inner.cmds.into_iter().skip(1));
            path.close();
            vec![Stroke(path, 2.0)]
        }
        Icon::Speaker | Icon::SpeakerMuted => {
            let mut v = vec![Stroke(
                polyline(
                    &[
                        (3.5, 9.5),
                        (7.5, 9.5),
                        (12.5, 5.0),
                        (12.5, 19.0),
                        (7.5, 14.5),
                        (3.5, 14.5),
                    ],
                    true,
                ),
                2.0,
            )];
            if icon == Icon::Speaker {
                v.push(Stroke(arc_path(12.5, 12.0, 4.4, 55.0, 70.0), 2.0));
                v.push(Stroke(arc_path(12.5, 12.0, 8.2, 52.0, 76.0), 2.0));
            } else {
                v.push(Stroke(polyline(&[(16.5, 9.5), (21.5, 14.5)], false), 2.0));
                v.push(Stroke(polyline(&[(21.5, 9.5), (16.5, 14.5)], false), 2.0));
            }
            v
        }
        Icon::Sun => {
            let mut v = vec![Stroke(circle_path(12.0, 12.0, 4.2), 2.0)];
            for k in 0..8 {
                let a = (k as f32 * 45.0).to_radians();
                let (s, c) = (a.sin(), a.cos());
                v.push(Stroke(
                    polyline(
                        &[
                            (12.0 + 7.4 * s, 12.0 - 7.4 * c),
                            (12.0 + 9.8 * s, 12.0 - 9.8 * c),
                        ],
                        false,
                    ),
                    2.0,
                ));
            }
            v
        }
        Icon::Gear => {
            let mut v = vec![
                Stroke(circle_path(12.0, 12.0, 3.2), 2.0),
                Stroke(circle_path(12.0, 12.0, 6.6), 2.0),
            ];
            for k in 0..8 {
                let a = (k as f32 * 45.0).to_radians();
                let (s, c) = (a.sin(), a.cos());
                v.push(Stroke(
                    polyline(
                        &[
                            (12.0 + 7.0 * s, 12.0 - 7.0 * c),
                            (12.0 + 9.6 * s, 12.0 - 9.6 * c),
                        ],
                        false,
                    ),
                    3.0,
                ));
            }
            v
        }
        Icon::Snip => vec![
            Stroke(polyline(&[(3.0, 9.0), (3.0, 3.0), (9.0, 3.0)], false), 2.0),
            Stroke(
                polyline(&[(15.0, 3.0), (21.0, 3.0), (21.0, 9.0)], false),
                2.0,
            ),
            Stroke(
                polyline(&[(21.0, 15.0), (21.0, 21.0), (15.0, 21.0)], false),
                2.0,
            ),
            Stroke(
                polyline(&[(9.0, 21.0), (3.0, 21.0), (3.0, 15.0)], false),
                2.0,
            ),
        ],
        Icon::Download => vec![
            Stroke(polyline(&[(12.0, 3.5), (12.0, 14.5)], false), 2.0),
            Stroke(
                polyline(&[(7.5, 10.5), (12.0, 15.0), (16.5, 10.5)], false),
                2.0,
            ),
            Stroke(
                polyline(
                    &[(4.0, 15.5), (4.0, 20.0), (20.0, 20.0), (20.0, 15.5)],
                    false,
                ),
                2.0,
            ),
        ],
        Icon::Timer => vec![
            Stroke(circle_path(12.0, 13.5, 7.5), 2.0),
            Stroke(polyline(&[(12.0, 13.5), (12.0, 9.5)], false), 2.0),
            Stroke(polyline(&[(9.5, 3.5), (14.5, 3.5)], false), 2.0),
            Stroke(polyline(&[(12.0, 3.5), (12.0, 6.0)], false), 2.0),
        ],
        Icon::Note => vec![
            Fill(circle_path(7.2, 17.6, 2.9)),
            Fill(circle_path(17.2, 15.6, 2.9)),
            Stroke(polyline(&[(10.1, 17.6), (10.1, 6.2)], false), 2.0),
            Stroke(polyline(&[(20.1, 15.6), (20.1, 4.2)], false), 2.0),
            Stroke(polyline(&[(10.1, 6.6), (20.1, 4.6)], false), 2.6),
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_fits_the_grid_with_room_for_its_stroke() {
        for icon in Icon::ALL {
            let o = ops(icon);
            assert!(!o.is_empty(), "{icon:?}");
            for op in o {
                let (path, half) = match &op {
                    IconOp::Fill(p) => (p, 0.0),
                    IconOp::Stroke(p, w) => (p, w * 0.5),
                };
                let b = path.bounds();
                assert!(b.x - half >= -0.01 && b.y - half >= -0.01, "{icon:?} {b:?}");
                assert!(
                    b.right() + half <= GRID + 0.01 && b.bottom() + half <= GRID + 0.01,
                    "{icon:?} {b:?}"
                );
            }
        }
    }

    #[test]
    fn circle_and_arc_are_round() {
        let c = circle_path(12.0, 12.0, 8.0);
        let poly = &c.flatten(0.01)[0];
        for v in poly {
            let d = ((v.x - 12.0).powi(2) + (v.y - 12.0).powi(2)).sqrt();
            assert!((d - 8.0).abs() < 0.05, "off-circle by {}", d - 8.0);
        }
        // Quarter arc from 12 o'clock to 3 o'clock.
        let a = arc_path(0.0, 0.0, 10.0, 0.0, 90.0);
        let pts = &a.flatten(0.01)[0];
        let first = pts.first().unwrap();
        let last = pts.last().unwrap();
        assert!(
            (first.x).abs() < 1e-3 && (first.y + 10.0).abs() < 1e-3,
            "starts at the top: {first:?}"
        );
        assert!(
            (last.x - 10.0).abs() < 1e-3 && last.y.abs() < 1e-3,
            "ends at 3 o'clock: {last:?}"
        );
        for v in pts {
            assert!((v.x.hypot(v.y) - 10.0).abs() < 0.02);
        }
    }

    #[test]
    fn full_sweep_arc_closes_on_itself() {
        let a = arc_path(5.0, 5.0, 4.0, 0.0, 360.0);
        let pts = &a.flatten(0.01)[0];
        let (f, l) = (pts.first().unwrap(), pts.last().unwrap());
        assert!((f.x - l.x).abs() < 1e-3 && (f.y - l.y).abs() < 1e-3);
    }
}
