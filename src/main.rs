use std::{
    io::{BufRead, BufReader, IsTerminal, Read, Write},
    process::{Command, Stdio},
    sync::{
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc, Arc, Mutex,
    },
    thread,
    time::{Duration, Instant},
};

use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{
        Axis, BarChart, Block, BorderType, Borders, Chart, Dataset, GraphType, Padding, Paragraph,
        Wrap,
    },
    Terminal,
};
use tui_big_text::{BigText, PixelSize};

// Theme & Configuration Constants

mod theme {
    use ratatui::style::Color;

    pub struct Colors;
    impl Colors {
        pub const COLOUR_GOOD_0: Color = Color::Rgb(0, 163, 224);
        pub const COLOUR_WARN_0: Color = Color::Rgb(245, 158, 11);
        pub const COLOUR_BAD_0: Color = Color::Rgb(224, 70, 90);
        pub const SELECTION_BG: Color = Self::COLOUR_GOOD_0;
        pub const BORDER_COLOUR: Color = Color::White;
        pub const TITLE_COLOUR: Color = Color::White;
    }

    pub struct CircleRender {
        pub samples: usize,
        pub cell_ratio: f64,
        pub outer_threshold: f64,
        pub coverage_thresholds: [f64; 4], // [full, dark, medium, light]
    }

    impl CircleRender {
        pub const BASIC: CircleRender = CircleRender {
            samples: 8,
            cell_ratio: 2.0,
            outer_threshold: 0.98,
            coverage_thresholds: [0.99, 0.80, 0.55, 0.30],
        };

        pub const SMOOTH: CircleRender = CircleRender {
            samples: 14,
            cell_ratio: 2.1,
            outer_threshold: 0.995,
            coverage_thresholds: [0.94, 0.70, 0.46, 0.22],
        };
    }

    pub const DEFAULT_WATCH_MS: u64 = 60_000;
    pub const MIN_WATCH_MS: u64 = 1;
}

#[derive(Clone, Copy, Debug)]
enum RenderMode {
    Text,
    Pie,
    BarChart,
    LineChart,
    StatusCircle,
    StatusRect,
    StatusCircleWithText,
    StatusRectWithText,
    Big,
}

fn cleanup_terminal() {
    let _ = disable_raw_mode();
    let mut stdout = std::io::stdout();
    let _ = execute!(stdout, LeaveAlternateScreen);
    let _ = stdout.flush();
}

fn sanitize_ansi_for_tui(input: &str) -> String {
    // Strip OSC sequences (e.g. iTerm shell integration: ESC ] ... BEL / ESC \)
    // before applying CSI filtering below.
    let bytes = input.as_bytes();
    let mut pre = String::with_capacity(input.len());
    let mut segment_start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b']' {
            pre.push_str(&input[segment_start..i]);
            i += 2;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    i += 1;
                    break;
                }
                if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
            segment_start = i;
            continue;
        }
        i += 1;
    }

    if segment_start < bytes.len() {
        pre.push_str(&input[segment_start..]);
    }

    let input = pre;

    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut segment_start = 0;
    let mut i = 0;

    while i < bytes.len() {
        if bytes[i] == 0x1B {
            out.push_str(&input[segment_start..i]);

            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                let mut j = i + 2;
                while j < bytes.len() {
                    let b = bytes[j];
                    if (0x40..=0x7E).contains(&b) {
                        // Keep SGR color/style escapes (CSI ... m); they are
                        // parsed into ratatui styled spans at render time.
                        if b == b'm' {
                            out.push_str(&input[i..=j]);
                        }
                        i = j + 1;
                        segment_start = i;
                        break;
                    }
                    j += 1;
                }

                if j >= bytes.len() {
                    break;
                }
                continue;
            }

            // Drop non-CSI escape sequences.
            i += 1;
            segment_start = i;
            continue;
        }

        i += 1;
    }

    if segment_start < bytes.len() {
        out.push_str(&input[segment_start..]);
    }

    out
}

fn line_requests_screen_clear(input: &str) -> bool {
    input.contains("\x1b[2J") || input.contains("\x1b[3J") || input.contains('\x0c')
}

/// Strip every ANSI/VT escape sequence from `input`, returning plain text.
/// Given `bytes[i] == 0x1B` (ESC) in a byte stream that may still be
/// growing, find where a CSI (`ESC [`) or OSC (`ESC ]`) sequence starting
/// there ends. Returns `None` if `bytes` doesn't (yet) contain enough data
/// to tell — the caller should wait for more bytes before deciding whether
/// `i` starts a real sequence at all. Unlike `strip_ansi` (which operates on
/// a complete, static string and has nothing to wait for), this is only
/// used by the streaming splitter in `read_stream_chunks`.
fn pending_escape_seq_end(bytes: &[u8], i: usize) -> Option<usize> {
    if i + 1 >= bytes.len() {
        return None; // ESC is the last byte read so far; more may follow
    }
    match bytes[i + 1] {
        b'[' => {
            // CSI sequence: ESC [ ... final-byte (0x40..=0x7E)
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7E).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() {
                Some(j + 1)
            } else {
                None
            }
        }
        b']' => {
            // OSC sequence: ESC ] ... (BEL | ESC \)
            let mut j = i + 2;
            while j < bytes.len() {
                if bytes[j] == 0x07 {
                    return Some(j + 1);
                }
                if bytes[j] == 0x1B && j + 1 < bytes.len() && bytes[j + 1] == b'\\' {
                    return Some(j + 2);
                }
                j += 1;
            }
            None
        }
        _ => Some(i + 1), // not CSI/OSC; only the ESC byte itself is consumed
    }
}

fn strip_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());
    let mut seg = 0usize;
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1B {
            out.push_str(&input[seg..i]);
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                // CSI sequence
                let mut j = i + 2;
                while j < bytes.len() && !(0x40..=0x7E).contains(&bytes[j]) {
                    j += 1;
                }
                i = if j < bytes.len() { j + 1 } else { j };
            } else if i + 1 < bytes.len() && bytes[i + 1] == b']' {
                // OSC sequence
                i += 2;
                while i < bytes.len() {
                    if bytes[i] == 0x07 {
                        i += 1;
                        break;
                    }
                    if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                        i += 2;
                        break;
                    }
                    i += 1;
                }
            } else {
                i += 1;
            }
            seg = i;
        } else {
            i += 1;
        }
    }
    if seg < input.len() {
        out.push_str(&input[seg..]);
    }
    out
}

/// Apply a semicolon-separated list of SGR parameters to `style`.
fn apply_sgr_params(mut style: Style, params: &str) -> Style {
    if params.is_empty() {
        return Style::default();
    }
    let parts: Vec<u8> = params.split(';').filter_map(|s| s.parse().ok()).collect();
    let mut i = 0usize;
    while i < parts.len() {
        match parts[i] {
            0 => style = Style::default(),
            1 => style = style.add_modifier(Modifier::BOLD),
            2 => style = style.add_modifier(Modifier::DIM),
            3 => style = style.add_modifier(Modifier::ITALIC),
            4 => style = style.add_modifier(Modifier::UNDERLINED),
            22 => style = style.remove_modifier(Modifier::BOLD),
            30 => style = style.fg(Color::Black),
            31 => style = style.fg(Color::Red),
            32 => style = style.fg(Color::Green),
            33 => style = style.fg(Color::Yellow),
            34 => style = style.fg(Color::Blue),
            35 => style = style.fg(Color::Magenta),
            36 => style = style.fg(Color::Cyan),
            37 => style = style.fg(Color::White),
            38 if i + 4 < parts.len() && parts[i + 1] == 2 => {
                style = style.fg(Color::Rgb(parts[i + 2], parts[i + 3], parts[i + 4]));
                i += 4;
            }
            39 => style = style.fg(Color::Reset),
            40 => style = style.bg(Color::Black),
            41 => style = style.bg(Color::Red),
            42 => style = style.bg(Color::Green),
            43 => style = style.bg(Color::Yellow),
            44 => style = style.bg(Color::Blue),
            45 => style = style.bg(Color::Magenta),
            46 => style = style.bg(Color::Cyan),
            47 => style = style.bg(Color::White),
            48 if i + 4 < parts.len() && parts[i + 1] == 2 => {
                style = style.bg(Color::Rgb(parts[i + 2], parts[i + 3], parts[i + 4]));
                i += 4;
            }
            49 => style = style.bg(Color::Reset),
            90 => style = style.fg(Color::DarkGray),
            91 => style = style.fg(Color::LightRed),
            92 => style = style.fg(Color::LightGreen),
            93 => style = style.fg(Color::LightYellow),
            94 => style = style.fg(Color::LightBlue),
            95 => style = style.fg(Color::LightMagenta),
            96 => style = style.fg(Color::LightCyan),
            97 => style = style.fg(Color::Gray),
            _ => {}
        }
        i += 1;
    }
    style
}

