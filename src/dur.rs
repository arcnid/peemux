use std::collections::HashMap;
use std::io::Read;

use anyhow::Result;
use flate2::read::GzDecoder;
use serde::Deserialize;

// ─── embedded .dur assets ──────────────────────────────────────────────────
const IDLE_DUR: &[u8] = include_bytes!("../art/idle.dur");
const WAVE_DUR: &[u8] = include_bytes!("../art/wave.dur");
const HAPPY_DUR: &[u8] = include_bytes!("../art/happy.dur");
const SAD_DUR: &[u8] = include_bytes!("../art/sad.dur");
const CRANKY_DUR: &[u8] = include_bytes!("../art/cranky.dur");
const MAD_DUR: &[u8] = include_bytes!("../art/mad.dur");
const SLEEPY_DUR: &[u8] = include_bytes!("../art/sleepy.dur");
const EATING_DUR: &[u8] = include_bytes!("../art/eating.dur");
const DEAD_DUR: &[u8] = include_bytes!("../art/dead.dur");

// ─── parsed types ──────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct DurCell {
    pub ch: char,
    pub fg: u8,
}

#[derive(Clone)]
pub struct DurFrame {
    pub delay_ms: u64,
    pub cells: Vec<Vec<DurCell>>, // [row][col]
}

#[derive(Clone)]
pub struct DurMovie {
    pub width: usize,
    pub height: usize,
    pub frames: Vec<DurFrame>,
}

// ─── JSON deserialization (matches durdraw's gzipped format) ───────────────

#[derive(Deserialize)]
struct RawDurFile {
    #[serde(rename = "DurMovie")]
    dur_movie: RawDurMovie,
}

#[derive(Deserialize)]
struct RawDurMovie {
    #[serde(rename = "sizeX")]
    size_x: usize,
    #[serde(rename = "sizeY")]
    size_y: usize,
    frames: Vec<RawFrame>,
}

#[derive(Deserialize)]
struct RawFrame {
    delay: Option<f64>,
    contents: Vec<String>,
    #[serde(rename = "colorMap")]
    color_map: Vec<Vec<Vec<u8>>>, // [x][y][fg, bg] — COLUMN-MAJOR
}

impl DurMovie {
    pub fn from_gzipped(data: &[u8]) -> Result<Self> {
        let mut decoder = GzDecoder::new(data);
        let mut json = String::new();
        decoder.read_to_string(&mut json)?;
        let raw: RawDurFile = serde_json::from_str(&json)?;
        let m = raw.dur_movie;

        let frames = m
            .frames
            .into_iter()
            .map(|f| {
                let delay_ms = ((f.delay.unwrap_or(0.1)) * 1000.0) as u64;
                let mut cells = Vec::with_capacity(m.size_y);
                for y in 0..m.size_y {
                    let mut row = Vec::with_capacity(m.size_x);
                    for x in 0..m.size_x {
                        let ch = f
                            .contents
                            .get(y)
                            .and_then(|s| s.chars().nth(x))
                            .unwrap_or(' ');
                        // colorMap is column-major: [x][y][0] = fg
                        let fg = f
                            .color_map
                            .get(x)
                            .and_then(|col| col.get(y))
                            .and_then(|pair| pair.first().copied())
                            .unwrap_or(0);
                        row.push(DurCell { ch, fg });
                    }
                    cells.push(row);
                }
                DurFrame { delay_ms, cells }
            })
            .collect();

        Ok(DurMovie {
            width: m.size_x,
            height: m.size_y,
            frames,
        })
    }
}

// ─── animation player ──────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Mood {
    Idle,
    Wave,
    Happy,
    Sad,
    Cranky,
    Mad,
    Sleepy,
    Eating,
    Dead,
}

#[derive(Clone, Copy, PartialEq)]
pub enum AnimMode {
    Loop,
    Oneshot,
}

pub struct DurPlayer {
    movies: HashMap<Mood, DurMovie>,
    pub current: Mood,
    pub frame: usize,
    pub mode: AnimMode,
    elapsed_ms: u64,
    default_mood: Mood,
}

