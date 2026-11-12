//! Screen edges (PRD 0005). Each peer's desktop sits at `position` on this
//! one: its point `p` is this desktop's `p + position`. The cursor crosses
//! where it would step off one desktop onto the other.

#[cfg(target_os = "linux")]
pub mod portal;
#[cfg(target_os = "linux")]
pub mod sway;

use crate::monitors::Monitor;

pub fn on_screen(monitors: &[Monitor], (x, y): (i32, i32)) -> bool {
    monitors
        .iter()
        .any(|m| (m.x..m.x + m.width).contains(&x) && (m.y..m.y + m.height).contains(&y))
}

/// The pixel the cursor at `at` would step onto with a motion of `delta`,
/// if that pixel is off every monitor of `here`: the cursor is pushing
/// Asking where the cursor is costs a round trip to the compositor on
/// Linux (Hyprland's socket), so motion is summed and the edge checked at
/// most once per `EDGE_CHECK` instead of on every mouse report (#187): at
/// 1000 Hz, 125 questions a second instead of 1000, a crossing at most
/// 8 ms later.
pub const EDGE_CHECK: std::time::Duration = std::time::Duration::from_millis(8);

/// Asks where the cursor is on a thread of its own, so the thread that
/// injects the mouse never waits for the compositor (#187): one answer took
/// 2.7 ms at p99 on Hyprland, and every event behind it waited too. Motion
/// goes in with `push`; whether it pushed off this desktop comes back, a
/// moment later, from `answer`. The thread ends with this.
pub struct EdgeWatch {
    pushed: std::sync::mpsc::Sender<(i32, i32)>,
    answers: std::sync::mpsc::Receiver<Option<(i32, i32)>>,
    monitors: std::sync::mpsc::Sender<Vec<Monitor>>,
}

impl EdgeWatch {
    pub fn start(mut monitors: Vec<Monitor>, cursor: fn() -> Option<(i32, i32)>) -> EdgeWatch {
        let (pushed, motion) = std::sync::mpsc::channel::<(i32, i32)>();
        let (answer, answers) = std::sync::mpsc::channel();
        let (moved, new_monitors) = std::sync::mpsc::channel();
        crate::threads::spawn("edge check", move || {
            while let Ok(mut delta) = motion.recv() {
                // Motion that came while the last question was out joins in.
                for (dx, dy) in motion.try_iter() {
                    delta = (delta.0.saturating_add(dx), delta.1.saturating_add(dy));
                }
                if let Some(newest) = new_monitors.try_iter().last() {
                    monitors = newest;
                }
                let off = cursor().and_then(|at| beyond(&monitors, at, delta));
                if answer.send(off).is_err() {
                    break;
                }
            }
        });
        EdgeWatch {
            pushed,
            answers,
            monitors: moved,
        }
    }

    /// This PC's monitors moved: the next question uses these.
    pub fn set_monitors(&self, monitors: Vec<Monitor>) {
        let _ = self.monitors.send(monitors);
    }

    /// Hands motion over, without waiting.
    pub fn push(&self, delta: (i32, i32)) {
        let _ = self.pushed.send(delta);
    }

    /// The newest answer, if one came since the last call: the pixel off
    /// this desktop the cursor is pushing onto, or `Some(None)` when it is
    /// not pushing off.
    pub fn answer(&self) -> Option<Option<(i32, i32)>> {
        self.answers.try_iter().last()
    }
}

/// Motion summed between two edge checks.
#[derive(Debug)]
pub struct Throttle {
    checked: std::time::Instant,
    sum: (i32, i32),
}

impl Throttle {
    pub fn new() -> Throttle {
        Throttle {
            checked: std::time::Instant::now(),
            sum: (0, 0),
        }
    }

    /// Adds a move; returns the motion summed since the last check when it
    /// is time to check again (and there was motion), then starts over.
    pub fn feed(&mut self, delta: (i32, i32)) -> Option<(i32, i32)> {
        self.feed_at(delta, std::time::Instant::now())
    }

    fn feed_at(&mut self, (dx, dy): (i32, i32), now: std::time::Instant) -> Option<(i32, i32)> {
        self.sum = (self.sum.0.saturating_add(dx), self.sum.1.saturating_add(dy));
        if now.duration_since(self.checked) < EDGE_CHECK || self.sum == (0, 0) {
            return None;
        }
        self.checked = now;
        Some(std::mem::take(&mut self.sum))
    }
}