/// Convert a line that may contain ANSI SGR codes into a ratatui `Line` with
/// properly styled `Span`s.
fn ansi_to_line(input: &str) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut current_style = Style::default();
    let bytes = input.as_bytes();
    let mut display_col: usize = 0;
    let mut text_start = 0usize;
    let mut i = 0usize;

    fn expand_tabs(segment: &str, start_col: usize) -> (String, usize) {
        let mut out = String::with_capacity(segment.len());
        let mut col = start_col;
        for ch in segment.chars() {
            if ch == '\t' {
                let spaces = 8 - (col % 8);
                out.extend(std::iter::repeat_n(' ', spaces));
                col += spaces;
            } else {
                out.push(ch);
                col += 1;
            }
        }
        (out, col)
    }

    while i < bytes.len() {
        if bytes[i] == 0x1B && i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            if i > text_start {
                let (expanded, new_col) = expand_tabs(&input[text_start..i], display_col);
                display_col = new_col;
                spans.push(Span::styled(expanded, current_style));
            }
            let mut j = i + 2;
            while j < bytes.len() && !(0x40..=0x7E).contains(&bytes[j]) {
                j += 1;
            }
            if j < bytes.len() {
                if bytes[j] == b'm' {
                    let params = &input[i + 2..j];
                    current_style = apply_sgr_params(current_style, params);
                }
                i = j + 1;
            } else {
                i = j;
            }
            text_start = i;
        } else {
            i += 1;
        }
    }
    if text_start < input.len() {
        let (expanded, _new_col) = expand_tabs(&input[text_start..], display_col);
        spans.push(Span::styled(expanded, current_style));
    }
    Line::from(spans)
}

fn format_watch_label(ms: u64) -> String {
    if ms.is_multiple_of(86_400_000) {
        format!("{}d", ms / 86_400_000)
    } else if ms.is_multiple_of(3_600_000) {
        format!("{}h", ms / 3_600_000)
    } else if ms.is_multiple_of(60_000) {
        format!("{}m", ms / 60_000)
    } else if ms.is_multiple_of(1000) {
        format!("{}s", ms / 1000)
    } else {
        format!("{}ms", ms)
    }
}

fn parse_colour(s: &str) -> Option<Color> {
    // Try RGB format: r,g,b (0-255 each)
    if s.contains(',') {
        let mut it = s.split(',').map(str::trim);
        let r = it.next()?.parse::<u8>().ok()?;
        let g = it.next()?.parse::<u8>().ok()?;
        let b = it.next()?.parse::<u8>().ok()?;
        if it.next().is_none() {
            return Some(Color::Rgb(r, g, b));
        }
    }

    // Try hex format: #RRGGBB or RRGGBB
    let hex = s.strip_prefix('#').unwrap_or(s);
    if hex.len() == 6 && hex.chars().all(|c| c.is_ascii_hexdigit()) {
        let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
        let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
        let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
        return Some(Color::Rgb(r, g, b));
    }
    // Named colors
    match s.to_lowercase().as_str() {
        "black" => Some(Color::Black),
        "red" => Some(Color::Red),
        "green" => Some(Color::Green),
        "yellow" => Some(Color::Yellow),
        "blue" => Some(Color::Blue),
        "magenta" => Some(Color::Magenta),
        "cyan" => Some(Color::Cyan),
        "white" => Some(Color::White),
        "gray" | "grey" => Some(Color::Gray),
        "darkgray" | "darkgrey" => Some(Color::DarkGray),
        _ => None,
    }
}

fn parse_duration_ms(s: &str) -> Option<u64> {
    let (num, unit) = if let Some(n) = s.strip_suffix("ms") {
        (n, "ms")
    } else if let Some(n) = s.strip_suffix('s') {
        (n, "s")
    } else if let Some(n) = s.strip_suffix('m') {
        (n, "m")
    } else if let Some(n) = s.strip_suffix('h') {
        (n, "h")
    } else {
        (s.strip_suffix('d')?, "d")
    };
    let n: u64 = num.parse().ok()?;
    let ms = match unit {
        "ms" => n,
        "s" => n.checked_mul(1000)?,
        "m" => n.checked_mul(60_000)?,
        "h" => n.checked_mul(3_600_000)?,
        "d" => n.checked_mul(86_400_000)?,
        _ => return None,
    };
    Some(ms)
}

fn parse_pie_values(output: &str) -> Option<[f64; 3]> {
    for line in output.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let normalized = trimmed
            .chars()
            .map(|c| {
                if c.is_ascii_digit() || c == '.' || c == '-' {
                    c
                } else {
                    ' '
                }
            })
            .collect::<String>();

        let nums: Vec<f64> = normalized
            .split_whitespace()
            .filter_map(|s| s.parse::<f64>().ok())
            .collect();

        if nums.len() >= 3 {
            let a = nums[0].max(0.0);
            let b = nums[1].max(0.0);
            let c = nums[2].max(0.0);
            if a + b + c > 0.0 {
                return Some([a, b, c]);
            }
        }
    }

    None
}

/// Parse each non-empty output line as a series of `label value` or bare `value` tokens.
/// Returns one bar entry per numeric token found, labelled from the token to its left (if
/// alphanumeric) or auto-indexed if not.  At least one bar must be found.
fn parse_bar_values(output: &str) -> Option<Vec<(String, u64)>> {
    let mut bars: Vec<(String, u64)> = Vec::new();
    for line in output.lines() {
        let trimmed = strip_ansi(line.trim());
        if trimmed.is_empty() {
            continue;
        }
        // Tokenise: split on whitespace, try to parse each token as a number;
        // if the previous token was a word treat it as the label.
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        let mut i = 0;
        while i < tokens.len() {
            if let Ok(n) = tokens[i].parse::<f64>() {
                let v = n.max(0.0).round() as u64;
                let label = if i > 0 {
                    let prev = tokens[i - 1].trim_matches(|c: char| {
                        matches!(c, ':' | ',' | ';' | '|' | '(' | ')' | '[' | ']')
                    });
                    if !prev.is_empty()
                        && prev.chars().all(|c| {
                            c.is_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | '=')
                        })
                    {
                        prev.to_string()
                    } else {
                        // No word label — use the rounded value so x-axis matches bar heights
                        v.to_string()
                    }
                } else {
                    // Value is first token — use the rounded value as its own x-axis label
                    v.to_string()
                };
                bars.push((label, v));
            }
            i += 1;
        }
    }
    if bars.is_empty() {
        None
    } else {
        Some(bars)
    }
}

fn parse_status_value(output: &str) -> Option<u64> {
    // Only the last non-empty line counts as the status: scanning every line
    // for "the last numeric token anywhere" let stray digits on earlier lines
    // (e.g. leaked escape-sequence fragments, pod names, timestamps) silently
    // override the real status.
    let last_line = output.lines().map(str::trim).rfind(|l| !l.is_empty())?;

    let normalized = last_line
        .chars()
        .map(|c| {
            if c.is_ascii_digit() || c == '-' {
                c
            } else {
                ' '
            }
        })
        .collect::<String>();

    normalized
        .split_whitespace()
        .next_back()?
        .parse::<u64>()
        .ok()
}

fn pie_lines(width: u16, height: u16, values: [f64; 3]) -> Vec<Line<'static>> {
    render_circle(
        width,
        height,
        values,
        theme::CircleRender::BASIC,
        theme::Colors::COLOUR_GOOD_0,
        theme::Colors::COLOUR_WARN_0,
        theme::Colors::COLOUR_BAD_0,
    )
}

fn render_circle(
    width: u16,
    height: u16,
    values: [f64; 3],
    config: theme::CircleRender,
    good: Color,
    warn: Color,
    bad: Color,
) -> Vec<Line<'static>> {
    let total = values[0] + values[1] + values[2];
    if total <= 0.0 || width == 0 || height == 0 {
        return Vec::new();
    }

    let a1 = values[0] / total * std::f64::consts::TAU;
    let a2 = (values[0] + values[1]) / total * std::f64::consts::TAU;

    let half_w = width as f64 / 2.0;
    let half_h = height as f64 / 2.0;
    let radius = half_w.min(half_h * config.cell_ratio).max(1.0);

    let mut lines = Vec::with_capacity(height as usize);
    for y in 0..height {
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(width as usize);
        for x in 0..width {
            let mut seg = [0usize; 3];
            let mut in_shape = 0usize;

            for sy in 0..config.samples {
                for sx in 0..config.samples {
                    let fx = (x as f64 + (sx as f64 + 0.5) / config.samples as f64) / width as f64;
                    let fy = (y as f64 + (sy as f64 + 0.5) / config.samples as f64) / height as f64;
                    let px = ((fx * width as f64) - half_w) / radius;
                    let py = ((fy * height as f64) - half_h) * config.cell_ratio / radius;
                    let r = (px * px + py * py).sqrt();
                    if r > config.outer_threshold {
                        continue;
                    }

                    in_shape += 1;
                    let mut ang = py.atan2(px);
                    if ang < 0.0 {
                        ang += std::f64::consts::TAU;
                    }
                    if ang < a1 {
                        seg[0] += 1;
                    } else if ang < a2 {
                        seg[1] += 1;
                    } else {
                        seg[2] += 1;
                    }
                }
            }

            if in_shape == 0 {
                spans.push(Span::raw(" "));
                continue;
            }

            let coverage = in_shape as f64 / (config.samples * config.samples) as f64;
            let ch = choose_glyph_for_coverage(coverage, config.coverage_thresholds);
            let color = choose_color(seg, good, warn, bad);
            spans.push(Span::styled(ch.to_string(), Style::default().fg(color)));
        }
        lines.push(Line::from(spans));
    }

    lines
}

fn centered_rect(area: Rect, w: u16, h: u16) -> Rect {
    let x = area.x + area.width.saturating_sub(w) / 2;
    let y = area.y + area.height.saturating_sub(h) / 2;
    Rect::new(x, y, w.min(area.width), h.min(area.height))
}

