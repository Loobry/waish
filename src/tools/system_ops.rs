//! System clipboard access, package ownership lookup, post-install binary inspection, and Git secret shield.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;

use colored::Colorize;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::cli::prompt::print_bot_header;
use crate::cli::ui::{self, Kaomoji};
use crate::config::settings::Settings;

const MAX_CLIPBOARD_CHARS: usize = 16_000;

static SECRET_PATTERNS: &[(&str, &str)] = &[
    ("Google API Key (AQ.)", r"AQ\.[A-Za-z0-9_-]{30,}"),
    ("Google API Key (AIza)", r"AIza[0-9A-Za-z_-]{30,}"),
    ("Anthropic API Key", r"sk-ant-[A-Za-z0-9_-]{30,}"),
    ("OpenAI / Secret Key", r"sk-[A-Za-z0-9_-]{30,}"),
    ("GitHub Classic Token", r"ghp_[A-Za-z0-9]{36}"),
    ("GitHub Fine-Grained Token", r"github_pat_[A-Za-z0-9_]{22,}"),
    (
        "Private Key Block",
        r"-----BEGIN (?:RSA |OPENSSH |EC |DSA )?PRIVATE KEY-----",
    ),
];

static SENSITIVE_FILENAMES: &[&str] = &[".env", "id_rsa", "id_ed25519", "id_ecdsa", "id_dsa"];

/// Potential secret or sensitive file finding detected in staged Git changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretFinding {
    pub category: &'static str,
    pub detail: String,
}

pub fn execute_read_clipboard() -> crate::Result<String> {
    let prefer_x11 = std::env::var("XDG_SESSION_TYPE")
        .map(|s| s.eq_ignore_ascii_case("x11"))
        .unwrap_or(false)
        && std::env::var("WAYLAND_DISPLAY").is_err();

    let (cmd_name, args): (&str, &[&str]) = if prefer_x11 {
        ("xclip", &["-o", "-selection", "clipboard"])
    } else if crate::wizard::detector::which("wl-paste").is_some() {
        ("wl-paste", &["--no-newline"])
    } else if crate::wizard::detector::which("xclip").is_some() {
        ("xclip", &["-o", "-selection", "clipboard"])
    } else {
        ("wl-paste", &["--no-newline"])
    };

    let output = Command::new(cmd_name)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| {
            format!(
                "Gagal menjalankan `{cmd_name}` (pasang `wl-clipboard` di Wayland atau `xclip` di X11): {e}"
            )
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let detail = if stderr.is_empty() {
            "clipboard kosong atau tidak berisi teks".to_string()
        } else {
            stderr
        };
        return Err(format!("`{cmd_name}` tidak dapat membaca clipboard: {detail}").into());
    }

    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    if text.trim().is_empty() {
        return Ok("(clipboard saat ini kosong)".to_string());
    }

    let char_count = text.chars().count();
    if char_count > MAX_CLIPBOARD_CHARS {
        let truncated: String = text.chars().take(MAX_CLIPBOARD_CHARS).collect();
        Ok(format!(
            "{truncated}\n... [clipboard dipotong pada {MAX_CLIPBOARD_CHARS} dari {char_count} karakter]"
        ))
    } else {
        Ok(text)
    }
}

/// Arguments for `find_package_owner`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FindPackageOwnerArgs {
    pub query: String,
}

impl FindPackageOwnerArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON find_package_owner tidak valid: {e}"))?;
        parsed.query = parsed.query.trim().to_string();
        if parsed.query.is_empty() {
            return Err("field `query` kosong".to_string());
        }
        Ok(parsed)
    }
}

pub fn detect_missing_command_or_lib(output: &str) -> Option<String> {
    static PATTERNS: OnceLock<Vec<Regex>> = OnceLock::new();
    let regexes = PATTERNS.get_or_init(|| {
        [
            r"shared libraries:\s*([A-Za-z0-9_.+-]+\.so(?:\.[0-9]+)*):",
            r"(?i)unknown command:\s*([A-Za-z0-9_.+-]+)",
            r"command not found:\s*([A-Za-z0-9_.+-]+)",
            r"(?:^|:\s*)([A-Za-z0-9_.+-]+):\s*command not found",
            r"(?:^|:\s*)([A-Za-z0-9_.+-]+):\s*not found",
        ]
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
    });

    for line in output.lines() {
        let trimmed = line.trim();
        for re in regexes {
            if let Some(name) = re
                .captures(trimmed)
                .and_then(|cap| cap.get(1))
                .map(|m| m.as_str().trim())
                .filter(|s| !s.is_empty())
            {
                return Some(name.to_string());
            }
        }
    }
    None
}