/// against the edge of its desktop there.
pub fn beyond(here: &[Monitor], at: (i32, i32), (dx, dy): (i32, i32)) -> Option<(i32, i32)> {
    if !on_screen(here, at) {
        return None;
    }
    // One axis at a time, so pushing along an edge (down a left edge, say)
    // is not mistaken for leaving through it.
    let steps = [(dx.signum(), 0), (0, dy.signum())];
    steps
        .into_iter()
        .filter(|&step| step != (0, 0))
        .map(|(sx, sy)| (at.0 + sx, at.1 + sy))
        .find(|&next| !on_screen(here, next))
}

/// `point` of this desktop as a point of the peer at `position`, if one
/// of the peer's monitors is there.
pub fn onto(peer: &[Monitor], [x, y]: [i32; 2], point: (i32, i32)) -> Option<(i32, i32)> {
    let there = (point.0 - x, point.1 - y);
    on_screen(peer, there).then_some(there)
}

/// `point` of the peer at `[x, y]` as a point of this desktop's layout,
/// whether or not a monitor of this PC is there: it may be another peer's.
pub fn in_layout([x, y]: [i32; 2], point: (i32, i32)) -> (i32, i32) {
    (point.0 + x, point.1 + y)
}

/// `point` of the peer at `position` as a point of this desktop, if one of
/// this PC's monitors is there.
pub fn back(home: &[Monitor], position: [i32; 2], point: (i32, i32)) -> Option<(i32, i32)> {
    let here = in_layout(position, point);
    on_screen(home, here).then_some(here)
}

/// Where the cursor comes home when it pushed off the peer at `position`
/// at `point` (a pixel just off the peer's `monitors`) but no monitor of
/// this PC is right there: the peer's edge is longer than the stretch it
/// shares with this desktop (a 1080 px tall peer next to a 720 px tall
/// laptop screen). Without this the lower part of that edge led nowhere and
/// the cursor was stuck over there. Only along the side it pushed through:
/// out the left edge, the nearest point of a monitor of this PC in that
/// column; out the top or bottom, in that row. Never across the desktop.
pub fn back_near(
    home: &[Monitor],
    peer: &[Monitor],
    position: [i32; 2],
    point: (i32, i32),
) -> Option<(i32, i32)> {
    let (x, y) = in_layout(position, point);
    let sideways =
        on_screen(peer, (point.0 - 1, point.1)) || on_screen(peer, (point.0 + 1, point.1));
    let upwards =
        on_screen(peer, (point.0, point.1 - 1)) || on_screen(peer, (point.0, point.1 + 1));
    home.iter()
        .filter_map(|m| {
            if sideways && (m.x..m.x + m.width).contains(&x) {
                Some((x, y.clamp(m.y, m.y + m.height - 1)))
            } else if upwards && (m.y..m.y + m.height).contains(&y) {
                Some((x.clamp(m.x, m.x + m.width - 1), y))
            } else {
                None
            }
        })
        .min_by_key(|&(hx, hy)| (hx - x).abs() + (hy - y).abs())
}

/// The stretches of this desktop's edges that lead to a placed peer, as
/// `(x1, y1, x2, y2)` lines just outside the monitors: where a GNOME or
/// KDE PC puts its InputCapture barriers (PRD 0004, phase 2). `peers` are
/// each placed peer's monitors and its position on this desktop.
/// A stretch of an edge, `(x1, y1, x2, y2)`, both ends included.
#[cfg(any(target_os = "linux", test))]
pub type Line = (i32, i32, i32, i32);

#[cfg(any(target_os = "linux", test))]
pub fn barriers(here: &[Monitor], peers: &[(&[Monitor], [i32; 2])]) -> Vec<Line> {
    let theirs: Vec<Monitor> = peers
        .iter()
        .flat_map(|(monitors, [px, py])| {
            monitors.iter().map(move |m| Monitor {
                x: m.x + px,
                y: m.y + py,
                ..*m
            })
        })
        .collect();
    // The overlap of [a, a + a_len) and [b, b + b_len), if any.
    let overlap = |a: i32, a_len: i32, b: i32, b_len: i32| {
        let (start, end) = (a.max(b), (a + a_len).min(b + b_len));
        (start < end).then_some((start, end - 1))
    };
    let mut lines = Vec::new();
    for m in here {
        for t in &theirs {
            // Right, left, bottom, top: the peer's monitor begins where this
            // one ends, along a shared stretch.
            if t.x == m.x + m.width
                && let Some((y1, y2)) = overlap(m.y, m.height, t.y, t.height)
            {
                lines.push((t.x, y1, t.x, y2));
            }
            if t.x + t.width == m.x
                && let Some((y1, y2)) = overlap(m.y, m.height, t.y, t.height)
            {
                lines.push((m.x, y1, m.x, y2));
            }
            if t.y == m.y + m.height
                && let Some((x1, x2)) = overlap(m.x, m.width, t.x, t.width)
            {
                lines.push((x1, t.y, x2, t.y));
            }
            if t.y + t.height == m.y
                && let Some((x1, x2)) = overlap(m.x, m.width, t.x, t.width)
            {
                lines.push((x1, m.y, x2, m.y));
            }
        }
    }
    lines.sort_unstable();
    lines.dedup();
    lines
}

