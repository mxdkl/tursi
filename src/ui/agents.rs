//! The agent block: a quiet pane beside the chat where every working
//! subagent lights one braille dot, colored by its kind and twinkling a
//! little. Unlit dots are invisible. Agents fill the block in reading order
//! — from the top-left cell rightward, then down — and agents of the same
//! kind share a cell, up to its eight dots (a terminal colors whole cells,
//! so different kinds never share one). A dot keeps its place for the
//! agent's whole life and fades when the agent finishes; a freed dot goes to
//! the next agent of a fitting kind. Nothing else about the agents is shown:
//! the lead's chat is the interface; the block says how much is happening.
//! A dim frame titled "agents" keeps the pane from looking empty when idle.

use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders};
use std::collections::BTreeMap;
use std::time::Instant;

use crate::bus::EventKind;

/// Finished agents kept in memory (a follow-up relights the same dot).
const KEEP_FINISHED: usize = 24;
/// How long a finished agent's dot takes to fade out.
const FADE_SECS: f32 = 1.5;

pub struct Tile {
    pub id: u32,
    pub access: String,
    pub ended: Option<Instant>,
    /// Cell index in reading order, and the dot (0..8) within that cell.
    pub slot: (usize, u8),
}

impl Tile {
    pub fn running(&self) -> bool {
        self.ended.is_none()
    }

    /// Still holding its dot: working, or finished and fading.
    fn lit(&self, now: Instant) -> bool {
        match self.ended {
            None => true,
            Some(at) => now.duration_since(at).as_secs_f32() < FADE_SECS,
        }
    }
}

/// The first free dot in reading order for an agent of `access`: an empty
/// cell, or a cell already lit by the same kind with a dot to spare.
fn first_free(tiles: &[Tile], access: &str, except: u32, now: Instant) -> (usize, u8) {
    let mut cells: BTreeMap<usize, (&str, u8)> = BTreeMap::new();
    for t in tiles.iter().filter(|t| t.id != except && t.lit(now)) {
        let e = cells.entry(t.slot.0).or_insert((&t.access, 0));
        e.1 |= 1 << t.slot.1;
    }
    for cell in 0.. {
        match cells.get(&cell) {
            None => return (cell, 0),
            Some((kind, used)) if *kind == access && *used != 0xff => return (cell, used.trailing_ones() as u8),
            Some(_) => {}
        }
    }
    unreachable!("cells are unbounded")
}

/// Is `slot` free for an agent of `access` (no other lit agent on that dot,
/// and its cell isn't lit by another kind)?
fn slot_free(tiles: &[Tile], access: &str, slot: (usize, u8), except: u32, now: Instant) -> bool {
    tiles.iter().filter(|t| t.id != except && t.lit(now) && t.slot.0 == slot.0).all(|t| t.access == access && t.slot.1 != slot.1)
}

/// Fold one subagent event into the block: a start lights (or relights) the
/// agent's dot, a finish starts its fade. Other events don't change the view.
pub fn apply(tiles: &mut Vec<Tile>, id: u32, kind: EventKind) {
    let now = Instant::now();
    match kind {
        EventKind::SubagentStarted { access, .. } => {
            match tiles.iter().position(|t| t.id == id) {
                Some(i) => {
                    let (r, slot) = (tiles[i].access.clone(), tiles[i].slot);
                    let slot = if slot_free(tiles, &r, slot, id, now) { slot } else { first_free(tiles, &r, id, now) };
                    tiles[i].slot = slot;
                    tiles[i].ended = None;
                }
                None => {
                    let slot = first_free(tiles, &access, id, now);
                    tiles.push(Tile { id, access, ended: None, slot });
                }
            }
            prune(tiles);
        }
        EventKind::SubagentFinished { .. } => {
            if let Some(tile) = tiles.iter_mut().find(|t| t.id == id) {
                tile.ended = Some(now);
            }
        }
        // Activity from an agent the block hasn't seen start (a resumed
        // session's child) still lights a dot.
        _ if !tiles.iter().any(|t| t.id == id) => {
            let slot = first_free(tiles, "agent", id, now);
            tiles.push(Tile { id, access: "agent".into(), ended: None, slot });
        }
        _ => {}
    }
}

fn prune(tiles: &mut Vec<Tile>) {
    let finished = tiles.iter().filter(|t| !t.running()).count();
    if finished > KEEP_FINISHED {
        let mut drop = finished - KEEP_FINISHED;
        tiles.retain(|t| {
            if drop > 0 && !t.running() {
                drop -= 1;
                false
            } else {
                true
            }
        });
    }
}

pub fn steps(n: usize) -> String {
    if n == 1 { "1 step".to_string() } else { format!("{n} steps") }
}

/// Readers and writers have their own colors.
pub fn access_rgb(access: &str) -> (u8, u8, u8) {
    match access {
        "reader" => (90, 150, 255),
        "writer" => (80, 220, 120),
        _ => (90, 210, 220),
    }
}

fn access_label(access: &str) -> &str {
    access
}