fn status_color(status: u64, good: Color, bad: Color) -> Color {
    if status == 0 {
        good
    } else {
        bad
    }
}

fn status_text_color(_status: u64, _good: Color, _bad: Color) -> Color {
    Color::White
}

fn status_to_circle_values(status: u64) -> [f64; 3] {
    if status == 0 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 0.0, 1.0]
    }
}

fn choose_glyph_for_coverage(coverage: f64, thresholds: [f64; 4]) -> &'static str {
    if coverage >= thresholds[0] {
        "█"
    } else if coverage >= thresholds[1] {
        "▓"
    } else if coverage >= thresholds[2] {
        "▒"
    } else if coverage >= thresholds[3] {
        "░"
    } else {
        " "
    }
}

fn choose_color(seg: [usize; 3], good: Color, warn: Color, bad: Color) -> Color {
    if seg[0] >= seg[1] && seg[0] >= seg[2] {
        good
    } else if seg[1] >= seg[2] {
        warn
    } else {
        bad
    }
}

/// Return the lines to display for the given run.
/// If the current run has no output yet (new run just launched),
/// fall back to the previous run's lines to avoid a "Waiting…" flash.
fn display_lines_for_run(buf: &[(u64, String)], current_run: u64) -> Vec<String> {
    let current: Vec<String> = buf
        .iter()
        .filter(|(rid, _)| *rid == current_run)
        .map(|(_, line)| line.clone())
        .collect();
    if !current.is_empty() {
        return current;
    }
    // New run hasn't produced output yet — show previous run's lines.
    buf.iter()
        .filter(|(rid, _)| *rid < current_run)
        .map(|(_, line)| line.clone())
        .collect()
}

fn apply_append_text(lines: &[String], append_text: Option<&str>) -> Option<String> {
    let value_line = lines
        .iter()
        .rev()
        .find(|l| !strip_ansi(l).trim().is_empty())
        .cloned()?;
    let trimmed = strip_ansi(&value_line);
    let trimmed = trimmed.trim();
    Some(match append_text {
        Some(suffix) => format!("{}{}", trimmed, suffix),
        None => trimmed.to_string(),
    })
}

fn status_with_text_label(status: u64, append_text: Option<&str>) -> String {
    if status == 0 {
        "OK".to_string()
    } else {
        match append_text {
            Some(suffix) => format!("{}{}", status, suffix),
            None => status.to_string(),
        }
    }
}

fn static_icon_mask_for_token(token: &str) -> Option<&'static [&'static str]> {
    // A compact, modern key silhouette. '#' are filled pixels, '.' are empty.
    const KEY_ICON_MASK: &[&str] = &[
        ".....######..............",
        "...###....###............",
        "..##..##....##...........",
        "..##..##....############.",
        "..##........##....##..##.",
        "...###....###.....######.",
        ".....######........##....",
    ];

    if token.eq_ignore_ascii_case("KEY_ICON") || token.eq_ignore_ascii_case("KEY") {
        Some(KEY_ICON_MASK)
    } else {
        None
    }
}

fn render_scaled_icon_lines(mask: &[&str], max_w: u16, max_h: u16) -> Vec<String> {
    if max_w == 0 || max_h == 0 || mask.is_empty() {
        return Vec::new();
    }

    // Normalize the mask to its filled bounding box so icon tokens stay centered
    // even if their masks include uneven left/right padding.
    let mask_chars: Vec<Vec<char>> = mask.iter().map(|row| row.chars().collect()).collect();
    let mut min_col = usize::MAX;
    let mut max_col = 0usize;
    for row in &mask_chars {
        for (idx, ch) in row.iter().enumerate() {
            if *ch != '.' && *ch != ' ' {
                min_col = min_col.min(idx);
                max_col = max_col.max(idx);
            }
        }
    }
    if min_col == usize::MAX {
        return Vec::new();
    }

    let trimmed_mask: Vec<String> = mask_chars
        .iter()
        .map(|row| {
            let end = max_col.min(row.len().saturating_sub(1));
            row[min_col..=end].iter().collect::<String>()
        })
        .collect();

    let base_h = trimmed_mask.len() as u16;
    let base_w = trimmed_mask
        .iter()
        .map(|row| row.chars().count() as u16)
        .max()
        .unwrap_or(1);
    if base_w == 0 || base_h == 0 {
        return Vec::new();
    }

    let scale_x = (max_w / base_w).max(1);
    let scale_y = (max_h / base_h).max(1);
    let scale = scale_x.min(scale_y).max(1) as usize;

    let mut out: Vec<String> = Vec::new();
    for row in &trimmed_mask {
        let mut expanded_row = String::new();
        for ch in row.chars() {
            let pixel = match ch {
                '#' => '█',
                '+' => '▓',
                '=' => '▒',
                '.' | ' ' => ' ',
                _ => ch,
            };
            for _ in 0..scale {
                expanded_row.push(pixel);
            }
        }
        for _ in 0..scale {
            out.push(expanded_row.clone());
        }
    }
    out
}

/// Combine pairs of full-block rows into a single row of half-block characters.
/// Each output row represents two input rows using ▀ (upper), ▄ (lower),
/// █ (both), or ' ' (neither), halving the visual height like PixelSize::HalfHeight.
fn render_half_height_pass(rows: Vec<String>) -> Vec<String> {
    let to_cells = |row: &str| -> Vec<bool> { row.chars().map(|c| c != ' ').collect() };
    let mut out = Vec::new();
    let mut i = 0;
    while i < rows.len() {
        let top = to_cells(&rows[i]);
        let bot = if i + 1 < rows.len() {
            to_cells(&rows[i + 1])
        } else {
            vec![]
        };
        let width = top.len().max(bot.len());
        let mut combined = String::new();
        for col in 0..width {
            let t = top.get(col).copied().unwrap_or(false);
            let b = bot.get(col).copied().unwrap_or(false);
            combined.push(match (t, b) {
                (true, true) => '█',
                (true, false) => '▀',
                (false, true) => '▄',
                (false, false) => ' ',
            });
        }
        out.push(combined);
        i += 2;
    }
    out
}

fn push_output_line(
    line: &str,
    buffer: &Arc<Mutex<Vec<(u64, String)>>>,
    active_run: &Arc<AtomicU64>,
    run_id: u64,
    update_tx: &mpsc::Sender<()>,
) {
    let plain_text = strip_ansi(line);
    let clear_requested = line_requests_screen_clear(line) && plain_text.trim().is_empty();
    let clean = sanitize_ansi_for_tui(line);
    let mut buf = buffer.lock().unwrap();
    if active_run.load(Ordering::SeqCst) != run_id {
        return;
    }
    // First line from a new run: evict all lines from older runs.
    let is_new_run = !buf.iter().any(|(rid, _)| *rid == run_id);
    if is_new_run {
        buf.retain(|(rid, _)| *rid == run_id);
    }
    if clear_requested {
        buf.retain(|(rid, _)| *rid != run_id);
    }
    if !clear_requested {
        buf.push((run_id, clean));
    }
    let _ = update_tx.send(());
}

fn read_stream_chunks<R: Read>(
    reader: R,
    buffer: Arc<Mutex<Vec<(u64, String)>>>,
    active_run: Arc<AtomicU64>,
    run_id: u64,
    update_tx: mpsc::Sender<()>,
) {
    let mut reader = BufReader::new(reader);
    let mut chunk = [0u8; 4096];
    let mut pending: Vec<u8> = Vec::new();

    loop {
        if active_run.load(Ordering::SeqCst) != run_id {
            break;
        }

        let n = match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => n,
            Err(_) => break,
        };

        pending.extend_from_slice(&chunk[..n]);

        // Split into "lines" on \n/\r, but never inside an escape sequence:
        // shell-integration hooks (e.g. iTerm2 via `zsh -i`) routinely emit
        // OSC sequences containing an embedded \r, and splitting through one
        // leaves two fragments that no longer look like a complete sequence
        // to push_output_line's ANSI stripping — leaking raw escape bytes
        // (often digits, e.g. RemoteHost=host) into the parsed status text.
        let mut start = 0usize;
        let mut i = 0usize;
        while i < pending.len() {
            if pending[i] == 0x1B {
                match pending_escape_seq_end(&pending, i) {
                    Some(end) => i = end,
                    None => break, // sequence incomplete; wait for more data
                }
            } else if pending[i] == b'\n' || pending[i] == b'\r' {
                let line = String::from_utf8_lossy(&pending[start..i]).to_string();
                push_output_line(&line, &buffer, &active_run, run_id, &update_tx);
                i += 1;
                start = i;
            } else {
                i += 1;
            }
        }

        if start > 0 {
            pending.drain(0..start);
        }
    }

    if !pending.is_empty() && active_run.load(Ordering::SeqCst) == run_id {
        let line = String::from_utf8_lossy(&pending).to_string();
        push_output_line(&line, &buffer, &active_run, run_id, &update_tx);
    }
}