#[cfg(test)]
mod tests {
    fn m(x: i32, y: i32, width: i32, height: i32) -> Monitor {
        Monitor {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn back_near_comes_home_below_the_shared_stretch() {
        // The firebee desk: a TV, the laptop (1280x720 at 2560,508) and the
        // NIVA (1920x1080) to the right of the laptop, at 3840,689.
        let home = [m(0, 0, 2560, 1080), m(2560, 508, 1280, 720)];
        let niva = [m(0, 0, 1920, 1080)];
        let at = [3840, 689];
        // Out the left edge high up: straight across, as before.
        assert_eq!(back(&home, at, (-1, 100)), Some((3839, 789)));
        // Lower down nothing of this PC is there...
        assert_eq!(back(&home, at, (-1, 900)), None);
        // ...so it lands at the bottom of the laptop, in that column.
        assert_eq!(back_near(&home, &niva, at, (-1, 900)), Some((3839, 1227)));
        // Out the right edge or the bottom, where nothing of this PC is in
        // line: stays over there.
        assert_eq!(back_near(&home, &niva, at, (1920, 900)), None);
        assert_eq!(back_near(&home, &niva, at, (500, 1080)), None);
    }

    #[test]
    fn barriers_go_where_a_placed_peer_touches() {
        let laptop = [m(0, 0, 1280, 800)];
        let desk = [m(0, 0, 1920, 1080)];
        // The desk to the right, its top 100 px above the laptop's.
        let right = barriers(&laptop, &[(&desk, [1280, -100])]);
        assert_eq!(right, [(1280, 0, 1280, 799)]);
        // To the left, lower down: only the shared stretch.
        let left = barriers(&laptop, &[(&desk, [-1920, 500])]);
        assert_eq!(left, [(0, 500, 0, 799)]);
        // Above.
        let above = barriers(&laptop, &[(&desk, [-300, -1080])]);
        assert_eq!(above, [(0, 0, 1279, 0)]);
        // Placed apart, touching nothing: no barrier.
        assert!(barriers(&laptop, &[(&desk, [1400, 0])]).is_empty());
    }

    /// #187's measurement, on a real Hyprland: how long the thread that
    /// injects the mouse waits per event at 1000 Hz, when the edge check
    /// runs on its own thread. `cargo test --release edge_watch_cost --
    /// --ignored --nocapture`, in a Hyprland session.
    #[cfg(target_os = "linux")]
    #[test]
    #[ignore = "needs a Hyprland session"]
    fn edge_watch_cost() {
        let screen = [Monitor {
            x: 0,
            y: 0,
            width: 1920,
            height: 1080,
        }];
        let cursor = || crate::desktop::hyprland::cursor().ok();
        let watch = EdgeWatch::start(screen.to_vec(), cursor);
        let mut throttle = Throttle::new();
        let mut waits = Vec::new();
        for _ in 0..8000 {
            let started = std::time::Instant::now();
            if let Some(delta) = throttle.feed((1, 0)) {
                watch.push(delta);
            }
            let _ = watch.answer();
            waits.push(started.elapsed().as_micros());
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        waits.sort();
        let at = |q: f64| waits[((waits.len() as f64 * q) as usize).min(waits.len() - 1)];
        println!(
            "an event waits: p99 {} us, p99.9 {} us, max {} us",
            at(0.99),
            at(0.999),
            waits[waits.len() - 1]
        );
    }

    #[test]
    fn motion_is_summed_and_checked_at_most_every_8_ms() {
        let start = std::time::Instant::now();
        let mut throttle = Throttle {
            checked: start,
            sum: (0, 0),
        };
        let ms = |n| start + std::time::Duration::from_millis(n);
        // A 1000 Hz mouse moving right: no question for the first 7 reports.
        for n in 1..8 {
            assert_eq!(throttle.feed_at((1, 0), ms(n)), None);
        }
        assert_eq!(
            throttle.feed_at((1, 0), ms(8)),
            Some((8, 0)),
            "summed, then asked"
        );
        assert_eq!(throttle.feed_at((0, 2), ms(9)), None);
        assert_eq!(throttle.feed_at((0, 0), ms(20)), Some((0, 2)));
        assert_eq!(
            throttle.feed_at((0, 0), ms(40)),
            None,
            "no motion, no question"
        );
    }

    use super::*;

    const LAPTOP: [Monitor; 1] = [Monitor {
        x: 0,
        y: 0,
        width: 1536,
        height: 864,
    }];
    /// The owner's Windows PC, with the laptop where they placed it: left
    /// of the main monitor, bottoms lined up.
    const WINDOWS: [Monitor; 3] = [
        Monitor {
            x: 0,
            y: 0,
            width: 2560,
            height: 1080,
        },
        Monitor {
            x: 246,
            y: -1080,
            width: 1920,
            height: 1080,
        },
        Monitor {
            x: 2560,
            y: -845,
            width: 1080,
            height: 1920,
        },
    ];
    const LAPTOP_AT: [i32; 2] = [-1536, 216];

    #[test]
    fn only_a_push_off_the_desktop_counts() {
        assert_eq!(beyond(&WINDOWS, (0, 500), (-3, 0)), Some((-1, 500)));
        assert_eq!(beyond(&WINDOWS, (0, 500), (3, 0)), None);
        assert_eq!(beyond(&WINDOWS, (1, 500), (-3, 0)), None);
        // Down along the left edge is not leaving through it.
        assert_eq!(beyond(&WINDOWS, (0, 500), (0, 4)), None);
        // The main monitor's right edge touches the sideways monitor.
        assert_eq!(beyond(&WINDOWS, (2559, 500), (5, 0)), None);
    }

    #[test]
    fn across_where_the_desktops_touch_and_back() {
        // Windows' left edge at y = 500 is the laptop's right edge, 284 down.
        let there = onto(&LAPTOP, LAPTOP_AT, (-1, 500)).unwrap();
        assert_eq!(there, (1535, 284));
        // Pushing right from there comes back to the same place.
        let off = beyond(&LAPTOP, there, (2, 0)).unwrap();
        assert_eq!(back(&WINDOWS, LAPTOP_AT, off), Some((0, 500)));
    }

    #[test]
    fn from_one_peer_onto_the_peer_next_to_it() {
        // Two laptops side by side below Windows' main monitor.
        let left_at = [0, 1080];
        let right_at = [1536, 1080];
        // Pushing right off the left laptop, 400 px down its edge.
        let off = beyond(&LAPTOP, (1535, 400), (3, 0)).unwrap();
        // Not on Windows: the cursor does not go home.
        assert_eq!(back(&WINDOWS, left_at, off), None);
        // On the right laptop's left edge, at the same height.
        let here = in_layout(left_at, off);
        assert_eq!(onto(&LAPTOP, right_at, here), Some((0, 400)));
    }

    #[test]
    fn where_the_desktops_do_not_touch_nothing_happens() {
        // Left of the main monitor but above the laptop's top (y = 216).
        assert_eq!(onto(&LAPTOP, LAPTOP_AT, (-1, 100)), None);
        // Pushing up off the laptop: nothing of Windows is above it.
        let off = beyond(&LAPTOP, (100, 0), (0, -1)).unwrap();
        assert_eq!(back(&WINDOWS, LAPTOP_AT, off), None);
    }

    #[test]
    fn the_edge_check_follows_moved_monitors() {
        // The cursor rests on the right edge of a 1536-wide laptop.
        fn on_the_right_edge() -> Option<(i32, i32)> {
            Some((1535, 400))
        }
        let watch = EdgeWatch::start(LAPTOP.to_vec(), on_the_right_edge);
        let answer = || {
            let until = std::time::Instant::now() + std::time::Duration::from_secs(1);
            loop {
                if let Some(off) = watch.answer() {
                    return off;
                }
                assert!(std::time::Instant::now() < until, "no answer");
                std::thread::yield_now();
            }
        };
        watch.push((5, 0));
        assert_eq!(answer(), Some((1536, 400)), "the pixel just off the laptop");
        // A second monitor plugged in to the right: the same push stays here.
        watch.set_monitors(vec![LAPTOP[0], m(1536, 0, 1920, 1080)]);
        watch.push((5, 0));
        assert_eq!(answer(), None, "onto the new monitor");
    }
}