pub fn execute_find_package_owner(query: &str, aur_helper: &str) -> crate::Result<String> {
    let clean_query = query.trim();
    if clean_query.is_empty() {
        return Err("Query nama command atau library tidak boleh kosong.".into());
    }

    let output = Command::new("pacman")
        .args(["-F", clean_query])
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Gagal menjalankan `pacman -F {clean_query}`: {e}"))?;

    let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

    if !output.status.success() || stdout.is_empty() {
        let hint = if stderr.contains("database") || stderr.is_empty() {
            "Database files pacman mungkin belum disinkronkan (jalankan `sudo pacman -Fy` dulu) atau paket tidak ada di repo resmi."
        } else {
            stderr.as_str()
        };
        return Ok(format!(
            "Tidak ditemukan paket repo resmi untuk `{clean_query}`. Info: {hint}"
        ));
    }

    let helper = if aur_helper.trim().is_empty() {
        "pacman"
    } else {
        aur_helper.trim()
    };

    let mut lines: Vec<&str> = stdout.lines().take(20).collect();
    if stdout.lines().count() > 20 {
        lines.push("... [hasil lainnya disingkat]");
    }

    Ok(format!(
        "Hasil `pacman -F {clean_query}`:\n{}\n\nRekomendasi instalasi: `{helper} -S <nama_paket>` (atau `sudo pacman -S <nama_paket>`).",
        lines.join("\n")
    ))
}

pub fn extract_installed_packages_from_command(command: &str) -> Vec<String> {
    let mut packages = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for segment in command.split([';', '|', '&', '\n']) {
        let tokens: Vec<&str> = segment
            .split_whitespace()
            .map(|t| t.trim_matches(|c| c == '\'' || c == '"'))
            .filter(|t| !t.is_empty())
            .collect();
        if tokens.is_empty() {
            continue;
        }

        let mut idx = 0usize;
        while idx < tokens.len() {
            let tok = tokens[idx];
            if tok.contains('=') && !tok.starts_with('-') && !tok.starts_with('/') {
                idx += 1;
                continue;
            }
            let base = Path::new(tok)
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or(tok);
            if matches!(base, "sudo" | "doas" | "env" | "command" | "time") {
                idx += 1;
                continue;
            }
            break;
        }
        if idx >= tokens.len() {
            continue;
        }

        let pm_bin = Path::new(tokens[idx])
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or(tokens[idx]);
        if !matches!(pm_bin, "pacman" | "yay" | "paru" | "trizen" | "pikaur") {
            continue;
        }

        let mut has_sync_install = false;
        let mut is_non_install_sync = false;
        let mut candidate_pkgs = Vec::new();
        let mut skip_next = false;

        for &arg in &tokens[idx + 1..] {
            if skip_next {
                skip_next = false;
                continue;
            }
            if arg.starts_with("--") {
                if matches!(
                    arg,
                    "--config"
                        | "--root"
                        | "--dbpath"
                        | "--cachedir"
                        | "--arch"
                        | "--assume-installed"
                        | "--ignore"
                        | "--ignoregroup"
                        | "--overwrite"
                ) {
                    skip_next = true;
                }
                if arg == "--sync" {
                    has_sync_install = true;
                } else if matches!(arg, "--search" | "--info" | "--clean" | "--list" | "--groups")
                {
                    is_non_install_sync = true;
                }
                continue;
            }
            if let Some(short) = arg.strip_prefix('-') {
                if short.contains('S') {
                    has_sync_install = true;
                    if short.chars().any(|c| matches!(c, 's' | 'i' | 'c' | 'l' | 'g' | 'p')) {
                        is_non_install_sync = true;
                    }
                }
                continue;
            }

            let after_repo = arg.rsplit('/').next().unwrap_or(arg).trim();
            let pkg_name = after_repo
                .split(['<', '>', '='])
                .next()
                .unwrap_or(after_repo)
                .trim();
            if !pkg_name.is_empty()
                && pkg_name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'+' | b'@'))
            {
                candidate_pkgs.push(pkg_name.to_string());
            }
        }

        if has_sync_install && !is_non_install_sync {
            for pkg in candidate_pkgs {
                if seen.insert(pkg.clone()) {
                    packages.push(pkg);
                }
            }
        }
    }

    packages
}

pub fn parse_usr_bin_paths_from_pacman_ql(ql_output: &str) -> Vec<String> {
    let mut bins = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for line in ql_output.lines() {
        let mut parts = line.split_whitespace();
        let _pkg = parts.next();
        let Some(path) = parts.next() else {
            continue;
        };
        if path
            .strip_prefix("/usr/bin/")
            .is_some_and(|name| !name.is_empty() && !name.contains('/'))
            && seen.insert(path.to_string())
        {
            bins.push(path.to_string());
        }
    }
    bins.sort();
    bins
}