/// Bit for dot `i` of a cell, numbered in reading order: left-right, then
/// down the cell's four rows.
fn dot_bit(i: u8) -> u8 {
    const BITS: [u8; 8] = [0x01, 0x08, 0x02, 0x10, 0x04, 0x20, 0x40, 0x80];
    BITS[i as usize & 7]
}

fn mix(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51afd7ed558ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ceb9fe1a85ec53);
    x ^ (x >> 33)
}

/// Brightness 0..=1 of an agent's dot at time t: steady with a soft twinkle,
/// each agent on its own phase; fading after it finishes.
fn twinkle(tile: &Tile, t: f32, now: Instant) -> f32 {
    let phase = (mix(tile.id as u64) >> 40) as f32 / (1u64 << 24) as f32 * std::f32::consts::TAU;
    let shimmer = 0.82 + 0.18 * (t * 2.3 + phase).sin() * (t * 0.9 + phase * 1.7).sin().abs();
    match tile.ended {
        None => shimmer,
        Some(at) => (1.0 - now.duration_since(at).as_secs_f32() / FADE_SECS).max(0.0) * shimmer,
    }
}

fn scale((r, g, b): (u8, u8, u8), k: f32) -> Color {
    let f = |c: u8| (c as f32 * k).round().clamp(0.0, 255.0) as u8;
    Color::Rgb(f(r), f(g), f(b))
}

/// Paint the block into `area`: a dim frame titled "agents", the lit dots
/// inside it, and a legend of who is working on the bottom edge. `t` is
/// seconds on a monotonic clock.
pub fn draw(frame: &mut Frame, area: Rect, tiles: &[Tile], t: f32) {
    let mut counts: Vec<(&str, usize)> = Vec::new();
    for tile in tiles.iter().filter(|t| t.running()) {
        match counts.iter_mut().find(|(r, _)| *r == tile.access) {
            Some((_, n)) => *n += 1,
            None => counts.push((&tile.access, 1)),
        }
    }
    counts.sort();
    let mut legend = vec![Span::raw(" ")];
    for (access, n) in &counts {
        let (r, g, b) = access_rgb(access);
        legend.push(Span::styled("● ", Style::new().fg(Color::Rgb(r, g, b))));
        legend.push(Span::styled(format!("{n} {}{} ", access_label(access), if *n == 1 { "" } else { "s" }), Style::new().fg(Color::Gray)));
    }
    let mut frame_block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(" agents ", Style::new().fg(Color::Gray)));
    if !counts.is_empty() {
        frame_block = frame_block.title_bottom(Line::from(legend).right_aligned());
    }
    let inner = frame_block.inner(area);
    frame.render_widget(frame_block, area);
    render(frame.buffer_mut(), inner, tiles, t, Instant::now());
}