impl DurPlayer {
    pub fn new() -> Result<Self> {
        let mut movies = HashMap::new();
        let entries: &[(Mood, &[u8])] = &[
            (Mood::Idle, IDLE_DUR),
            (Mood::Wave, WAVE_DUR),
            (Mood::Happy, HAPPY_DUR),
            (Mood::Sad, SAD_DUR),
            (Mood::Cranky, CRANKY_DUR),
            (Mood::Mad, MAD_DUR),
            (Mood::Sleepy, SLEEPY_DUR),
            (Mood::Eating, EATING_DUR),
            (Mood::Dead, DEAD_DUR),
        ];
        for (mood, data) in entries {
            movies.insert(*mood, DurMovie::from_gzipped(data)?);
        }
        Ok(DurPlayer {
            movies,
            current: Mood::Idle,
            frame: 0,
            mode: AnimMode::Loop,
            elapsed_ms: 0,
            default_mood: Mood::Idle,
        })
    }

    pub fn set_anim(&mut self, mood: Mood, mode: AnimMode) {
        if !self.movies.contains_key(&mood) {
            return;
        }
        self.current = mood;
        self.frame = 0;
        self.mode = mode;
        self.elapsed_ms = 0;
    }

    pub fn set_default_mood(&mut self, mood: Mood) {
        self.default_mood = mood;
        if self.mode == AnimMode::Loop && self.current != mood {
            self.set_anim(mood, AnimMode::Loop);
        }
    }

    /// Advance the animation by `dt_ms` milliseconds.
    /// Returns true if the frame changed (caller should mark dirty).
    pub fn tick(&mut self, dt_ms: u64) -> bool {
        let movie = match self.movies.get(&self.current) {
            Some(m) => m,
            None => return false,
        };
        if movie.frames.is_empty() {
            return false;
        }

        self.elapsed_ms += dt_ms;
        let delay = movie.frames[self.frame].delay_ms.max(16);
        if self.elapsed_ms < delay {
            return false;
        }

        self.elapsed_ms -= delay;
        let next = self.frame + 1;
        if next < movie.frames.len() {
            self.frame = next;
        } else if self.mode == AnimMode::Oneshot {
            self.set_anim(self.default_mood, AnimMode::Loop);
        } else {
            self.frame = 0;
        }
        true
    }

    pub fn current_frame(&self) -> Option<&DurFrame> {
        self.movies
            .get(&self.current)
            .and_then(|m| m.frames.get(self.frame))
    }
}

// ─── color mapping ─────────────────────────────────────────────────────────
// Durdraw's 1-indexed 16-color palette → ratatui Color.
// Derived from build_dur.py FG_* constants and tui.js FG_SGR mapping.

use ratatui::style::Color;

pub fn dur_to_color(idx: u8) -> Color {
    match idx {
        1 => Color::Indexed(0),   // black
        2 => Color::Indexed(4),   // blue
        3 => Color::Indexed(2),   // green
        4 => Color::Indexed(6),   // cyan
        5 => Color::Indexed(1),   // red
        6 => Color::Indexed(5),   // magenta
        7 => Color::Indexed(3),   // yellow
        8 => Color::Indexed(7),   // white/grey
        9 => Color::Indexed(8),   // dark grey
        10 => Color::Reset,       // sky → transparent (not blue — let terminal bg show)
        11 => Color::Indexed(10), // bright green
        12 => Color::Indexed(14), // bright cyan
        13 => Color::Indexed(9),  // bright red
        14 => Color::Indexed(13), // bright magenta
        15 => Color::Indexed(11), // bright yellow
        16 => Color::Indexed(15), // bright white
        _ => Color::Reset,
    }
}

/// Whether a cell is "visible" (has a drawn pixel vs transparent space).
pub fn is_visible(cell: &DurCell) -> bool {
    cell.ch != ' ' && cell.fg != 0
}