pub fn inspect_installed_package_binaries(packages: &[String]) -> Option<String> {
    if packages.is_empty() {
        return None;
    }

    const MAX_INSPECTED_PKGS: usize = 5;
    const MAX_BINS_PER_PKG: usize = 40;
    let mut sections = Vec::new();

    for pkg in packages.iter().take(MAX_INSPECTED_PKGS) {
        let Ok(out) = Command::new("pacman")
            .args(["-Ql", pkg])
            .stdin(Stdio::null())
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let stdout = String::from_utf8_lossy(&out.stdout);
        let bins = parse_usr_bin_paths_from_pacman_ql(&stdout);
        if bins.is_empty() {
            continue;
        }

        let mut lines = vec![format!("Paket `{pkg}` ({} binary resmi):", bins.len())];
        for bin in bins.iter().take(MAX_BINS_PER_PKG) {
            lines.push(format!("- {bin}"));
        }
        if bins.len() > MAX_BINS_PER_PKG {
            lines.push(format!(
                "- ... ({} binary lainnya tersedia via `pacman -Ql {pkg}`)",
                bins.len() - MAX_BINS_PER_PKG
            ));
        }
        sections.push(lines.join("\n"));
    }

    if sections.is_empty() {
        None
    } else {
        Some(format!(
            "[AUTO BINARY INSPECTION (pacman -Ql)]:\n{}\nGunakan path/nama binary resmi di atas untuk meluncurkan aplikasi; DILARANG menebak nama binary lain.",
            sections.join("\n")
        ))
    }
}

pub fn command_invokes_git_commit(command: &str) -> bool {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"(?:^|[;&|]\s*)(?:sudo\s+)?git\s+(?:-[A-Za-z]\s+\S+\s+)*commit\b")
            .expect("valid git commit regex")
    });
    re.is_match(command.trim())
}

pub fn extract_git_commit_workdir(command: &str) -> Option<PathBuf> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?:^|[;&|]\s*)(?:sudo\s+)?git\s+.*?-C\s+(?:'([^']+)'|"([^"]+)"|(\S+)).*?\bcommit\b"#,
        )
        .expect("valid git -C regex")
    });
    let cap = re.captures(command.trim())?;
    let raw = cap.get(1).or_else(|| cap.get(2)).or_else(|| cap.get(3))?.as_str();
    Some(PathBuf::from(raw))
}

fn compiled_secret_patterns() -> &'static Vec<(&'static str, Regex)> {
    static PATTERNS: OnceLock<Vec<(&'static str, Regex)>> = OnceLock::new();
    PATTERNS.get_or_init(|| {
        SECRET_PATTERNS
            .iter()
            .filter_map(|(cat, pat)| Regex::new(pat).ok().map(|re| (*cat, re)))
            .collect()
    })
}

pub fn scan_git_staging_texts(staged_files: &[String], staged_diff: &str) -> Vec<SecretFinding> {
    let mut findings = Vec::new();

    for file in staged_files {
        let base = Path::new(file)
            .file_name()
            .map(|n| n.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();

        let is_sensitive = SENSITIVE_FILENAMES.contains(&base.as_str())
            || base.starts_with(".env.")
            || base.ends_with(".pem")
            || base.ends_with(".key");

        if is_sensitive {
            findings.push(SecretFinding {
                category: "Sensitive File",
                detail: format!("File sensitif ter-stage untuk commit: `{file}`"),
            });
        }
    }

    let patterns = compiled_secret_patterns();
    let mut current_file = String::from("<staged>");

    for line in staged_diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            current_file = rest.trim().to_string();
            continue;
        }
        if line.starts_with("+++") || !line.starts_with('+') {
            continue;
        }
        let added_content = &line[1..];
        for (cat, re) in patterns {
            if let Some(m) = re.find(added_content) {
                let raw = m.as_str();
                if *cat == "OpenAI / Secret Key" && raw.starts_with("sk-ant-") {
                    continue;
                }
                let preview = mask_secret_preview(raw);
                findings.push(SecretFinding {
                    category: cat,
                    detail: format!("{cat} terdeteksi di `{current_file}` ({preview})"),
                });
            }
        }
    }

    findings
}

fn mask_secret_preview(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 10 {
        return "****".to_string();
    }
    let head: String = chars[..6].iter().collect();
    let tail: String = chars[chars.len() - 4..].iter().collect();
    format!("{head}****{tail}")
}

