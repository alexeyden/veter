// Two progress UIs sharing a `ProgressUI` trait:
//
//   * `VgeProgress` — draws a single VGE element (background rect,
//     foreground rect that grows, and a centred status text) on the
//     terminal grid. Updates via `UpdateCommand` so each tick costs
//     only the changed command body, not the whole element.
//
//   * `AsciiProgress` — carriage-return-driven fallback for terminals
//     that did not advertise VGE in their probe response.
//
// Both write directly to stdout / stderr; the caller is responsible
// for any raw-TTY mode (writes use `\r\n` to behave correctly under
// raw mode, which still works under cooked mode).

use std::io::Write;
use std::time::Instant;

use anyhow::Result;

use vge_protocol::codec::{Point, Rect};
use vge_protocol::command::{
    Align, Color, Command, CreateElementBody, DrawCmd, FontStyle, Style, UpdateCommandBody,
};
use vge_protocol::encode::build_envelope;

const TRACK_RGBA: Color = Color {
    r: 0.20,
    g: 0.22,
    b: 0.25,
    a: 1.0,
};
const FILL_RGBA: Color = Color {
    r: 0.30,
    g: 0.62,
    b: 1.00,
    a: 1.0,
};
const TEXT_RGBA: Color = Color {
    r: 0.97,
    g: 0.97,
    b: 0.97,
    a: 1.0,
};

// Document indices into the element's command list. Only FG and TEXT
// receive UpdateCommand calls; the background never moves, but the
// constant is here to make the layout obvious.
#[allow(dead_code)]
const CMD_IDX_BG: usize = 0;
const CMD_IDX_FG: usize = 1;
const CMD_IDX_TEXT: usize = 2;

pub trait ProgressUI {
    fn start(&mut self) -> Result<()>;
    /// `current` / `total` in bytes, `rate_bps` in bytes per second.
    /// `total = 0` means "unknown size" — the implementation should
    /// fall back to a count-only display.
    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()>;
    /// Tear down the UI (e.g. delete the VGE element) and print
    /// `final_line` on its own row at the cursor.
    fn finish(&mut self, final_line: &str) -> Result<()>;
}

// ---- VGE progress ----------------------------------------------------

pub struct VgeProgress {
    element_id: String,
    /// Row at which the bar is drawn (0-indexed for VGE, derived from
    /// the 1-indexed DSR-CPR result minus one).
    origin_y: f32,
    bar_w: f32,
    label: String,
    last_render: Option<Instant>,
}

impl VgeProgress {
    pub fn new(element_id: String, label: String, cursor_row_1based: u32, term_cols: u32) -> Self {
        let bar_w = bar_width_cells(term_cols);
        Self {
            element_id,
            origin_y: cursor_row_1based.saturating_sub(1) as f32,
            bar_w: bar_w as f32,
            label,
            last_render: None,
        }
    }

    fn render_text(&self, current: u64, total: u64, rate_bps: f64) -> String {
        format_status(&self.label, current, total, rate_bps)
    }

    fn write_envelope(&self, env: &[u8]) -> Result<()> {
        let mut out = std::io::stdout().lock();
        out.write_all(env)?;
        out.flush()?;
        Ok(())
    }
}

