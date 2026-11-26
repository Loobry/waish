//! Safety Interceptor & Gatekeeper.

use std::io::{self, IsTerminal, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;

use colored::Colorize;
use regex::Regex;

use crate::cli::prompt::{print_bot_header, print_bot_inline, print_bot_inline_face};
use crate::cli::ui::Kaomoji;
use crate::config::settings::Settings;
use crate::wizard::i18n::WizardLang;

pub const CONFIRMATION_KEYWORD: &str = "konfirmasi";

pub static DANGEROUS_PATTERNS: &[(&str, &str)] = &[
    (
        r"rm\s+-(?:[a-zA-Z]*r[a-zA-Z]*[fF]|[a-zA-Z]*[fF][a-zA-Z]*r)[a-zA-Z]*\s+[/~]",
        "penghapusan rekursif paksa pada root (/) atau direktori home (~)",
    ),
    (
        r"rm\s+.*--no-preserve-root",
        "penghapusan root filesystem (--no-preserve-root)",
    ),
    (
        r"\bmkfs(?:\.[a-z0-9]+)?\s+",
        "pemformatan partisi/disk (mkfs)",
    ),
    (
        r"\bdd\s+.*of=/dev/(?:sd|nvme|vd|hd|loop|mapper)",
        "penimpaan blok mentah perangkat disk (dd of=/dev/...)",
    ),
    (
        r">\s*/dev/(?:sd|nvme|vd|hd)",
        "redireksi output langsung ke perangkat blok mentah (/dev/...)",
    ),
    (
        r":\(\)\s*\{\s*:\s*\|\s*:\s*&\s*\}\s*;?\s*:",
        "fork bomb yang melumpuhkan proses kernel",
    ),
    (
        r"\bchmod\s+-R\s+777\s+/",
        "perusakan izin akses seluruh sistem (chmod -R 777 /)",
    ),
    (
        r"\bpacman\s+-[RU][a-zA-Z]*\s+.*\b(?:glibc|systemd|linux)\b",
        "penghapusan/modifikasi paket inti sistem (glibc/systemd/linux)",
    ),
];

/// Hasil pemeriksaan Safety Interceptor.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SafetyVerdict {
    Safe,
    Confirmed { risk: &'static str },
    Blocked { risk: &'static str },
}

fn compiled_patterns() -> &'static Vec<(Regex, &'static str)> {
    static PATTERNS: OnceLock<Vec<(Regex, &'static str)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        DANGEROUS_PATTERNS
            .iter()
            .filter_map(|(pat, desc)| Regex::new(pat).ok().map(|re| (re, *desc)))
            .collect()
    })
}

pub fn detect_danger(command: &str) -> Option<&'static str> {
    let normalized = command.trim();
    compiled_patterns()
        .iter()
        .find_map(|(re, desc)| re.is_match(normalized).then_some(*desc))
}

pub fn is_confirmed_input(input: &str) -> bool {
    input.trim() == CONFIRMATION_KEYWORD
}

pub fn is_confirmed_input_for_lang(input: &str, lang: WizardLang) -> bool {
    input.trim() == lang.danger_confirm_keyword() || is_confirmed_input(input)
}

/// Hasil keputusan konfirmasi aksi pada Safety Gatekeeper (mendukung Inline Course-Correction).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfirmationDecision {
    Confirmed,
    Cancelled,
    Feedback(String),
}

pub fn parse_confirmation_input(raw: &str) -> ConfirmationDecision {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return ConfirmationDecision::Cancelled;
    }

    match trimmed.to_ascii_lowercase().as_str() {
        "y" | "yes" | "ya" | "はい" => ConfirmationDecision::Confirmed,
        "n" | "no" | "tidak" | "batal" | "cancel" | "いいえ" => ConfirmationDecision::Cancelled,
        _ => ConfirmationDecision::Feedback(trimmed.to_string()),
    }
}

#[cfg(test)]
pub fn is_yes_confirmation(input: &str) -> bool {
    parse_confirmation_input(input) == ConfirmationDecision::Confirmed
}

pub fn prompt_action_confirmation(
    action_type: &str,
    target_command: &str,
    language: &str,
) -> ConfirmationDecision {
    let lang = WizardLang::from_code(language);

    if cfg!(test) || !io::stdin().is_terminal() {
        return ConfirmationDecision::Cancelled;
    }

    let label = if action_type.trim().is_empty()
        || action_type.eq_ignore_ascii_case("shell")
        || action_type.eq_ignore_ascii_case("root")
        || action_type.eq_ignore_ascii_case("run_shell_command")
    {
        target_command.to_string()
    } else {
        format!("{action_type}: {target_command}")
    };

    let prompt = lang.action_confirm_prompt_styled(
        &Kaomoji::Warn.as_str().bright_magenta().to_string(),
        &label.yellow().bold().to_string(),
        &lang.inline_correction_hint_tag().cyan().bold().to_string(),
    );
    print!("  {prompt}");
    let _ = io::stdout().flush();

    let mut line = String::new();
    if io::stdin().read_line(&mut line).is_err() {
        return ConfirmationDecision::Cancelled;
    }

    parse_confirmation_input(&line)
}