fn spawn_script(
    cmd_argv: &[String],
    buffer: Arc<Mutex<Vec<(u64, String)>>>,
    active_run: Arc<AtomicU64>,
    run_id: u64,
    update_tx: mpsc::Sender<()>,
    done_tx: mpsc::Sender<u64>,
) {
    let argv = cmd_argv.to_vec();
    thread::spawn(move || {
        if argv.is_empty() {
            let _ = done_tx.send(run_id);
            return;
        }

        let mut cmd = Command::new(&argv[0]);
        if argv.len() > 1 {
            cmd.args(&argv[1..]);
        }

        let mut child = match cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                let msg = format!("error: failed to spawn command '{}': {e}", argv[0]);
                push_output_line(&msg, &buffer, &active_run, run_id, &update_tx);
                let _ = update_tx.send(());
                let _ = done_tx.send(run_id);
                return;
            }
        };

        let stdout = child.stdout.take().unwrap();
        let stderr = child.stderr.take().unwrap();

        let reader_buffer_out = Arc::clone(&buffer);
        let reader_run_out = Arc::clone(&active_run);
        let reader_tx_out = update_tx.clone();
        let out_handle = thread::spawn(move || {
            read_stream_chunks(
                stdout,
                reader_buffer_out,
                reader_run_out,
                run_id,
                reader_tx_out,
            );
        });

        let reader_buffer_err = Arc::clone(&buffer);
        let reader_run_err = Arc::clone(&active_run);
        let reader_tx_err = update_tx.clone();
        let err_handle = thread::spawn(move || {
            read_stream_chunks(
                stderr,
                reader_buffer_err,
                reader_run_err,
                run_id,
                reader_tx_err,
            );
        });

        let _ = out_handle.join();
        let _ = err_handle.join();
        let _ = child.wait();

        // Ensure the final buffered output is rendered.
        let _ = update_tx.send(());
        let _ = done_tx.send(run_id);
    });
}

fn is_exit_key(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q'))
        || (matches!(code, KeyCode::Char('c')) && modifiers.contains(KeyModifiers::CONTROL))
}

fn split_args_on_separator(args: &[String]) -> (usize, Vec<String>) {
    if let Some(sep_index) = args.iter().position(|a| a == "--") {
        (sep_index, args[sep_index + 1..].to_vec())
    } else {
        (args.len(), Vec::new())
    }
}

pub fn shell_escape_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "''".to_string();
    }

    let is_safe = arg
        .bytes()
        .all(|b| matches!(b, b'a'..=b'z' | b'A'..=b'Z' | b'0'..=b'9' | b'_' | b'-' | b'.' | b'/' | b':' | b'@' | b'%'));
    if is_safe {
        return arg.to_string();
    }

    let escaped = arg.replace('\'', "'\\''");
    format!("'{}'", escaped)
}

#[cfg(unix)]
fn install_termination_signal_flag() -> Arc<AtomicBool> {
    let terminate_requested = Arc::new(AtomicBool::new(false));
    for sig in [
        signal_hook::consts::signal::SIGTERM,
        signal_hook::consts::signal::SIGINT,
        signal_hook::consts::signal::SIGHUP,
    ] {
        let _ = signal_hook::flag::register(sig, Arc::clone(&terminate_requested));
    }
    terminate_requested
}

#[cfg(not(unix))]
fn install_termination_signal_flag() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

fn read_stdin_once(
    buffer: Arc<Mutex<Vec<(u64, String)>>>,
    active_run: Arc<AtomicU64>,
    run_id: u64,
    update_tx: mpsc::Sender<()>,
    done_tx: mpsc::Sender<u64>,
) {
    let stdin = std::io::stdin();
    let reader = BufReader::new(stdin.lock());

    for line in reader.lines() {
        if active_run.load(Ordering::SeqCst) != run_id {
            break;
        }
        if let Ok(line) = line {
            push_output_line(&line, &buffer, &active_run, run_id, &update_tx);
        }
    }

    let _ = update_tx.send(());
    let _ = done_tx.send(run_id);
}

struct CliArgs {
    frame: bool,
    select_mode: bool,
    render_mode: RenderMode,
    watch_ms: Option<u64>,
    title_override: Option<String>,
    append_text: Option<String>,
    status_rect_static_text: Option<String>,
    status_rect_static_icon: Option<String>,
    status_rect_conditional_cmd: Option<String>,
    description: Option<String>,
    border_colour: Color,
    title_colour: Color,
    status_good_colour: Color,
    status_bad_colour: Color,
    script_args: Vec<String>,
}