impl ProgressUI for VgeProgress {
    fn start(&mut self) -> Result<()> {
        let bg = DrawCmd::FillRectangles {
            fill: Style::Flat(TRACK_RGBA),
            rects: vec![Rect {
                x: 0.0,
                y: 0.0,
                w: self.bar_w,
                h: 1.0,
            }],
        };
        let fg = DrawCmd::FillRectangles {
            fill: Style::Flat(FILL_RGBA),
            rects: vec![Rect {
                x: 0.0,
                y: 0.0,
                w: 0.0,
                h: 1.0,
            }],
        };
        let text = DrawCmd::DrawText {
            origin: Point {
                x: self.bar_w / 2.0,
                y: 0.78,
            },
            align: Align::Center,
            fill: Style::Flat(TEXT_RGBA),
            font_style: FontStyle::default(),
            text: self.render_text(0, 0, 0.0),
        };
        let create = Command::CreateElement(CreateElementBody {
            id: self.element_id.clone(),
            commands: vec![bg, fg, text],
            origin: Point {
                x: 0.0,
                y: self.origin_y,
            },
            is_visible: true,
            draw_order: 0,
            parent: None,
            size: None,
        });
        let env = build_envelope(&[(create, 0)]);
        self.write_envelope(&env)?;
        // Move the cursor below the bar so subsequent stdout doesn't
        // overlap. \r\n covers raw + cooked modes.
        let mut out = std::io::stdout().lock();
        out.write_all(b"\r\n")?;
        out.flush()?;
        Ok(())
    }

    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()> {
        // Throttle to ~30 Hz so we don't flood the PTY with envelopes.
        let now = Instant::now();
        if let Some(prev) = self.last_render {
            if now.duration_since(prev).as_millis() < 33 && current < total {
                return Ok(());
            }
        }
        self.last_render = Some(now);

        let frac = if total == 0 {
            0.0
        } else {
            (current as f32 / total as f32).clamp(0.0, 1.0)
        };
        let fill_w = self.bar_w * frac;
        let fg_cmd = DrawCmd::FillRectangles {
            fill: Style::Flat(FILL_RGBA),
            rects: vec![Rect {
                x: 0.0,
                y: 0.0,
                w: fill_w,
                h: 1.0,
            }],
        };
        let text_cmd = DrawCmd::DrawText {
            origin: Point {
                x: self.bar_w / 2.0,
                y: 0.78,
            },
            align: Align::Center,
            fill: Style::Flat(TEXT_RGBA),
            font_style: FontStyle::default(),
            text: self.render_text(current, total, rate_bps),
        };
        let upd_fg = Command::UpdateCommand(UpdateCommandBody {
            id: self.element_id.clone(),
            index: CMD_IDX_FG,
            command: fg_cmd,
        });
        let upd_text = Command::UpdateCommand(UpdateCommandBody {
            id: self.element_id.clone(),
            index: CMD_IDX_TEXT,
            command: text_cmd,
        });
        let env = build_envelope(&[(upd_fg, 0), (upd_text, 0)]);
        self.write_envelope(&env)
    }

    fn finish(&mut self, final_line: &str) -> Result<()> {
        let del = Command::DeleteElement {
            id: self.element_id.clone(),
        };
        let env = build_envelope(&[(del, 0)]);
        self.write_envelope(&env)?;
        let mut out = std::io::stdout().lock();
        write!(out, "{final_line}\r\n")?;
        out.flush()?;
        Ok(())
    }
}

// ---- ASCII progress ---------------------------------------------------

pub struct AsciiProgress {
    label: String,
    bar_w: usize,
    last_render: Option<Instant>,
    last_line_len: usize,
}

impl AsciiProgress {
    pub fn new(label: String, term_cols: u32) -> Self {
        Self {
            label,
            bar_w: bar_width_cells(term_cols).clamp(20, 60) as usize,
            last_render: None,
            last_line_len: 0,
        }
    }
}

impl ProgressUI for AsciiProgress {
    fn start(&mut self) -> Result<()> {
        Ok(())
    }

    fn update(&mut self, current: u64, total: u64, rate_bps: f64) -> Result<()> {
        let now = Instant::now();
        if let Some(prev) = self.last_render {
            if now.duration_since(prev).as_millis() < 100 && current < total {
                return Ok(());
            }
        }
        self.last_render = Some(now);

        let bar = render_ascii_bar(current, total, self.bar_w);
        let stats = format_status(&self.label, current, total, rate_bps);
        let line = format!("{bar} {stats}");
        // Carriage-return + erase-to-EOL so a shorter line cleanly
        // overwrites the previous one.
        let mut err = std::io::stderr().lock();
        write!(err, "\r{line}\x1b[K")?;
        err.flush()?;
        self.last_line_len = line.len();
        Ok(())
    }

    fn finish(&mut self, final_line: &str) -> Result<()> {
        let mut err = std::io::stderr().lock();
        write!(err, "\r\x1b[K")?;
        err.flush()?;
        let mut out = std::io::stdout().lock();
        writeln!(out, "{final_line}")?;
        out.flush()?;
        Ok(())
    }
}

