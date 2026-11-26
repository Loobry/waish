//! Formatter box-drawing ASCII dan kaomoji selector.

use std::path::Path;

use colored::{ColoredString, Colorize};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

const MIN_INNER_WIDTH: usize = 30;
const FALLBACK_INNER_WIDTH: usize = 76;

/// Kaomoji state machine.
#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kaomoji {
    Greet,
    Ready,
    Wait,
    Busy,
    Music,
    Success,
    Fail,
    Confused,
    Warn,
    Angry,
    Farewell,
}

impl Kaomoji {
    pub fn as_str(self) -> &'static str {
        match self {
            Kaomoji::Greet => "( ^ _ ^ )",
            Kaomoji::Ready => "( •̀ ω •́ )✧",
            Kaomoji::Wait => "( - _ - ) . . .",
            Kaomoji::Busy => "( - _ - ) zZz",
            Kaomoji::Music => "( ˘ ▽ ˘ ) ♫",
            Kaomoji::Success => "( > - < ) v",
            Kaomoji::Fail => "(╥﹏╥)",
            Kaomoji::Confused => "(｡•́︿•̀｡)",
            Kaomoji::Warn => "( > < ) !",
            Kaomoji::Angry => "( •ˋ _ ˊ•)",
            Kaomoji::Farewell => "( ´ ▽ ` )ﾉ",
        }
    }
}

pub fn print_bot_line(bot_name: &str, mood: Kaomoji) {
    println!(
        "{}{}{}  {} {}",
        bot_name.bright_magenta().bold(),
        "@".bright_white().bold(),
        "waish".bright_magenta(),
        mood.as_str().bright_magenta(),
        "~>".bright_black()
    );
}

pub fn tilde(path: &Path) -> String {
    if let Some(rest) = dirs::home_dir().and_then(|home| path.strip_prefix(&home).ok()) {
        return format!("~/{}", rest.display());
    }
    path.display().to_string()
}

pub fn display_width(s: &str) -> usize {
    UnicodeWidthStr::width(s)
}

pub fn max_inner_width() -> usize {
    match crossterm::terminal::size() {
        Ok((cols, _)) if cols as usize > MIN_INNER_WIDTH + 6 => (cols as usize - 6).min(100),
        _ => FALLBACK_INNER_WIDTH,
    }
}

pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if display_width(line) <= width {
        return vec![line.to_string()];
    }

    let indent: String = line.chars().take_while(|c| c.is_whitespace()).collect();
    let indent_width = display_width(indent.as_str()).min(width / 2);

    let mut out = Vec::new();
    let mut current = " ".repeat(indent_width);
    let mut current_width = indent_width;

    for word in line.split_whitespace() {
        let word_width = display_width(word);
        let sep = usize::from(current_width > indent_width);

        if current_width + sep + word_width <= width {
            if sep == 1 {
                current.push(' ');
            }
            current.push_str(word);
            current_width += sep + word_width;
            continue;
        }

        if current_width > indent_width {
            out.push(std::mem::replace(&mut current, " ".repeat(indent_width)));
            current_width = indent_width;
        }

        for ch in word.chars() {
            let w = UnicodeWidthChar::width(ch).unwrap_or(0);
            if current_width + w > width {
                out.push(std::mem::replace(&mut current, " ".repeat(indent_width)));
                current_width = indent_width;
            }
            current.push(ch);
            current_width += w;
        }
    }

    if current_width > indent_width || out.is_empty() {
        out.push(current);
    }
    out
}

pub fn render_box<F>(title: &str, lines: &[String], indent: usize, style: F) -> String
where
    F: Fn(&str) -> ColoredString,
{
    let cap = max_inner_width().saturating_sub(indent).max(MIN_INNER_WIDTH);
    let title_width = display_width(title);

    let wrapped: Vec<String> = lines.iter().flat_map(|l| wrap_line(l, cap)).collect();
    let content_width = wrapped.iter().map(|l| display_width(l)).max().unwrap_or(0);
    let inner = content_width.max(title_width + 3).min(cap.max(title_width + 3));

    let pad = " ".repeat(indent);
    let mut out = String::new();

    let dashes = inner.saturating_sub(title_width + 2).max(1);
    out.push_str(&format!(
        "{pad}{}{}{}\n",
        "+-- ".magenta(),
        title.bold(),
        format!(" {}+", "-".repeat(dashes)).magenta()
    ));

    for line in &wrapped {
        let fill = inner.saturating_sub(display_width(line));
        out.push_str(&format!(
            "{pad}{} {}{} {}\n",
            "|".magenta(),
            style(line),
            " ".repeat(fill),
            "|".magenta()
        ));
    }

    out.push_str(&format!("{pad}{}", format!("+{}+", "-".repeat(inner + 2)).magenta()));
    out
}

pub fn render_plain_box(title: &str, lines: &[String], indent: usize) -> String {
    render_box(title, lines, indent, |l| l.normal())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strip_ansi(s: &str) -> String {
        let mut out = String::new();
        let mut in_esc = false;
        for c in s.chars() {
            match (in_esc, c) {
                (false, '\x1b') => in_esc = true,
                (true, 'm') => in_esc = false,
                (true, _) => {}
                (false, c) => out.push(c),
            }
        }
        out
    }

    #[test]
    fn box_lines_have_equal_width() {
        let lines = vec![
            "Companion : alice".to_string(),
            "Kaomoji   : ( •̀ ω •́ )✧".to_string(),
            "日本語のテキスト".to_string(),
        ];
        let rendered = strip_ansi(&render_plain_box("Ringkasan Pengaturan", &lines, 2));
        let widths: Vec<usize> = rendered.lines().map(display_width).collect();
        assert!(widths.windows(2).all(|w| w[0] == w[1]), "{rendered}\n{widths:?}");
    }

    #[test]
    fn wraps_long_lines() {
        let wrapped = wrap_line("- satu dua tiga empat lima enam tujuh delapan", 12);
        assert!(wrapped.iter().all(|l| display_width(l) <= 12), "{wrapped:?}");
        assert!(wrapped.len() > 1);
    }
}
