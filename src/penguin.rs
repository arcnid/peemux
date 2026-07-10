use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::widgets::Widget;

use crate::dur::{dur_to_color, is_visible, DurFrame};

/// Renders a durdraw frame using half-block characters (▀ / ▄) to pack
/// two vertical pixels per terminal row. A 32×16 canvas becomes 32×8.
pub struct PenguinWidget<'a> {
    pub frame: &'a DurFrame,
}

impl Widget for PenguinWidget<'_> {
    fn render(self, area: Rect, buf: &mut Buffer) {
        let canvas_w = 32usize;
        let canvas_h = 16usize;
        let term_rows = canvas_h / 2; // 8

        let x_offset = area.x + area.width.saturating_sub(canvas_w as u16) / 2;

        for tr in 0..term_rows {
            if tr as u16 >= area.height {
                break;
            }
            let top_y = tr * 2;
            let bot_y = top_y + 1;

            for x in 0..canvas_w.min(area.width as usize) {
                let bx = x_offset + x as u16;
                let by = area.y + tr as u16;
                if bx >= area.x + area.width || by >= area.y + area.height {
                    continue;
                }

                let top = &self.frame.cells[top_y][x];
                let bot = if bot_y < canvas_h {
                    &self.frame.cells[bot_y][x]
                } else {
                    // shouldn't happen for 16-row canvas but be safe
                    &self.frame.cells[top_y][x]
                };

                let top_vis = is_visible(top);
                let bot_vis = is_visible(bot);
                let top_color = dur_to_color(top.fg);
                let bot_color = dur_to_color(bot.fg);

                let cell = &mut buf[(bx, by)];
                match (top_vis, bot_vis) {
                    (true, true) => {
                        cell.set_symbol("▀");
                        cell.set_style(Style::default().fg(top_color).bg(bot_color));
                    }
                    (true, false) => {
                        cell.set_symbol("▀");
                        cell.set_style(Style::default().fg(top_color));
                    }
                    (false, true) => {
                        cell.set_symbol("▄");
                        cell.set_style(Style::default().fg(bot_color));
                    }
                    (false, false) => {
                        cell.set_symbol(" ");
                    }
                }
            }
        }
    }
}
