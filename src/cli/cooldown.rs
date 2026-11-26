//! Interactive Rate-Limit Cooldown Countdown UI.

use std::io::{self, IsTerminal, Write};
use std::thread;
use std::time::{Duration, Instant};

use colored::Colorize;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal;

use crate::config::settings::{DEFAULT_BOT_NAME, DEFAULT_USER_REAL_NAME};

const PROGRESS_BAR_WIDTH: usize = 20;
const POLL_INTERVAL_MS: u64 = 100;

/// RAII Guard ensuring terminal raw mode is always disabled on scope exit.
struct RawModeGuard {
    active: bool,
}

impl RawModeGuard {
    fn acquire_if_tty() -> Self {
        if !cfg!(test) && io::stdin().is_terminal() && io::stdout().is_terminal() {
            let active = terminal::enable_raw_mode().is_ok();
            Self { active }
        } else {
            Self { active: false }
        }
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        if self.active {
            let _ = terminal::disable_raw_mode();
            let _ = io::stdout().flush();
            self.active = false;
        }
    }
}

pub fn is_cancel_key_event(key: &KeyEvent) -> bool {
    if key.kind != KeyEventKind::Press {
        return false;
    }

    match key.code {
        KeyCode::Esc => true,
        KeyCode::Char('q') | KeyCode::Char('Q') => true,
        KeyCode::Char('c') | KeyCode::Char('C')
            if key.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            true
        }
        _ => false,
    }
}

pub fn format_countdown_line(remaining_secs: u64, total_secs: u64) -> String {
    let total = total_secs.max(1);
    let clamped_rem = remaining_secs.min(total);
    let elapsed = total.saturating_sub(clamped_rem);
    let filled = ((elapsed as f64 / total as f64) * PROGRESS_BAR_WIDTH as f64).round() as usize;
    let filled = filled.min(PROGRESS_BAR_WIDTH);

    let bar = if filled == 0 {
        format!(">{}", " ".repeat(PROGRESS_BAR_WIDTH.saturating_sub(1)))
    } else if filled >= PROGRESS_BAR_WIDTH {
        "=".repeat(PROGRESS_BAR_WIDTH)
    } else {
        format!(
            "{}>{}",
            "=".repeat(filled.saturating_sub(1)),
            " ".repeat(PROGRESS_BAR_WIDTH - filled)
        )
    };

    format!("  ( ˘ω˘ ) Menunggu cooldown: {clamped_rem}s tersisa... [{bar}]")
}

#[cfg(test)]
pub fn wait_with_interactive_countdown(duration: Duration, model_name: &str) -> bool {
    wait_with_interactive_countdown_for_user(
        duration,
        model_name,
        DEFAULT_BOT_NAME,
        DEFAULT_USER_REAL_NAME,
    )
}