pub fn confirm_root_execution(
    command: &str,
    settings: &Settings,
) -> io::Result<ConfirmationDecision> {
    if !settings.behavior.safety_interceptor {
        return Ok(ConfirmationDecision::Confirmed);
    }

    let lang = WizardLang::from_code(&settings.general.language);

    if cfg!(test) || !io::stdin().is_terminal() {
        print_bot_inline(settings, Kaomoji::Angry, lang.root_blocked_non_tty());
        return Ok(ConfirmationDecision::Cancelled);
    }

    let decision = prompt_action_confirmation("root", command, &settings.general.language);
    match &decision {
        ConfirmationDecision::Confirmed => {}
        ConfirmationDecision::Cancelled => {
            print_bot_inline(settings, Kaomoji::Angry, lang.root_cancelled());
        }
        ConfirmationDecision::Feedback(_) => {
            print_bot_inline_face(settings, "( ˘ω˘ )", lang.inline_correction_adjusting());
        }
    }
    Ok(decision)
}

pub fn inspect_and_confirm(command: &str, settings: &Settings) -> io::Result<SafetyVerdict> {
    if !settings.behavior.safety_interceptor {
        return Ok(SafetyVerdict::Safe);
    }

    let Some(risk) = detect_danger(command) else {
        return Ok(SafetyVerdict::Safe);
    };

    prompt_danger_confirmation(command, risk, settings)
}

pub fn detect_critical_path(path: &Path) -> Option<&'static str> {
    let mut clean = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                clean.pop();
            }
            other => clean.push(other.as_os_str()),
        }
    }

    let s = clean.to_string_lossy();
    if s == "/etc/fstab" {
        Some("file tabel partisi boot (/etc/fstab)")
    } else if s == "/etc/sudoers" || s.starts_with("/etc/sudoers.d/") || s == "/etc/sudoers.d" {
        Some("file konfigurasi hak akses root (/etc/sudoers*)")
    } else if s == "/boot" || s.starts_with("/boot/") {
        Some("partisi/direktori kernel & bootloader (/boot/*)")
    } else if s == "/dev" || s.starts_with("/dev/") {
        Some("perangkat sistem mentah (/dev/*)")
    } else if s == "/etc/passwd" || s == "/etc/shadow" || s == "/etc/gshadow" {
        Some("database kredensial akun Linux (/etc/passwd|/etc/shadow)")
    } else {
        None
    }
}

pub fn inspect_path_and_confirm(
    path: &Path,
    action_label: &str,
    settings: &Settings,
) -> io::Result<SafetyVerdict> {
    if !settings.behavior.safety_interceptor {
        return Ok(SafetyVerdict::Safe);
    }

    let Some(risk) = detect_critical_path(path) else {
        return Ok(SafetyVerdict::Safe);
    };

    let display = format!("{action_label} -> {}", path.display());
    prompt_danger_confirmation(&display, risk, settings)
}