// ---- shared helpers ---------------------------------------------------

fn bar_width_cells(term_cols: u32) -> u32 {
    // Leave a 2-cell margin on each side; clamp to a maximum that
    // keeps the bar comfortable on wide screens.
    let usable = term_cols.saturating_sub(4);
    usable.clamp(20, 80)
}

fn render_ascii_bar(current: u64, total: u64, width: usize) -> String {
    let frac = if total == 0 {
        0.0
    } else {
        (current as f64 / total as f64).clamp(0.0, 1.0)
    };
    let filled = (frac * width as f64).round() as usize;
    let mut s = String::with_capacity(width + 2);
    s.push('[');
    for i in 0..width {
        if i < filled {
            s.push('#');
        } else {
            s.push('.');
        }
    }
    s.push(']');
    s
}

fn format_status(label: &str, current: u64, total: u64, rate_bps: f64) -> String {
    let pct = if total == 0 {
        String::new()
    } else {
        let p = (current as f64 / total as f64 * 100.0).clamp(0.0, 100.0);
        format!("{p:>5.1}%  ")
    };
    let sizes = if total == 0 {
        format_bytes(current)
    } else {
        format!("{} / {}", format_bytes(current), format_bytes(total))
    };
    let rate = if rate_bps > 0.0 {
        format!("  {}", format_rate(rate_bps))
    } else {
        String::new()
    };
    format!("{label}  {pct}{sizes}{rate}")
}

fn format_bytes(b: u64) -> String {
    let f = b as f64;
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    const GIB: f64 = 1024.0 * 1024.0 * 1024.0;
    if f >= GIB {
        format!("{:.2} GiB", f / GIB)
    } else if f >= MIB {
        format!("{:.1} MiB", f / MIB)
    } else if f >= KIB {
        format!("{:.1} KiB", f / KIB)
    } else {
        format!("{b} B")
    }
}

fn format_rate(bps: f64) -> String {
    const KIB: f64 = 1024.0;
    const MIB: f64 = 1024.0 * 1024.0;
    if bps >= MIB {
        format!("{:.1} MiB/s", bps / MIB)
    } else if bps >= KIB {
        format!("{:.1} KiB/s", bps / KIB)
    } else {
        format!("{bps:.0} B/s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ascii_bar_rounds_correctly() {
        let s = render_ascii_bar(50, 100, 10);
        assert_eq!(s, "[#####.....]");
    }

    #[test]
    fn ascii_bar_zero_progress() {
        assert_eq!(render_ascii_bar(0, 100, 10), "[..........]");
    }

    #[test]
    fn ascii_bar_complete() {
        assert_eq!(render_ascii_bar(100, 100, 10), "[##########]");
    }

    #[test]
    fn ascii_bar_unknown_total() {
        // total = 0 short-circuits to 0% rendering.
        assert_eq!(render_ascii_bar(123, 0, 5), "[.....]");
    }

    #[test]
    fn format_bytes_picks_unit() {
        assert_eq!(format_bytes(0), "0 B");
        assert_eq!(format_bytes(1023), "1023 B");
        assert_eq!(format_bytes(2048), "2.0 KiB");
        assert_eq!(format_bytes(2 * 1024 * 1024), "2.0 MiB");
        assert_eq!(format_bytes(2 * 1024 * 1024 * 1024), "2.00 GiB");
    }

    #[test]
    fn format_status_includes_pct_when_total_known() {
        let s = format_status("vsend: foo", 50, 100, 1024.0 * 1024.0);
        assert!(s.contains("50.0%"));
        assert!(s.contains("MiB"));
    }

    #[test]
    fn format_status_omits_pct_when_total_unknown() {
        let s = format_status("vsend: foo", 50, 0, 0.0);
        assert!(!s.contains('%'));
    }

    #[test]
    fn bar_width_clamps_to_minimum() {
        assert_eq!(bar_width_cells(10), 20);
    }

    #[test]
    fn bar_width_clamps_to_maximum() {
        assert_eq!(bar_width_cells(500), 80);
    }
}