fn parse_cli_args(args: &[String]) -> CliArgs {
    let mut frame = true;
    let mut select_mode = false;
    let mut render_mode = RenderMode::Text;
    let mut watch_ms: Option<u64> = None;
    let mut title_override: Option<String> = None;
    let mut append_text: Option<String> = None;
    let mut status_rect_static_text: Option<String> = None;
    let mut status_rect_static_icon: Option<String> = None;
    let mut status_rect_conditional_cmd: Option<String> = None;
    let mut description: Option<String> = None;
    let mut border_colour: Color = theme::Colors::BORDER_COLOUR;
    let mut title_colour: Color = theme::Colors::TITLE_COLOUR;
    let mut status_good_colour: Color = theme::Colors::COLOUR_GOOD_0;
    let mut status_bad_colour: Color = theme::Colors::COLOUR_BAD_0;
    let mut script_args: Vec<String> = Vec::new();

    let (parse_end, trailing_script_args) = split_args_on_separator(args);

    let mut i = 1;
    while i < parse_end {
        match args[i].as_str() {
            "--no-frame" => {
                frame = false;
                i += 1;
            }
            "--select" => {
                select_mode = true;
                i += 1;
            }
            "--pie-chart" => {
                render_mode = RenderMode::Pie;
                i += 1;
            }
            "--bar-chart" => {
                render_mode = RenderMode::BarChart;
                i += 1;
            }
            "--line-chart" => {
                render_mode = RenderMode::LineChart;
                i += 1;
            }
            "--status-circle" => {
                render_mode = RenderMode::StatusCircle;
                i += 1;
            }
            "--status-rect" => {
                render_mode = RenderMode::StatusRect;
                i += 1;
            }
            "--status-circle-with-text" => {
                render_mode = RenderMode::StatusCircleWithText;
                i += 1;
            }
            "--status-rect-with-text" => {
                render_mode = RenderMode::StatusRectWithText;
                status_rect_static_text = None;
                status_rect_static_icon = None;
                i += 1;
            }
            "--status-rect-with-static-text" => {
                if i + 1 >= parse_end {
                    eprintln!("Error: --status-rect-with-static-text requires a value");
                    std::process::exit(1);
                }
                status_rect_static_text = Some(args[i + 1].clone());
                status_rect_static_icon = None;
                render_mode = RenderMode::StatusRectWithText;
                i += 2;
            }
            _ if args[i].starts_with("--status-rect-with-static-text=") => {
                let value = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                if value.is_empty() {
                    eprintln!("Error: --status-rect-with-static-text requires a non-empty value");
                    std::process::exit(1);
                }
                status_rect_static_text = Some(value.to_string());
                status_rect_static_icon = None;
                render_mode = RenderMode::StatusRectWithText;
                i += 1;
            }
            "--status-rect-with-static-icon" => {
                if i + 1 >= parse_end {
                    eprintln!("Error: --status-rect-with-static-icon requires a value");
                    std::process::exit(1);
                }
                status_rect_static_icon = Some(args[i + 1].clone());
                status_rect_static_text = None;
                render_mode = RenderMode::StatusRectWithText;
                i += 2;
            }
            _ if args[i].starts_with("--status-rect-with-static-icon=") => {
                let value = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                if value.is_empty() {
                    eprintln!("Error: --status-rect-with-static-icon requires a non-empty value");
                    std::process::exit(1);
                }
                status_rect_static_icon = Some(value.to_string());
                status_rect_static_text = None;
                render_mode = RenderMode::StatusRectWithText;
                i += 1;
            }
            "--status-rect-with-conditional-command" => {
                if i + 1 >= parse_end {
                    eprintln!("Error: --status-rect-with-conditional-command requires a value");
                    std::process::exit(1);
                }
                status_rect_conditional_cmd = Some(args[i + 1].clone());
                render_mode = RenderMode::StatusRectWithText;
                i += 2;
            }
            _ if args[i].starts_with("--status-rect-with-conditional-command=") => {
                let value = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                if value.is_empty() {
                    eprintln!(
                        "Error: --status-rect-with-conditional-command requires a non-empty value"
                    );
                    std::process::exit(1);
                }
                status_rect_conditional_cmd = Some(value.to_string());
                render_mode = RenderMode::StatusRectWithText;
                i += 1;
            }
            "--big-text" => {
                render_mode = RenderMode::Big;
                i += 1;
            }
            "--append-text" => {
                if i + 1 >= parse_end {
                    eprintln!("Error: --append-text requires a value");
                    std::process::exit(1);
                }
                append_text = Some(args[i + 1].clone());
                i += 2;
            }
            _ if args[i].starts_with("--append-text=") => {
                let value = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                append_text = Some(value.to_string());
                i += 1;
            }
            "--description" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --description requires a value");
                    std::process::exit(1);
                }
                description = Some(args[i + 1].clone());
                i += 2;
            }
            _ if args[i].starts_with("--description=") => {
                let value = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                if value.is_empty() {
                    eprintln!("Error: --description requires a non-empty value");
                    std::process::exit(1);
                }
                description = Some(value.to_string());
                i += 1;
            }
            "--title" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --title requires a value");
                    std::process::exit(1);
                }
                title_override = Some(args[i + 1].clone());
                i += 2;
            }
            _ if args[i].starts_with("--title=") => {
                let value = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                if value.is_empty() {
                    eprintln!("Error: --title requires a non-empty value");
                    std::process::exit(1);
                }
                title_override = Some(value.to_string());
                i += 1;
            }
            "--watch" => {
                if i + 1 < args.len() && parse_duration_ms(&args[i + 1]).is_some() {
                    let interval_ms = parse_duration_ms(&args[i + 1]).unwrap();
                    watch_ms = Some(interval_ms.max(theme::MIN_WATCH_MS));
                    i += 2;
                } else {
                    watch_ms = Some(theme::DEFAULT_WATCH_MS);
                    i += 1;
                }
            }
            _ if args[i].starts_with("--watch=") => {
                let val = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                let interval_ms = match parse_duration_ms(val) {
                    Some(ms) => ms,
                    None => {
                        eprintln!("Error: invalid --watch value '{}'. Use a number with unit: ms, s, m, h, d (e.g. 60ms, 5s, 2m)", val);
                        std::process::exit(1);
                    }
                };
                watch_ms = Some(interval_ms.max(theme::MIN_WATCH_MS));
                i += 1;
            }
            "--border-colour" => {
                if i + 1 >= args.len() {
                    eprintln!(
                        "Error: --border-colour requires a value (e.g. #ff0000, cyan, white)"
                    );
                    std::process::exit(1);
                }
                border_colour = match parse_colour(&args[i + 1]) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --border-colour value '{}'", args[i + 1]);
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            _ if args[i].starts_with("--border-colour=") => {
                let val = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                border_colour = match parse_colour(val) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --border-colour value '{}'", val);
                        std::process::exit(1);
                    }
                };
                i += 1;
            }
            "--title-colour" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --title-colour requires a value (e.g. #ff0000, cyan, white)");
                    std::process::exit(1);
                }
                title_colour = match parse_colour(&args[i + 1]) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --title-colour value '{}'", args[i + 1]);
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            _ if args[i].starts_with("--title-colour=") => {
                let val = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                title_colour = match parse_colour(val) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --title-colour value '{}'", val);
                        std::process::exit(1);
                    }
                };
                i += 1;
            }
            "--status-colour-good" => {
                if i + 1 >= args.len() {
                    eprintln!(
                        "Error: --status-colour-good requires a value (e.g. #00a3e0, cyan, green)"
                    );
                    std::process::exit(1);
                }
                status_good_colour = match parse_colour(&args[i + 1]) {
                    Some(c) => c,
                    None => {
                        eprintln!(
                            "Error: invalid --status-colour-good value '{}'",
                            args[i + 1]
                        );
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            _ if args[i].starts_with("--status-colour-good=") => {
                let val = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                status_good_colour = match parse_colour(val) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --status-colour-good value '{}'", val);
                        std::process::exit(1);
                    }
                };
                i += 1;
            }
            "--status-colour-bad" => {
                if i + 1 >= args.len() {
                    eprintln!("Error: --status-colour-bad requires a value (e.g. #e0465a, red)");
                    std::process::exit(1);
                }
                status_bad_colour = match parse_colour(&args[i + 1]) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --status-colour-bad value '{}'", args[i + 1]);
                        std::process::exit(1);
                    }
                };
                i += 2;
            }
            _ if args[i].starts_with("--status-colour-bad=") => {
                let val = args[i].split_once('=').map(|(_, v)| v).unwrap_or("");
                status_bad_colour = match parse_colour(val) {
                    Some(c) => c,
                    None => {
                        eprintln!("Error: invalid --status-colour-bad value '{}'", val);
                        std::process::exit(1);
                    }
                };
                i += 1;
            }
            "--version" | "-V" => {
                let version = match std::process::Command::new("git")
                    .args(["describe", "--exact-match", "HEAD"])
                    .stderr(std::process::Stdio::null())
                    .output()
                {
                    Ok(out) if out.status.success() => {
                        String::from_utf8_lossy(&out.stdout).trim().to_string()
                    }
                    _ => std::process::Command::new("git")
                        .args(["rev-parse", "HEAD"])
                        .stderr(std::process::Stdio::null())
                        .output()
                        .ok()
                        .filter(|o| o.status.success())
                        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                        .unwrap_or_else(|| include_str!("../VERSION").trim().to_string()),
                };
                println!("{}", version);
                std::process::exit(0);
            }
            "--help" | "-h" => {
                println!("vju-t - the versatile terminal widget\n");
                println!("Usage: vju-t [OPTIONS] [--] <command> [arguments...]\n");
                println!("Options:");
                println!("  --no-frame                    Disable border frame");
                println!(
                    "  --select                      Enable output line selection in text mode"
                );
                println!("  --big-text                    Render output as large text");
                println!("  --pie-chart                   Render output as pie chart");
                println!("  --bar-chart                   Render output as bar chart");
                println!("  --line-chart                  Render output as line chart");
                println!("  --status-circle               Render status as circle");
                println!("  --status-rect                 Render status as rectangle");
                println!("  --status-circle-with-text     Circle with text overlay");
                println!("  --status-rect-with-text       Rectangle with big-text value");
                println!(
                    "  --status-rect-with-static-text <text>  Rectangle with static text overlay"
                );
                println!("  --status-rect-with-static-icon <icon>  Rectangle with built-in static icon (e.g. KEY_ICON)");
                println!("  --status-rect-with-conditional-command <cmd>  Rectangle: run <cmd> on success and show its output instead of 'OK'");
                println!("      Tip: use 'zsh -i -c \"shell-fn args\"' to access shell functions");
                println!("  --append-text <text>          Append suffix after big-text value (e.g. 's', 'ms')");
                println!(
                    "  --watch [<duration>]          Re-run command periodically (default: 60s)"
                );
                println!("  --title <text>                Set pane title");
                println!("  --description <text>          Description shown in info overlay (v)");
                println!("  --border-colour <colour>      Border colour (e.g. #ff0000, cyan)");
                println!("  --title-colour <colour>       Title colour");
                println!("  --status-colour-good <colour> Status color for healthy (exit 0)");
                println!("  --status-colour-bad <colour>  Status color for unhealthy (!=0)");
                println!("  -V, --version                 Print version");
                println!("  -h, --help                    Show this help\n");
                println!("Duration units: ms, s, m, h, d (e.g. 500ms, 5s, 2m)\n");
                println!("Keys:");
                println!("  q       Quit");
                println!("  v       Toggle info overlay");
                println!("  r       Re-run command");
                println!("  Up/Down Cycle selection (with --select)");
                println!("  Up/Down Scroll output");
                println!("  End     Resume auto-scroll");
                std::process::exit(0);
            }
            _ => {
                script_args.push(args[i].clone());
                i += 1;
            }
        }
    }

    if !trailing_script_args.is_empty() {
        script_args = trailing_script_args;
    }

    CliArgs {
        frame,
        select_mode,
        render_mode,
        watch_ms,
        title_override,
        append_text,
        status_rect_static_text,
        status_rect_static_icon,
        status_rect_conditional_cmd,
        description,
        border_colour,
        title_colour,
        status_good_colour,
        status_bad_colour,
        script_args,
    }
}