fn render(buf: &mut Buffer, area: Rect, tiles: &[Tile], t: f32, now: Instant) {
    let (cols, rows) = (area.width as usize, area.height as usize);
    if cols == 0 || rows == 0 {
        return;
    }
    // Lit cells: the kind, its dots, and the brightest of its agents.
    let mut cells: BTreeMap<usize, (&str, u8, f32)> = BTreeMap::new();
    for tile in tiles.iter().filter(|t| t.lit(now)) {
        let e = cells.entry(tile.slot.0).or_insert((&tile.access, 0, 0.0));
        e.1 |= dot_bit(tile.slot.1);
        e.2 = e.2.max(twinkle(tile, t, now));
    }
    for (cell, (access, bits, k)) in cells {
        if cell >= cols * rows || k <= 0.0 {
            continue;
        }
        let pos = (area.x + (cell % cols) as u16, area.y + (cell / cols) as u16);
        let style = Style::new().fg(scale(access_rgb(access), k)).add_modifier(Modifier::BOLD);
        buf[pos].set_char(char::from_u32(0x2800 + bits as u32).unwrap_or(' ')).set_style(style);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn start(tiles: &mut Vec<Tile>, id: u32, access: &str) {
        apply(tiles, id, EventKind::SubagentStarted { access: access.into(), title: "t".into(), model: "m".into(), continued: false });
    }

    fn slot(tiles: &[Tile], id: u32) -> (usize, u8) {
        tiles.iter().find(|t| t.id == id).unwrap().slot
    }

    #[test]
    fn agents_fill_in_reading_order_and_kinds_share_cells() {
        let mut tiles = Vec::new();
        start(&mut tiles, 1, "writer");
        start(&mut tiles, 2, "writer");
        start(&mut tiles, 3, "reader");
        start(&mut tiles, 4, "writer");
        start(&mut tiles, 5, "agent");
        assert_eq!(slot(&tiles, 1), (0, 0), "first agent: top-left cell, first dot");
        assert_eq!(slot(&tiles, 2), (0, 1), "same kind shares the cell");
        assert_eq!(slot(&tiles, 3), (1, 0), "another kind takes the next cell");
        assert_eq!(slot(&tiles, 4), (0, 2), "writers keep packing into their cell");
        assert_eq!(slot(&tiles, 5), (2, 0));
        for id in 6..=10 {
            start(&mut tiles, id, "writer");
        }
        // Writers 1, 2, 4 and 6..=10 are eight writers: one full cell.
        assert_eq!(slot(&tiles, 10), (0, 7), "eight writers fill a cell");
        start(&mut tiles, 11, "writer");
        assert_eq!(slot(&tiles, 11), (3, 0), "the ninth starts the next free cell");
        // No cell ever holds two kinds.
        let mut kinds: BTreeMap<usize, &str> = BTreeMap::new();
        for t in &tiles {
            assert_eq!(*kinds.entry(t.slot.0).or_insert(&t.access), t.access.as_str());
        }
    }

    #[test]
    fn a_freed_dot_goes_to_the_next_agent_of_its_kind() {
        let mut tiles = Vec::new();
        start(&mut tiles, 1, "writer");
        start(&mut tiles, 2, "writer");
        start(&mut tiles, 3, "reader");
        apply(&mut tiles, 1, EventKind::SubagentFinished { ok: true });
        // Still fading: its dot is held.
        start(&mut tiles, 4, "writer");
        assert_eq!(slot(&tiles, 4), (0, 2));
        // Past the fade, the dot is free again.
        tiles.iter_mut().find(|t| t.id == 1).unwrap().ended = Some(Instant::now() - Duration::from_secs(5));
        start(&mut tiles, 5, "writer");
        assert_eq!(slot(&tiles, 5), (0, 0));
        // The reader's cell frees up for anyone once it's gone.
        apply(&mut tiles, 3, EventKind::SubagentFinished { ok: true });
        tiles.iter_mut().find(|t| t.id == 3).unwrap().ended = Some(Instant::now() - Duration::from_secs(5));
        start(&mut tiles, 6, "agent");
        assert_eq!(slot(&tiles, 6), (1, 0));
        // A follow-up keeps its old dot when it's still free.
        apply(&mut tiles, 2, EventKind::SubagentFinished { ok: true });
        apply(&mut tiles, 2, EventKind::SubagentStarted { access: "writer".into(), title: "t".into(), model: "m".into(), continued: true });
        assert_eq!(slot(&tiles, 2), (0, 1));
    }

    #[test]
    fn braille_bits_follow_the_unicode_layout() {
        assert_eq!(char::from_u32(0x2800 + dot_bit(0) as u32), Some('⠁'), "top-left");
        assert_eq!(char::from_u32(0x2800 + dot_bit(1) as u32), Some('⠈'), "top-right");
        assert_eq!(char::from_u32(0x2800 + dot_bit(7) as u32), Some('⢀'), "bottom-right");
        assert_eq!((0..8).map(dot_bit).fold(0u8, |a, b| a | b), 0xff);
    }

    #[test]
    fn the_block_is_invisible_except_for_lit_dots() {
        let area = Rect::new(0, 0, 10, 4);
        let now = Instant::now();
        let tiles = vec![
            Tile { id: 1, access: "writer".into(), ended: None, slot: (0, 0) },
            Tile { id: 2, access: "writer".into(), ended: None, slot: (0, 1) },
            Tile { id: 3, access: "reader".into(), ended: None, slot: (1, 0) },
            Tile { id: 4, access: "agent".into(), ended: Some(now - Duration::from_secs(5)), slot: (2, 0) },
        ];
        let mut buf = Buffer::empty(area);
        render(&mut buf, area, &tiles, 3.0, now);
        assert_eq!(buf[(0, 0)].symbol(), "⠉", "two writers share the top-left cell");
        assert!(matches!(buf[(0, 0)].fg, Color::Rgb(r, g, b) if g > r && g > b), "green");
        assert_eq!(buf[(1, 0)].symbol(), "⠁");
        assert!(matches!(buf[(1, 0)].fg, Color::Rgb(r, g, b) if b > r && b > g), "blue");
        let lit = buf.content().iter().filter(|c| c.symbol() != " ").count();
        assert_eq!(lit, 2, "the faded agent and every unlit cell are blank");
        // Nothing moves with no agents.
        let (mut a, mut b) = (Buffer::empty(area), Buffer::empty(area));
        render(&mut a, area, &[], 1.0, now);
        render(&mut b, area, &[], 9.0, now);
        assert_eq!(a, b);
    }

    #[test]
    fn twinkling_varies_but_stays_bright() {
        let tile = Tile { id: 4, access: "writer".into(), ended: None, slot: (0, 0) };
        let now = Instant::now();
        let ks: Vec<f32> = (0..40).map(|i| twinkle(&tile, i as f32 * 0.25, now)).collect();
        let (lo, hi) = ks.iter().fold((f32::MAX, f32::MIN), |(lo, hi), &k| (lo.min(k), hi.max(k)));
        assert!(lo >= 0.6 && hi <= 1.0 && hi - lo > 0.05, "{lo}..{hi}");
    }
}