pub fn wait_with_interactive_countdown_for_user(
    duration: Duration,
    model_name: &str,
    bot_name: &str,
    user_name: &str,
) -> bool {
    let bot = match bot_name.trim() {
        "" => DEFAULT_BOT_NAME,
        trimmed => trimmed,
    };
    let user = match user_name.trim() {
        "" => DEFAULT_USER_REAL_NAME,
        trimmed => trimmed,
    };
    let model_label = match model_name.trim() {
        "" => "model",
        trimmed => trimmed,
    };

    if duration.is_zero() {
        return true;
    }

    let total_secs = duration.as_secs().max(1);

    if !cfg!(test) {
        println!(
            "{}{}{}  {} {} Kuota RPM {} penuh, pending dulu yaa... {}",
            bot.bright_magenta().bold(),
            "@".bright_white().bold(),
            "waish".bright_magenta(),
            "( ˘ω˘ )".bright_magenta(),
            "~>".bright_black(),
            model_label.cyan(),
            "[Tekan 'q', 'Esc', atau Ctrl+C untuk batal]".bright_black()
        );
        let _ = io::stdout().flush();
    }

    let start = Instant::now();
    let effective_wait = if cfg!(test) {
        duration.min(Duration::from_millis(50))
    } else {
        duration
    };
    let deadline = start + effective_wait;
    let mut last_rendered_sec: Option<u64> = None;
    let mut cancelled = false;

    {
        let raw_guard = RawModeGuard::acquire_if_tty();

        // Drain residual stdin events from the previous REPL prompt to prevent false-positive cancellation.
        if raw_guard.active {
            while event::poll(Duration::ZERO).unwrap_or(false) {
                let _ = event::read();
            }
        }

        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            let rem_secs = (remaining.as_secs() + u64::from(remaining.subsec_nanos() > 0))
                .clamp(1, total_secs);

            if !cfg!(test) && last_rendered_sec != Some(rem_secs) {
                let line = format_countdown_line(rem_secs, total_secs);
                print!("\r\x1b[2K{}", line.yellow());
                let _ = io::stdout().flush();
                last_rendered_sec = Some(rem_secs);
            }

            let step = remaining.min(Duration::from_millis(POLL_INTERVAL_MS));
            if raw_guard.active {
                if matches!(event::poll(step), Ok(true)) {
                    if let Ok(Event::Key(key)) = event::read() {
                        if is_cancel_key_event(&key) {
                            cancelled = true;
                            break;
                        }
                    }
                }
            } else {
                thread::sleep(step);
            }
        }

        if !cfg!(test) {
            print!("\r\x1b[2K");
            let _ = io::stdout().flush();
        }
    }

    if cancelled {
        if !cfg!(test) {
            println!(
                "{}{}{}  {} {} Cooldown dibatalkan oleh {}.",
                bot.bright_magenta().bold(),
                "@".bright_white().bold(),
                "waish".bright_magenta(),
                "( •ˋ _ ˊ•)".bright_magenta(),
                "~>".bright_black(),
                user.cyan()
            );
            let _ = io::stdout().flush();
        }
        false
    } else {
        if !cfg!(test) {
            println!(
                "{}{}{}  {} {} Cooldown selesai! Melanjutkan permintaan tertunda...",
                bot.bright_magenta().bold(),
                "@".bright_white().bold(),
                "waish".bright_magenta(),
                "( •̀ ω •́ )✧".bright_magenta(),
                "~>".bright_black(),
            );
            let _ = io::stdout().flush();
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognizes_all_cancel_key_events_and_ignores_other_keys() {
        let press = |code: KeyCode, mods: KeyModifiers| KeyEvent::new(code, mods);
        assert!(is_cancel_key_event(&press(
            KeyCode::Char('q'),
            KeyModifiers::NONE
        )));
        assert!(is_cancel_key_event(&press(
            KeyCode::Char('Q'),
            KeyModifiers::SHIFT
        )));
        assert!(is_cancel_key_event(&press(KeyCode::Esc, KeyModifiers::NONE)));
        assert!(is_cancel_key_event(&press(
            KeyCode::Char('c'),
            KeyModifiers::CONTROL
        )));
        assert!(is_cancel_key_event(&press(
            KeyCode::Char('q'),
            KeyModifiers::CONTROL
        )));

        assert!(!is_cancel_key_event(&press(
            KeyCode::Char('c'),
            KeyModifiers::NONE
        )));
        assert!(!is_cancel_key_event(&press(
            KeyCode::Enter,
            KeyModifiers::NONE
        )));

        let mut release = KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE);
        release.kind = KeyEventKind::Release;
        assert!(!is_cancel_key_event(&release));
    }

    #[test]
    fn formats_pure_ascii_countdown_bar_with_kaomoji() {
        let start = format_countdown_line(32, 32);
        assert!(start.contains("( ˘ω˘ )"));
        assert!(start.contains("32s tersisa"));
        assert!(start.contains("[>"));

        let mid = format_countdown_line(16, 32);
        assert!(mid.contains("16s tersisa"));
        assert!(mid.contains("=========>"));

        let done = format_countdown_line(0, 32);
        assert!(done.contains("[====================]"));
    }

    #[test]
    fn zero_or_short_countdown_completes_cleanly_in_non_tty_test_mode() {
        assert!(wait_with_interactive_countdown(Duration::ZERO, "qwen-plus"));
        assert!(wait_with_interactive_countdown(
            Duration::from_millis(20),
            "qwen-plus"
        ));
    }
}