pub fn inspect_git_commit_shield(cwd: &Path, settings: &Settings) -> Result<(), Vec<SecretFinding>> {
    let files_out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--cached", "--name-only"])
        .stdin(Stdio::null())
        .output();

    let diff_out = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(["diff", "--cached", "-U0"])
        .stdin(Stdio::null())
        .output();

    let staged_files: Vec<String> = match files_out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_string)
            .collect(),
        _ => return Ok(()),
    };

    let staged_diff = match diff_out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).into_owned(),
        _ => String::new(),
    };

    let findings = scan_git_staging_texts(&staged_files, &staged_diff);
    if findings.is_empty() {
        return Ok(());
    }

    print_bot_header(settings, Kaomoji::Warn);
    println!(
        "{}",
        "BERHENTI! Git Shield mendeteksi potensi kebocoran secret/API key pada staged commit:"
            .red()
            .bold()
    );
    let box_lines: Vec<String> = findings
        .iter()
        .map(|f| format!("[{}] {}", f.category, f.detail))
        .collect();
    println!("{}", ui::render_plain_box("Git Shield Alert", &box_lines, 2));
    println!(
        "  {} Perintah `git commit` diblokir seketika. Hapus/unstage kredensial tersebut dulu yaa!",
        "( > < ) !".bright_magenta()
    );

    Err(findings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_missing_command_and_shared_library_errors() {
        assert_eq!(
            detect_missing_command_or_lib("bash: line 1: rg: command not found"),
            Some("rg".to_string())
        );
        assert_eq!(
            detect_missing_command_or_lib("fish: Unknown command: fzf"),
            Some("fzf".to_string())
        );
        assert_eq!(
            detect_missing_command_or_lib(
                "app: error while loading shared libraries: libcrypto.so.3: cannot open shared object file"
            ),
            Some("libcrypto.so.3".to_string())
        );
        assert_eq!(detect_missing_command_or_lib("build succeeded cleanly"), None);
    }

    #[test]
    fn detects_git_commit_invocations() {
        assert!(command_invokes_git_commit("git commit -m 'init'"));
        assert!(command_invokes_git_commit("git add . && git commit -m 'fix'"));
        assert!(command_invokes_git_commit("git -C /tmp/repo commit --amend"));
        assert!(!command_invokes_git_commit("git status"));
        assert!(!command_invokes_git_commit("git diff --cached"));
    }

    #[test]
    fn git_shield_catches_api_keys_tokens_and_sensitive_files() {
        let files = vec!["src/main.rs".to_string(), ".env".to_string(), "keys/id_ed25519".to_string()];
        let diff = concat!(
            "+++ b/src/config.rs\n",
            "+const GOOGLE_KEY: &str = \"AQ.",
            "AbCdEfGhIjKlMnOpQrStUvWxYz0123456789\";\n",
            "+const ANT_KEY: &str = \"sk-ant-api03-",
            "abcdefghijklmnopqrstuvwxyz012345\";\n",
            "+const GH_TOKEN: &str = \"ghp_",
            "1234567890abcdefghijklmnopqrstuvwxyz\";\n",
            "-const REMOVED_KEY: &str = \"sk-ant-api03-",
            "removedkeyremovedkeyremovedkey0000\";\n"
        );

        let findings = scan_git_staging_texts(&files, diff);
        assert!(findings.iter().any(|f| f.category == "Sensitive File" && f.detail.contains(".env")));
        assert!(findings.iter().any(|f| f.category == "Sensitive File" && f.detail.contains("id_ed25519")));
        assert!(findings.iter().any(|f| f.category == "Google API Key (AQ.)"));
        assert!(findings.iter().any(|f| f.category == "Anthropic API Key"));
        assert!(findings.iter().any(|f| f.category == "GitHub Classic Token"));
        assert_eq!(findings.len(), 5, "unexpected findings: {findings:?}");
    }

    #[test]
    fn extracts_installed_packages_and_parses_usr_bin_binaries() {
        assert_eq!(
            extract_installed_packages_from_command("sudo pacman -S --needed --noconfirm puzzles"),
            vec!["puzzles".to_string()]
        );
        assert_eq!(
            extract_installed_packages_from_command("yay -S extra/puzzles htop"),
            vec!["puzzles".to_string(), "htop".to_string()]
        );
        assert_eq!(
            extract_installed_packages_from_command("paru -S 'neovim>=0.10'"),
            vec!["neovim".to_string()]
        );
        assert!(extract_installed_packages_from_command("sudo pacman -Syu").is_empty());
        assert!(extract_installed_packages_from_command("pacman -Ss puzzles").is_empty());
        assert!(extract_installed_packages_from_command("yay -Si puzzles").is_empty());

        let ql = concat!(
            "puzzles /usr/\n",
            "puzzles /usr/bin/\n",
            "puzzles /usr/bin/puzzles-mines\n",
            "puzzles /usr/bin/puzzles-net\n",
            "puzzles /usr/bin/subdir/ignored\n",
            "puzzles /usr/share/doc/puzzles/README\n"
        );
        assert_eq!(
            parse_usr_bin_paths_from_pacman_ql(ql),
            vec![
                "/usr/bin/puzzles-mines".to_string(),
                "/usr/bin/puzzles-net".to_string()
            ]
        );
    }
}