fn prompt_danger_confirmation(
    target_display: &str,
    risk: &'static str,
    settings: &Settings,
) -> io::Result<SafetyVerdict> {
    let lang = WizardLang::from_code(&settings.general.language);
    print_bot_header(settings, Kaomoji::Warn);
    println!("{}", lang.danger_warning_header(&risk.yellow().to_string()));
    println!("  {} {}", ">".red().bold(), target_display.red().bold());
    println!();

    if cfg!(test) || !io::stdin().is_terminal() {
        print_bot_inline(settings, Kaomoji::Angry, lang.danger_blocked_non_tty());
        return Ok(SafetyVerdict::Blocked { risk });
    }

    let kw_colored = lang.danger_confirm_keyword().yellow().bold().to_string();
    print!("{}", lang.danger_type_keyword_prompt(&kw_colored));
    io::stdout().flush()?;

    let mut line = String::new();
    io::stdin().read_line(&mut line)?;

    if is_confirmed_input_for_lang(&line, lang) {
        print_bot_inline(settings, Kaomoji::Warn, lang.danger_confirmed());
        Ok(SafetyVerdict::Confirmed { risk })
    } else {
        print_bot_inline(settings, Kaomoji::Angry, lang.danger_cancelled());
        Ok(SafetyVerdict::Blocked { risk })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_all_dangerous_command_patterns() {
        let dangerous = [
            "rm -rf /",
            "sudo rm -rf ~/.config",
            "rm -fr /etc",
            "rm -rf --no-preserve-root /",
            "sudo mkfs.ext4 /dev/sda1",
            "mkfs -t vfat /dev/sdb1",
            "sudo dd if=/dev/zero of=/dev/sda bs=1M",
            "dd if=arch.iso of=/dev/nvme0n1",
            "echo test > /dev/sda",
            ":(){ :|:& };:",
            "sudo chmod -R 777 /",
            "sudo pacman -Rdd glibc",
            "sudo pacman -Rns systemd",
            "sudo pacman -R linux",
        ];

        for cmd in dangerous {
            assert!(
                detect_danger(cmd).is_some(),
                "expected dangerous pattern match for: {cmd}"
            );
        }
    }

    #[test]
    fn allows_safe_commands() {
        let safe = [
            "ls -la",
            "pacman -Qi neovim",
            "sudo pacman -S --noconfirm neovim",
            "rm -rf ./target/debug",
            "chmod 755 script.sh",
            "df -h",
        ];

        for cmd in safe {
            assert_eq!(
                detect_danger(cmd),
                None,
                "expected safe command not to match: {cmd}"
            );
        }
    }

    #[test]
    fn confirmation_requires_exact_keyword() {
        assert!(is_confirmed_input("konfirmasi"));
        assert!(is_confirmed_input("  konfirmasi\n"));
        assert!(!is_confirmed_input("KONFIRMASI"));
        assert!(!is_confirmed_input("ya"));
        assert!(!is_confirmed_input("y"));
        assert!(!is_confirmed_input(""));

        assert!(is_confirmed_input_for_lang("confirm", WizardLang::En));
        assert!(is_confirmed_input_for_lang("konfirmasi", WizardLang::Id));
        assert!(is_confirmed_input_for_lang("confirm", WizardLang::Ja));
        assert!(!is_confirmed_input_for_lang("yes", WizardLang::En));
    }

    #[test]
    fn root_confirmation_accepts_only_yes_variants() {
        assert!(is_yes_confirmation("y"));
        assert!(is_yes_confirmation("Y\n"));
        assert!(is_yes_confirmation(" yes "));
        assert!(is_yes_confirmation("ya"));
        assert!(!is_yes_confirmation("n"));
        assert!(!is_yes_confirmation("no"));
        assert!(!is_yes_confirmation(""));
        assert!(!is_yes_confirmation("batal"));
    }

    #[test]
    fn prompt_action_confirmation_parses_confirmed_cancelled_and_inline_feedback() {
        // Confirmed: "y", "Y", "yes", "ya"
        for ok in ["y", "Y", "yes", "YES", "ya", "  ya\n"] {
            assert_eq!(
                parse_confirmation_input(ok),
                ConfirmationDecision::Confirmed,
                "expected Confirmed for {ok:?}"
            );
        }

        // Cancelled: "n", "N", "no", "tidak", "batal", "" (Enter kosong)
        for cancel in ["n", "N", "no", "tidak", "batal", "", "   \n"] {
            assert_eq!(
                parse_confirmation_input(cancel),
                ConfirmationDecision::Cancelled,
                "expected Cancelled for {cancel:?}"
            );
        }

        // Feedback: teks koreksi spesifik
        assert_eq!(
            parse_confirmation_input("coba pake sudo pacman aja"),
            ConfirmationDecision::Feedback("coba pake sudo pacman aja".to_string())
        );
        assert_eq!(
            parse_confirmation_input("  jangan install itu, cek versi bin di repo\n"),
            ConfirmationDecision::Feedback(
                "jangan install itu, cek versi bin di repo".to_string()
            )
        );

        // Non-TTY / mode unit test selalu menolak secara aman tanpa hang.
        assert_eq!(
            prompt_action_confirmation("root", "yay -Syu", "id"),
            ConfirmationDecision::Cancelled
        );
    }

    #[test]
    fn detects_critical_system_paths() {
        assert!(detect_critical_path(Path::new("/etc/fstab")).is_some());
        assert!(detect_critical_path(Path::new("/etc/sudoers")).is_some());
        assert!(detect_critical_path(Path::new("/etc/sudoers.d/wheel")).is_some());
        assert!(detect_critical_path(Path::new("/boot/grub/grub.cfg")).is_some());
        assert!(detect_critical_path(Path::new("/dev/nvme0n1")).is_some());
        assert!(detect_critical_path(Path::new("/etc/./fstab")).is_some());
        assert_eq!(detect_critical_path(Path::new("/home/steve/.config/waybar/style.css")), None);
        assert_eq!(detect_critical_path(Path::new("/etc/environment")), None);
    }
}