fn main() -> anyhow::Result<()> {
    // Setup panic handler to restore terminal
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        cleanup_terminal();
        default_hook(panic_info);
    }));

    let terminate_requested = install_termination_signal_flag();

    let args: Vec<String> = std::env::args().collect();
    let cli = parse_cli_args(&args);
    let frame = cli.frame;
    let select_mode = cli.select_mode;
    let render_mode = cli.render_mode;
    let watch_ms = cli.watch_ms;
    let append_text = cli.append_text;
    let status_rect_static_text = cli.status_rect_static_text;
    let status_rect_static_icon = cli.status_rect_static_icon;
    let status_rect_conditional_cmd = cli.status_rect_conditional_cmd;
    let description = cli.description;
    let border_colour = cli.border_colour;
    let title_colour = cli.title_colour;
    let status_good_colour = cli.status_good_colour;
    let status_bad_colour = cli.status_bad_colour;
    let script_args = cli.script_args;

    if script_args.is_empty() && std::io::stdin().is_terminal() {
        eprintln!("Usage: vju-t [--no-frame] [--pie-chart|--bar-chart|--line-chart|--status-circle|--status-rect|--status-circle-with-text|--status-rect-with-text|--status-rect-with-static-text <text>|--status-rect-with-static-icon <icon>|--big-text] [--watch <duration>] [--title <text>] [--] <script> [arguments...]");
        eprintln!("  duration: number with unit, e.g. 60ms, 5s, 2m, 1h, 1d");
        eprintln!("  or pipe input: echo '10 20 30' | vju-t --line-chart");
        std::process::exit(1);
    }

    let stdin_mode = script_args.is_empty();

    let first_arg_name = if stdin_mode {
        "stdin"
    } else {
        std::path::Path::new(&script_args[0])
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&script_args[0])
    };
    let script_name = if let Some(custom_title) = cli.title_override {
        custom_title
    } else if !stdin_mode && first_arg_name == "zsh" && script_args.len() > 1 {
        script_args[1].clone()
    } else {
        first_arg_name.to_string()
    };

    // Shared output buffer (tagged with run_id)
    let buffer: Arc<Mutex<Vec<(u64, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let active_run = Arc::new(AtomicU64::new(1));
    let (update_tx, update_rx) = mpsc::channel::<()>();
    let (done_tx, done_rx) = mpsc::channel::<u64>();

    // Result of the conditional command run (shown instead of "OK" when healthy)
    let conditional_text: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));

    // Run once at startup: either command output or piped stdin.
    if stdin_mode {
        read_stdin_once(
            Arc::clone(&buffer),
            Arc::clone(&active_run),
            1,
            update_tx.clone(),
            done_tx.clone(),
        );
    } else {
        spawn_script(
            &script_args,
            Arc::clone(&buffer),
            Arc::clone(&active_run),
            1,
            update_tx.clone(),
            done_tx.clone(),
        );
    }

    let mut running_run: Option<u64> = Some(1);
    let mut last_watch_launch = Instant::now();

    // Setup terminal (retry briefly if TTY not yet ready, e.g. in freshly created tmux pane)
    for attempt in 0..10 {
        match enable_raw_mode() {
            Ok(_) => break,
            Err(e) => {
                if attempt == 9 {
                    eprintln!("Error: Failed to initialize terminal: {}", e);
                    std::process::exit(1);
                }
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
    let mut stdout = std::io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut needs_redraw = true;
    let mut scroll_offset: u16 = 0;
    let mut auto_scroll = true;
    let mut max_scroll: u16 = 0;
    let mut selected_index: Option<usize> = None;
    let mut show_info = false;

    loop {
        if terminate_requested.load(Ordering::SeqCst) {
            break;
        }

        if needs_redraw {
            terminal.draw(|f| {
                let size = f.area();

                if show_info {
                    let mut info_lines = vec![
                        Line::from(Span::styled("Command", Style::default().add_modifier(Modifier::BOLD))),
                        Line::from(script_args.join(" ")),
                        Line::from(""),
                        Line::from(Span::styled("Mode", Style::default().add_modifier(Modifier::BOLD))),
                        Line::from(format!("{:?}", render_mode)),
                        Line::from(""),
                        Line::from(Span::styled("Watch", Style::default().add_modifier(Modifier::BOLD))),
                        Line::from(match watch_ms {
                            Some(ms) => format_watch_label(ms),
                            None => "off".to_string(),
                        }),
                    ];
                    if let Some(ref desc) = description {
                        info_lines.push(Line::from(""));
                        info_lines.push(Line::from(Span::styled("Description", Style::default().add_modifier(Modifier::BOLD))));
                        info_lines.push(Line::from(desc.clone()));
                    }
                    let info_block = Block::default()
                        .borders(Borders::ALL)
                        .border_type(BorderType::Rounded)
                        .border_style(Style::default().fg(border_colour))
                        .title(Span::styled(" info ", Style::default().fg(title_colour).add_modifier(Modifier::BOLD)));
                    let info_paragraph = Paragraph::new(info_lines)
                        .block(info_block)
                        .wrap(Wrap { trim: false });
                    f.render_widget(info_paragraph, size);
                    return;
                }

                // Collect raw lines (may contain SGR escape codes).
                // Show the most recent run that has any data — avoids flashing
                // "Waiting…" between watch cycles while the new run is starting.
                let raw_lines: Vec<String> = {
                    let buf = buffer.lock().unwrap();
                    let current_run = active_run.load(Ordering::SeqCst);
                    display_lines_for_run(&buf, current_run)
                };
                // Plain text (ANSI stripped) for Pie / Status parsing.
                let text: String = raw_lines.iter()
                    .map(|l| strip_ansi(l))
                    .collect::<Vec<_>>()
                    .join("\n");

                let border_height: u16 = if frame { 2 } else { 0 };
                let padding_top: u16 = if frame { 1 } else { 0 };
                match render_mode {
                    RenderMode::Text => {
                        let inner_height = size
                            .height
                            .saturating_sub(border_height + padding_top) as usize;

                        // Preserve fixed-width column layout (e.g. kubectl tables)
                        // by disabling soft wrapping in text mode.
                        let display_lines: usize = raw_lines.len();
                        let max_s = display_lines.saturating_sub(inner_height) as u16;
                        max_scroll = max_s;
                        let scroll = if auto_scroll {
                            max_s
                        } else {
                            scroll_offset.min(max_s)
                        };

                        let block = if frame {
                            let mut b = Block::default()
                                .title(Line::styled(script_name.as_str(), Style::default().fg(title_colour)))
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(border_colour))
                                .padding(Padding::new(1, 1, 1, 0));
                            if let Some(ms) = watch_ms {
                                b = b.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                            }
                            b
                        } else {
                            Block::default()
                        };

                        let styled_lines: Vec<Line<'static>> = raw_lines.iter()
                            .map(|l| ansi_to_line(l))
                            .collect();
                        let mut styled_lines = styled_lines;

                        if select_mode && !styled_lines.is_empty() && selected_index.is_some() {
                            let idx = selected_index.unwrap_or(0).min(styled_lines.len() - 1);
                            if let Some(line) = styled_lines.get_mut(idx) {
                                for span in &mut line.spans {
                                    span.style = span
                                        .style
                                        .patch(Style::default().bg(theme::Colors::SELECTION_BG).add_modifier(Modifier::BOLD));
                                }
                            }
                        }

                        let paragraph = Paragraph::new(styled_lines)
                            .block(block)
                            .scroll((scroll, 0));

                        f.render_widget(paragraph, size);
                    }
                    RenderMode::Pie => {
                        let c1 = theme::Colors::COLOUR_GOOD_0;
                        let c2 = theme::Colors::COLOUR_WARN_0;
                        let c3 = theme::Colors::COLOUR_BAD_0;
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        if let Some(values) = parse_pie_values(&text) {
                            f.render_widget(chart_block.clone(), size);
                            let inner = chart_block.inner(size);
                            let safe = Rect::new(
                                inner.x + 2, inner.y + 1,
                                inner.width.saturating_sub(4),
                                inner.height.saturating_sub(2),
                            );

                            let total = values[0] + values[1] + values[2];
                            let p = [values[0]/total*100.0, values[1]/total*100.0, values[2]/total*100.0];
                            let bar = |pct: f64| "■".repeat(((pct / 5.0).round() as usize).min(20));

                            // Split: pie on left, legend on right
                            let chunks = Layout::default()
                                .direction(Direction::Horizontal)
                                .constraints([Constraint::Min(10), Constraint::Length(28)])
                                .split(safe);

                            let pie_area = chunks[0];
                            let side = pie_area.height.min((pie_area.width as f32 * 0.92) as u16);
                            let draw_area = centered_rect(pie_area, side, side);
                            let lines = pie_lines(draw_area.width, draw_area.height, values);
                            f.render_widget(Paragraph::new(lines), draw_area);

                            let legend = vec![
                                Line::styled(format!("● {:>8.2}  {:>5.1}%", values[0], p[0]),
                                    Style::default().fg(c1)),
                                Line::styled(format!("  {}", bar(p[0])),
                                    Style::default().fg(c1)),
                                Line::raw(""),
                                Line::styled(format!("● {:>8.2}  {:>5.1}%", values[1], p[1]),
                                    Style::default().fg(c2)),
                                Line::styled(format!("  {}", bar(p[1])),
                                    Style::default().fg(c2)),
                                Line::raw(""),
                                Line::styled(format!("● {:>8.2}  {:>5.1}%", values[2], p[2]),
                                    Style::default().fg(c3)),
                                Line::styled(format!("  {}", bar(p[2])),
                                    Style::default().fg(c3)),
                                Line::raw(""),
                                Line::raw(format!("total {:>8.2}", total)),
                            ];
                            f.render_widget(
                                Paragraph::new(legend).block(
                                    Block::default().padding(Padding::new(1,0,1,0))
                                ),
                                chunks[1],
                            );
                        } else {
                            let help = Paragraph::new(
                                "Waiting for data… Expected first output line with 3 numbers, e.g. '10 20 30'.",
                            )
                            .block(chart_block)
                            .wrap(Wrap { trim: false });
                            f.render_widget(help, size);
                        }
                    }
                    RenderMode::BarChart => {
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        if let Some(bars_data) = parse_bar_values(&text) {
                            f.render_widget(chart_block.clone(), size);
                            let inner = chart_block.inner(size);
                            let safe = Rect::new(
                                inner.x + 2,
                                inner.y + 1,
                                inner.width.saturating_sub(4),
                                inner.height.saturating_sub(2),
                            );

                            let max_v = bars_data.iter().map(|(_, v)| *v).max().unwrap_or(1).max(1);
                            let n = bars_data.len() as u16;
                            // Fit all bars into the available width; minimum bar width of 3.
                            // n == 0 is handled by the outer branch, so the division below
                            // (guarded by the `if n == 0` check) can't divide by zero.
                            #[allow(clippy::manual_checked_ops)]
                            let bar_width = if n == 0 { 7 } else {
                                ((safe.width.saturating_sub(n.saturating_sub(1))) / n).clamp(3, 15)
                            };
                            let bar_gap = if bar_width <= 4 { 1 } else { 2 };

                            // BarChart::data takes &[(&str, u64)]; x-axis labels from parsed words.
                            let data_refs: Vec<(&str, u64)> = bars_data.iter().map(|(l, v)| (l.as_str(), *v)).collect();

                            let bars = BarChart::default()
                                .data(&data_refs)
                                .max(max_v)
                                .bar_width(bar_width)
                                .bar_gap(bar_gap)
                                .bar_style(Style::default().fg(theme::Colors::COLOUR_GOOD_0))
                                .value_style(Style::default().fg(theme::Colors::COLOUR_GOOD_0).bg(theme::Colors::COLOUR_GOOD_0))
                                .label_style(Style::default().fg(Color::Gray));

                            f.render_widget(bars, safe);
                        } else {
                            let help = Paragraph::new(
                                "Waiting for data… Expected output with numbers, e.g. '10 20 30' or 'foo 10  bar 20'.",
                            )
                            .block(chart_block)
                            .wrap(Wrap { trim: false });
                            f.render_widget(help, size);
                        }
                    }
                    RenderMode::LineChart => {
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        let values: Vec<f64> = text
                            .split(|c: char| c.is_whitespace())
                            .filter_map(|s| s.parse::<f64>().ok())
                            .collect();

                        if values.is_empty() {
                            let help = Paragraph::new(
                                "Waiting for data… Expected output with numbers on separate lines or space-separated.",
                            )
                            .block(chart_block)
                            .wrap(Wrap { trim: false });
                            f.render_widget(help, size);
                        } else {
                            let points: Vec<(f64, f64)> = values
                                .iter()
                                .enumerate()
                                .map(|(i, v)| (i as f64, *v))
                                .collect();

                            let min_y = values.iter().fold(f64::INFINITY, |a, v| a.min(*v));
                            let max_y = values.iter().fold(f64::NEG_INFINITY, |a, v| a.max(*v));
                            let y_pad = if (max_y - min_y).abs() < f64::EPSILON {
                                1.0
                            } else {
                                (max_y - min_y) * 0.1
                            };

                            let x_max = (values.len().saturating_sub(1)).max(1) as f64;
                            let mid_x = (x_max / 2.0).round();
                            let mid_y = (min_y + max_y) / 2.0;

                            let datasets = vec![
                                Dataset::default()
                                    .name("series")
                                    .style(Style::default().fg(theme::Colors::COLOUR_GOOD_0))
                                    .graph_type(GraphType::Line)
                                    .data(&points),
                            ];

                            let chart = Chart::new(datasets)
                                .block(chart_block)
                                .x_axis(
                                    Axis::default()
                                        .style(Style::default().fg(Color::Gray))
                                        .bounds([0.0, x_max])
                                        .labels(vec![
                                            Span::raw("0"),
                                            Span::raw(format!("{mid_x:.0}")),
                                            Span::raw(format!("{x_max:.0}")),
                                        ]),
                                )
                                .y_axis(
                                    Axis::default()
                                        .style(Style::default().fg(Color::Gray))
                                        .bounds([min_y - y_pad, max_y + y_pad])
                                        .labels(vec![
                                            Span::raw(format!("{:.2}", min_y)),
                                            Span::raw(format!("{:.2}", mid_y)),
                                            Span::raw(format!("{:.2}", max_y)),
                                        ]),
                                );

                            f.render_widget(chart, size);
                        }
                    }
                    RenderMode::StatusCircle => {
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        f.render_widget(chart_block.clone(), size);
                        let inner = chart_block.inner(size);
                        let safe = Rect::new(
                            inner.x + 1,
                            inner.y + 1,
                            inner.width.saturating_sub(2),
                            inner.height.saturating_sub(2),
                        );

                        if let Some(status) = parse_status_value(&text) {
                            let values = status_to_circle_values(status);
                            let lines = render_circle(
                                safe.width,
                                safe.height,
                                values,
                                theme::CircleRender::SMOOTH,
                                status_good_colour,
                                theme::Colors::COLOUR_WARN_0,
                                status_bad_colour,
                            );
                            f.render_widget(Paragraph::new(lines), safe);
                        } else {
                            let help = Paragraph::new(
                                "Waiting for status… Expected command to output an exit code (0 = healthy).",
                            )
                            .wrap(Wrap { trim: false });
                            f.render_widget(help, safe);
                        }
                    }
                    RenderMode::StatusRect => {
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        f.render_widget(chart_block.clone(), size);
                        let inner = chart_block.inner(size);

                        if let Some(status) = parse_status_value(&text) {
                            let pad_x = inner.width / 20; // ~5%
                            let pad_y = 1u16.max(inner.height / 20); // at least 1 row
                            let padded = Rect::new(
                                inner.x + pad_x,
                                inner.y + pad_y,
                                inner.width.saturating_sub(pad_x * 2),
                                inner.height.saturating_sub(pad_y * 2),
                            );
                            let rect_fill = Block::default().style(Style::default().bg(status_color(status, status_good_colour, status_bad_colour)));
                            f.render_widget(rect_fill, padded);
                        } else {
                            let help = Paragraph::new(
                                "Waiting for status… Expected command to output an exit code (0 = healthy).",
                            )
                            .wrap(Wrap { trim: false });
                            f.render_widget(help, inner);
                        }
                    }
                    RenderMode::StatusCircleWithText => {
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        f.render_widget(chart_block.clone(), size);
                        let inner = chart_block.inner(size);

                        // Split screen: status pie on left, text on right
                        let chunks = Layout::default()
                            .direction(Direction::Horizontal)
                            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                            .split(inner);

                        if let Some(status) = parse_status_value(&text) {
                            let values = status_to_circle_values(status);
                            let lines = render_circle(
                                chunks[0].width,
                                chunks[0].height,
                                values,
                                theme::CircleRender::SMOOTH,
                                status_good_colour,
                                theme::Colors::COLOUR_WARN_0,
                                status_bad_colour,
                            );
                            f.render_widget(Paragraph::new(lines), chunks[0]);

                            let label = status_with_text_label(status, append_text.as_deref());
                            let text_para = Paragraph::new(
                                Line::styled(label, Style::default().fg(Color::White).add_modifier(Modifier::BOLD)),
                            )
                            .centered();
                            f.render_widget(text_para, chunks[1]);
                        } else {
                            // Text on the right when no status could be parsed.
                            let styled_lines: Vec<Line<'static>> = raw_lines.iter()
                                .map(|l| ansi_to_line(l))
                                .collect();
                            let text_para = Paragraph::new(styled_lines)
                                .wrap(Wrap { trim: false });
                            f.render_widget(text_para, chunks[1]);
                        }
                    }
                    RenderMode::StatusRectWithText => {
                        let mut chart_block = Block::default()
                            .title(Line::styled(format!(" {} ", script_name), Style::default().fg(title_colour)))
                            .borders(Borders::ALL)
                            .border_type(BorderType::Rounded)
                            .border_style(Style::default().fg(border_colour));
                        if let Some(ms) = watch_ms {
                            chart_block = chart_block.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                        }

                        f.render_widget(chart_block.clone(), size);
                        let inner = chart_block.inner(size);

                        if let Some(status) = parse_status_value(&text) {
                            // True overlay mode: fill full area with status color,
                            // then render big text centered on top of it.
                            let rect_fill = Block::default().style(Style::default().bg(status_color(status, status_good_colour, status_bad_colour)));
                            f.render_widget(rect_fill, inner);

                            if raw_lines.is_empty() {
                                let waiting = Paragraph::new(
                                    Line::styled("Waiting…", Style::default().fg(status_text_color(status, status_good_colour, status_bad_colour)).add_modifier(Modifier::BOLD))
                                )
                                .centered();
                                f.render_widget(waiting, inner);
                            } else {
                                let text_style = Style::default()
                                    .fg(status_text_color(status, status_good_colour, status_bad_colour))
                                    .add_modifier(Modifier::BOLD);

                                if let Some(icon_mask) = status_rect_static_icon
                                    .as_deref()
                                    .and_then(static_icon_mask_for_token)
                                {
                                    // Scale into twice the vertical budget, then halve with
                                    // half-block characters — same as PixelSize::HalfHeight.
                                    let raw = render_scaled_icon_lines(icon_mask, inner.width, inner.height.saturating_mul(2));
                                    let icon_lines = render_half_height_pass(raw);
                                    let icon_w = icon_lines
                                        .iter()
                                        .map(|l| l.chars().count() as u16)
                                        .max()
                                        .unwrap_or(1)
                                        .min(inner.width);
                                    let icon_h = (icon_lines.len() as u16).min(inner.height);
                                    let icon_area = Rect::new(
                                        inner.x + inner.width.saturating_sub(icon_w) / 2,
                                        inner.y + inner.height.saturating_sub(icon_h) / 2,
                                        icon_w,
                                        icon_h,
                                    );
                                    let lines: Vec<Line<'static>> = icon_lines
                                        .iter()
                                        .map(|l| Line::styled((*l).to_string(), text_style))
                                        .collect();
                                    let icon = Paragraph::new(lines)
                                        .alignment(Alignment::Center)
                                        .wrap(Wrap { trim: false });
                                    f.render_widget(icon, icon_area);
                                } else {
                                    let display_str = match status_rect_static_text.as_deref() {
                                        Some(static_text) => static_text.to_string(),
                                        None if status == 0 && status_rect_conditional_cmd.is_some() => {
                                            conditional_text.lock().unwrap().clone()
                                                .unwrap_or_else(|| "...".to_string())
                                        }
                                        None => status_with_text_label(status, append_text.as_deref()),
                                    };
                                    let big_lines: Vec<Line<'static>> = vec![Line::raw(display_str)];

                                    let area = inner;
                                    let line_count = big_lines.len() as u16;
                                    let max_chars = big_lines.iter()
                                        .map(|l| l.width() as u16)
                                        .max()
                                        .unwrap_or(1);
                                    let needed_h = (line_count * 4).min(area.height);
                                    let needed_w = (max_chars * 8).min(area.width);
                                    let centered = Rect::new(
                                        area.x + area.width.saturating_sub(needed_w) / 2,
                                        area.y + area.height.saturating_sub(needed_h) / 2,
                                        needed_w,
                                        needed_h,
                                    );

                                    let big_text = BigText::builder()
                                        .pixel_size(PixelSize::HalfHeight)
                                        .centered()
                                        .style(text_style)
                                        .lines(big_lines)
                                        .build();
                                    f.render_widget(big_text, centered);
                                }
                            }
                        } else {
                            let help = Paragraph::new(
                                "Waiting for status… Expected command to output an exit code (0 = healthy).",
                            )
                            .wrap(Wrap { trim: false });
                            f.render_widget(help, inner);
                        }
                    }
                    RenderMode::Big => {
                        let block = if frame {
                            let mut b = Block::default()
                                .title(Line::styled(script_name.as_str(), Style::default().fg(title_colour)))
                                .borders(Borders::ALL)
                                .border_type(BorderType::Rounded)
                                .border_style(Style::default().fg(border_colour))
                                .padding(Padding::new(1, 1, 1, 0));
                            if let Some(ms) = watch_ms {
                                b = b.title_top(Line::from(format!(" ⟳ {} ", format_watch_label(ms))).alignment(Alignment::Right));
                            }
                            b
                        } else {
                            Block::default()
                        };

                        if raw_lines.is_empty() {
                            let waiting = Paragraph::new("Waiting…").block(block);
                            f.render_widget(waiting, size);
                        } else {
                            // Use the last non-empty line as the value to display.
                            let display_str = apply_append_text(
                                &raw_lines,
                                append_text.as_deref(),
                            ).unwrap_or_default();
                            let big_lines: Vec<Line<'static>> = vec![Line::raw(display_str)];

                            let area = if frame { block.inner(size) } else { size };
                            if frame {
                                f.render_widget(block, size);
                            }

                            // Each glyph is 8×4 terminal cells at PixelSize::HalfHeight.
                            let line_count = big_lines.len() as u16;
                            let max_chars = big_lines.iter()
                                .map(|l| l.width() as u16)
                                .max()
                                .unwrap_or(1);
                            let needed_h = (line_count * 4).min(area.height);
                            let needed_w = (max_chars * 8).min(area.width);
                            let centered = Rect::new(
                                area.x + area.width.saturating_sub(needed_w) / 2,
                                area.y + area.height.saturating_sub(needed_h) / 2,
                                needed_w,
                                needed_h,
                            );

                            let big_text = BigText::builder()
                                .pixel_size(PixelSize::HalfHeight)
                                .centered()
                                .style(Style::default().fg(theme::Colors::COLOUR_GOOD_0))
                                .lines(big_lines)
                                .build();
                            f.render_widget(big_text, centered);
                        }
                    }
                }
            })?;
            needs_redraw = false;
        }

        // Block for up to 500ms when nothing needs drawing; drain all queued events.
        let _ = event::poll(Duration::from_millis(if needs_redraw { 0 } else { 500 }));
        let mut exit_requested = false;
        while let Ok(true) = event::poll(Duration::from_millis(0)) {
            if let Ok(ev) = event::read() {
                match ev {
                    Event::Key(key) => match key.code {
                        _ if is_exit_key(key.code, key.modifiers) => {
                            exit_requested = true;
                            break;
                        }
                        KeyCode::Char('v') => {
                            show_info = !show_info;
                            needs_redraw = true;
                        }

                        KeyCode::Up => {
                            if select_mode && matches!(render_mode, RenderMode::Text) {
                                let line_count = {
                                    let buf = buffer.lock().unwrap();
                                    let current_run = active_run.load(Ordering::SeqCst);
                                    display_lines_for_run(&buf, current_run).len()
                                };
                                if line_count > 0 {
                                    auto_scroll = false;
                                    let next = match selected_index {
                                        Some(current) => {
                                            let current = current.min(line_count - 1);
                                            if current == 0 {
                                                line_count - 1
                                            } else {
                                                current - 1
                                            }
                                        }
                                        None => line_count - 1,
                                    };
                                    selected_index = Some(next);
                                    scroll_offset = (next as u16).min(max_scroll);
                                }
                            } else {
                                if auto_scroll {
                                    scroll_offset = max_scroll;
                                }
                                auto_scroll = false;
                                scroll_offset = scroll_offset.saturating_sub(1);
                            }
                            needs_redraw = true;
                        }
                        KeyCode::Down => {
                            if select_mode && matches!(render_mode, RenderMode::Text) {
                                let line_count = {
                                    let buf = buffer.lock().unwrap();
                                    let current_run = active_run.load(Ordering::SeqCst);
                                    display_lines_for_run(&buf, current_run).len()
                                };
                                if line_count > 0 {
                                    auto_scroll = false;
                                    let next = match selected_index {
                                        Some(current) => {
                                            let current = current.min(line_count - 1);
                                            if current + 1 >= line_count {
                                                0
                                            } else {
                                                current + 1
                                            }
                                        }
                                        None => 0,
                                    };
                                    selected_index = Some(next);
                                    scroll_offset = (next as u16).min(max_scroll);
                                }
                            } else {
                                if auto_scroll {
                                    scroll_offset = max_scroll;
                                }
                                auto_scroll = false;
                                scroll_offset = scroll_offset.saturating_add(1);
                            }
                            needs_redraw = true;
                        }
                        KeyCode::PageUp => {
                            if auto_scroll {
                                scroll_offset = max_scroll;
                            }
                            auto_scroll = false;
                            scroll_offset = scroll_offset.saturating_sub(10);
                            needs_redraw = true;
                        }
                        KeyCode::PageDown => {
                            if auto_scroll {
                                scroll_offset = max_scroll;
                            }
                            auto_scroll = false;
                            scroll_offset = scroll_offset.saturating_add(10);
                            needs_redraw = true;
                        }
                        KeyCode::End => {
                            auto_scroll = true;
                            needs_redraw = true;
                        }
                        KeyCode::Esc => {
                            if select_mode && matches!(render_mode, RenderMode::Text) {
                                selected_index = None;
                                auto_scroll = true;
                                needs_redraw = true;
                            }
                        }

                        // Re-run script on 'r'
                        KeyCode::Char('r') if !stdin_mode => {
                            buffer.lock().unwrap().clear();
                            let next_run = active_run.fetch_add(1, Ordering::SeqCst) + 1;
                            spawn_script(
                                &script_args,
                                Arc::clone(&buffer),
                                Arc::clone(&active_run),
                                next_run,
                                update_tx.clone(),
                                done_tx.clone(),
                            );
                            running_run = Some(next_run);
                            last_watch_launch = Instant::now();
                            needs_redraw = true;
                        }

                        _ => {}
                    },
                    Event::Resize(_, _) => {
                        needs_redraw = true;
                    }
                    _ => {}
                }
            }
        }

        if exit_requested {
            break;
        }

        if update_rx.try_iter().next().is_some() {
            needs_redraw = true;
        }

        for finished_run in done_rx.try_iter() {
            if running_run == Some(finished_run) {
                running_run = None;
            }
            // When the primary command finishes, run the conditional command if
            // health succeeded, or clear the stale result if it failed.
            if let Some(ref cond_cmd) = status_rect_conditional_cmd {
                let raw = {
                    let buf = buffer.lock().unwrap();
                    display_lines_for_run(&buf, finished_run)
                };
                let last_text = raw
                    .iter()
                    .rev()
                    .find(|l| !strip_ansi(l).trim().is_empty())
                    .map(|l| strip_ansi(l).trim().to_string())
                    .unwrap_or_default();
                if parse_status_value(&last_text) == Some(0) {
                    let cmd = cond_cmd.clone();
                    let ct = Arc::clone(&conditional_text);
                    let utx = update_tx.clone();
                    thread::spawn(move || {
                        // Use zsh -i so .zshrc is sourced and shell functions
                        // (e.g. from k8sh) are available without extra wrapping.
                        let shell = std::env::var("SHELL").unwrap_or_else(|_| "zsh".to_string());
                        let result = std::process::Command::new(&shell)
                            .arg("-i")
                            .arg("-c")
                            .arg(&cmd)
                            .output()
                            .ok()
                            .and_then(|o| {
                                let raw = String::from_utf8_lossy(&o.stdout).to_string();
                                // Strip ANSI/OSC sequences (e.g. iTerm2 shell integration
                                // injects \e]1337;RemoteHost=... into stdout when zsh -i
                                // sources .zshrc).
                                let s = strip_ansi(&raw);
                                s.lines()
                                    .map(str::trim)
                                    .rfind(|l| !l.is_empty())
                                    .map(String::from)
                            });
                        *ct.lock().unwrap() = result;
                        let _ = utx.send(());
                    });
                } else {
                    *conditional_text.lock().unwrap() = None;
                }
            }
        }

        if let Some(interval_ms) = watch_ms {
            if !stdin_mode
                && running_run.is_none()
                && last_watch_launch.elapsed() >= Duration::from_millis(interval_ms)
            {
                let next_run = active_run.fetch_add(1, Ordering::SeqCst) + 1;
                // Clear buffer only once new data starts arriving (in push_output_line)
                // to avoid flashing "Waiting…" between runs.
                spawn_script(
                    &script_args,
                    Arc::clone(&buffer),
                    Arc::clone(&active_run),
                    next_run,
                    update_tx.clone(),
                    done_tx.clone(),
                );
                running_run = Some(next_run);
                last_watch_launch = Instant::now();
            }
        }
    }

    cleanup_terminal();
    Ok(())
}
