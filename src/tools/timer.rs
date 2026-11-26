//! Systemd user transient alarm and timer management (`systemd-run --user`).

use std::fs;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::executor::pty::shell_single_quote;

pub const ALARM_UNIT_PREFIX: &str = "waish-alarm-";

/// Arguments for `manage_alarm`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManageAlarmArgs {
    pub action: String,
    #[serde(default)]
    pub time: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
}

impl ManageAlarmArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON manage_alarm tidak valid: {e}"))?;
        parsed.action = parsed.action.trim().to_ascii_lowercase();
        parsed.time = parsed
            .time
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        parsed.label = parsed
            .label
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        if parsed.action.is_empty() {
            return Err(
                "field `action` tidak boleh kosong (gunakan: set, cancel, atau list)".to_string(),
            );
        }
        Ok(parsed)
    }
}

/// Captured stdout/stderr from a systemd command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdRunOutput {
    pub success: bool,
    pub stdout: String,
    pub stderr: String,
}

/// Parsed systemd schedule specification (`OnCalendar` vs `OnActive`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AlarmScheduleSpec {
    Calendar {
        formatted_hhmm: String,
        calendar_expr: String,
    },
    Relative {
        active_expr: String,
    },
}

impl AlarmScheduleSpec {
    pub fn systemd_flag(&self) -> String {
        match self {
            Self::Calendar { calendar_expr, .. } => format!("--on-calendar={calendar_expr}"),
            Self::Relative { active_expr } => format!("--on-active={active_expr}"),
        }
    }

    pub fn display_summary(&self) -> String {
        match self {
            Self::Calendar { calendar_expr, .. } => calendar_expr.clone(),
            Self::Relative { active_expr } => format!("in {active_expr}"),
        }
    }

    pub fn short_time_tag(&self) -> &str {
        match self {
            Self::Calendar { formatted_hhmm, .. } => formatted_hhmm.as_str(),
            Self::Relative { active_expr } => active_expr.as_str(),
        }
    }
}

pub fn parse_alarm_time_hhmm(raw: &str) -> Result<(u8, u8), String> {
    let trimmed = raw.trim();
    let Some((h_str, m_str)) = trimmed.split_once(':') else {
        return Err(format!(
            "Format waktu `{trimmed}` tidak valid. Gunakan format 24 jam `HH:MM` (contoh: `05:30`) atau durasi `25m`."
        ));
    };
    if h_str.is_empty() || m_str.len() != 2 {
        return Err(format!(
            "Format waktu `{trimmed}` tidak valid. Gunakan format 24 jam `HH:MM` (contoh: `05:30`) atau durasi `25m`."
        ));
    }
    let hour: u8 = h_str
        .parse()
        .map_err(|_| format!("Jam pada `{trimmed}` bukan angka valid (00-23)."))?;
    let minute: u8 = m_str
        .parse()
        .map_err(|_| format!("Menit pada `{trimmed}` bukan angka valid (00-59)."))?;
    if hour > 23 || minute > 59 {
        return Err(format!(
            "Waktu `{trimmed}` di luar rentang 24 jam (00:00 sampai 23:59)."
        ));
    }
    Ok((hour, minute))
}

pub fn parse_alarm_schedule(raw: &str) -> Result<AlarmScheduleSpec, String> {
    let trimmed = raw.trim();
    if trimmed.contains(':') {
        let (hour, minute) = parse_alarm_time_hhmm(trimmed)?;
        let formatted_hhmm = format!("{hour:02}:{minute:02}");
        let calendar_expr = format!("*-*-* {formatted_hhmm}:00");
        return Ok(AlarmScheduleSpec::Calendar {
            formatted_hhmm,
            calendar_expr,
        });
    }

    let without_plus = trimmed.strip_prefix('+').unwrap_or(trimmed).trim();
    let rel_re =
        Regex::new(r"(?i)^(?:(\d+)\s*(?:h|hr|jam))?\s*(?:(\d+)\s*(?:m|min|menit))?\s*(?:(\d+)\s*(?:s|sec|detik))?$")
            .expect("valid relative duration regex");
    if let Some(caps) = rel_re.captures(without_plus) {
        let h = caps.get(1).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
        let m = caps.get(2).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
        let s = caps.get(3).and_then(|m| m.as_str().parse::<u64>().ok()).unwrap_or(0);
        if h > 0 || m > 0 || s > 0 {
            let mut active = String::new();
            if h > 0 {
                active.push_str(&format!("{h}h"));
            }
            if m > 0 {
                active.push_str(&format!("{m}m"));
            }
            if s > 0 {
                active.push_str(&format!("{s}s"));
            }
            return Ok(AlarmScheduleSpec::Relative { active_expr: active });
        }
    }

    Err(format!(
        "Format waktu `{trimmed}` tidak valid. Gunakan format 24 jam `HH:MM` (contoh: `05:30`) atau durasi relatif seperti `25m` / `1h30m`."
    ))
}

pub fn sanitize_alarm_unit_name(label: Option<&str>) -> String {
    let raw = label.map(str::trim).unwrap_or("");
    let mut slug = String::new();
    let mut last_dash = false;

    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch.to_ascii_lowercase());
            last_dash = false;
        } else if !slug.is_empty() && !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 32 {
            break;
        }
    }

    let clean_slug = slug.trim_matches('-');
    if clean_slug.is_empty() {
        format!("{ALARM_UNIT_PREFIX}default")
    } else if clean_slug.starts_with(ALARM_UNIT_PREFIX) {
        clean_slug.to_string()
    } else {
        format!("{ALARM_UNIT_PREFIX}{clean_slug}")
    }
}

pub fn resolve_alarms_script_dir() -> Option<PathBuf> {
    dirs::data_local_dir().map(|base| base.join("waish").join("alarms"))
}

fn build_alarm_inline_command(time_tag: &str, display_label: &str) -> String {
    let title_q = shell_single_quote(&format!("waish alarm ({time_tag})"));
    let body_q = shell_single_quote(&format!(
        "( •̀ ω •́ )✧ Waktunya: {display_label} ({time_tag})!"
    ));
    format!(
        "if command -v notify-send >/dev/null 2>&1; then notify-send -u critical {title_q} {body_q}; fi; \
         if command -v paplay >/dev/null 2>&1 && [ -f /usr/share/sounds/freedesktop/stereo/complete.oga ]; then \
         paplay /usr/share/sounds/freedesktop/stereo/complete.oga >/dev/null 2>&1 || true; fi; \
         printf '\\a'"
    )
}

fn write_alarm_trigger_script_in_dir(
    alarms_dir: &Path,
    unit_name: &str,
    time_tag: &str,
    display_label: &str,
) -> std::io::Result<PathBuf> {
    fs::create_dir_all(alarms_dir)?;
    let script_path = alarms_dir.join(format!("{unit_name}.sh"));
    let body = format!(
        "#!/bin/sh\n{}\n",
        build_alarm_inline_command(time_tag, display_label)
    );
    let mut f = fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o700)
        .open(&script_path)?;
    f.write_all(body.as_bytes())?;
    f.sync_all()?;
    Ok(script_path)
}

fn cleanup_alarm_scripts_in_dir(alarms_dir: &Path, specific_unit: Option<&str>) {
    if !alarms_dir.exists() {
        return;
    }
    if let Some(unit) = specific_unit {
        let _ = fs::remove_file(alarms_dir.join(format!("{unit}.sh")));
        return;
    }
    if let Ok(entries) = fs::read_dir(alarms_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            if name
                .to_str()
                .is_some_and(|s| s.starts_with(ALARM_UNIT_PREFIX) && s.ends_with(".sh"))
            {
                let _ = fs::remove_file(entry.path());
            }
        }
    }
}

fn build_alarm_systemd_run_args_with_script_dir(
    time_raw: &str,
    label: Option<&str>,
    alarms_dir: Option<&Path>,
) -> Result<(String, String, Vec<String>), String> {
    let schedule = parse_alarm_schedule(time_raw)?;
    let unit_name = sanitize_alarm_unit_name(label);
    let display_label = label
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("Alarm");
    let time_tag = schedule.short_time_tag();

    let exec_tail: Vec<String> = if let Some(script_path) = alarms_dir.and_then(|dir| {
        write_alarm_trigger_script_in_dir(dir, &unit_name, time_tag, display_label).ok()
    }) {
        vec![script_path.to_string_lossy().into_owned()]
    } else {
        vec![
            "/bin/sh".to_string(),
            "-c".to_string(),
            build_alarm_inline_command(time_tag, display_label),
        ]
    };

    let mut args = vec![
        "--user".to_string(),
        schedule.systemd_flag(),
        format!("--unit={unit_name}"),
        format!("--description=waish alarm: {display_label} ({time_tag})"),
    ];
    args.extend(exec_tail);

    Ok((unit_name, schedule.display_summary(), args))
}

fn run_system_cmd(program: &str, args: &[String]) -> Result<CmdRunOutput, String> {
    let out = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("Gagal menjalankan `{program}`: {e}"))?;

    Ok(CmdRunOutput {
        success: out.status.success(),
        stdout: String::from_utf8_lossy(&out.stdout).trim().to_string(),
        stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
    })
}

pub fn execute_manage_alarm(args: &ManageAlarmArgs) -> crate::Result<String> {
    let alarms_dir = resolve_alarms_script_dir();
    execute_manage_alarm_internal(args, alarms_dir.as_deref(), run_system_cmd)
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })
}

#[cfg(test)]
pub fn execute_manage_alarm_with_runner<F>(
    args: &ManageAlarmArgs,
    run_cmd: F,
) -> Result<String, String>
where
    F: FnMut(&str, &[String]) -> Result<CmdRunOutput, String>,
{
    execute_manage_alarm_internal(args, None, run_cmd)
}

fn execute_manage_alarm_internal<F>(
    args: &ManageAlarmArgs,
    alarms_dir: Option<&Path>,
    mut run_cmd: F,
) -> Result<String, String>
where
    F: FnMut(&str, &[String]) -> Result<CmdRunOutput, String>,
{
    match args.action.as_str() {
        "set" => {
            let Some(time_raw) = args.time.as_deref() else {
                return Err(
                    "Field `time` (`HH:MM` seperti `05:30` atau durasi `25m`) wajib diisi untuk aksi `set`."
                        .to_string(),
                );
            };
            let (unit_name, schedule_summary, run_args) =
                build_alarm_systemd_run_args_with_script_dir(
                    time_raw,
                    args.label.as_deref(),
                    alarms_dir,
                )?;

            let pre_stop = vec![
                "--user".to_string(),
                "stop".to_string(),
                format!("{unit_name}.timer"),
                format!("{unit_name}.service"),
            ];
            let _ = run_cmd("systemctl", &pre_stop);
            let _ = run_cmd(
                "systemctl",
                &["--user".to_string(), "reset-failed".to_string()],
            );

            let res = run_cmd("systemd-run", &run_args)?;
            if !res.success {
                let detail = if res.stderr.is_empty() {
                    res.stdout
                } else {
                    res.stderr
                };
                return Err(format!(
                    "Gagal menyetel transient alarm `{unit_name}` via systemd-run: {detail}"
                ));
            }

            let label_display = args.label.as_deref().unwrap_or("default");
            Ok(format!(
                "status: alarm_set\n\
                 unit: {unit_name}.timer\n\
                 calendar: {schedule_summary}\n\
                 label: {label_display}\n\
                 note: Alarm transient systemd user berhasil dijadwalkan."
            ))
        }
        "cancel" => {
            let (timer_pattern, service_pattern, scope_label, specific_unit) =
                match args.label.as_deref() {
                    Some(lbl)
                        if !lbl.eq_ignore_ascii_case("all")
                            && !lbl.eq_ignore_ascii_case("semua") =>
                    {
                        let unit = sanitize_alarm_unit_name(Some(lbl));
                        (
                            format!("{unit}.timer"),
                            format!("{unit}.service"),
                            format!("{unit}.timer"),
                            Some(unit),
                        )
                    }
                    _ => (
                        format!("{ALARM_UNIT_PREFIX}*.timer"),
                        format!("{ALARM_UNIT_PREFIX}*.service"),
                        format!("{ALARM_UNIT_PREFIX}*"),
                        None,
                    ),
                };

            let stop_args = vec![
                "--user".to_string(),
                "stop".to_string(),
                timer_pattern,
                service_pattern,
            ];
            let stop_res = run_cmd("systemctl", &stop_args)?;

            let reset_args = vec!["--user".to_string(), "reset-failed".to_string()];
            let _ = run_cmd("systemctl", &reset_args);

            if let Some(dir) = alarms_dir {
                cleanup_alarm_scripts_in_dir(dir, specific_unit.as_deref());
            }

            let detail = if stop_res.success {
                "Unit alarm transient berhasil dihentikan dan dibersihkan."
            } else {
                "Tidak ada unit alarm transient aktif yang tersisa (sudah bersih)."
            };

            Ok(format!(
                "status: alarm_cancelled\n\
                 target: {scope_label}\n\
                 message: {detail}"
            ))
        }
        "list" => {
            let list_args = vec![
                "--user".to_string(),
                "list-timers".to_string(),
                "--all".to_string(),
                "--no-pager".to_string(),
                format!("{ALARM_UNIT_PREFIX}*"),
            ];
            let res = run_cmd("systemctl", &list_args)?;
            if !res.success {
                let detail = if res.stderr.is_empty() {
                    res.stdout
                } else {
                    res.stderr
                };
                return Err(format!("Gagal membaca daftar alarm systemd: {detail}"));
            }
            if res.stdout.is_empty() || res.stdout.contains("0 timers listed") {
                Ok("status: empty\nmessage: Tidak ada alarm waish yang sedang aktif.".to_string())
            } else {
                Ok(format!("status: listed\n{}", res.stdout))
            }
        }
        other => Err(format!(
            "Aksi `{other}` tidak dikenal pada manage_alarm. Gunakan: set, cancel, atau list."
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_times_relative_durations_and_sanitizes_unit_names() {
        assert_eq!(parse_alarm_time_hhmm("05:30").unwrap(), (5, 30));
        assert_eq!(parse_alarm_time_hhmm("5:05").unwrap(), (5, 5));
        assert_eq!(parse_alarm_time_hhmm("23:59").unwrap(), (23, 59));
        assert!(parse_alarm_time_hhmm("24:00").is_err());
        assert!(parse_alarm_time_hhmm("12:60").is_err());
        assert!(parse_alarm_time_hhmm("pagi").is_err());

        assert_eq!(
            parse_alarm_schedule("25m").unwrap(),
            AlarmScheduleSpec::Relative {
                active_expr: "25m".to_string()
            }
        );
        assert_eq!(
            parse_alarm_schedule("1h 30m").unwrap(),
            AlarmScheduleSpec::Relative {
                active_expr: "1h30m".to_string()
            }
        );

        assert_eq!(
            sanitize_alarm_unit_name(Some("Bangun Pagi!")),
            "waish-alarm-bangun-pagi"
        );
        assert_eq!(sanitize_alarm_unit_name(None), "waish-alarm-default");
        assert_eq!(
            sanitize_alarm_unit_name(Some("waish-alarm-kuliah")),
            "waish-alarm-kuliah"
        );
    }

    #[test]
    fn manage_alarm_set_list_and_cancel_use_systemd_transient_units_without_filesystem_search() {
        let script_dir =
            std::env::temp_dir().join(format!("waish-alarm-scripts-{}", std::process::id()));
        let _ = fs::remove_dir_all(&script_dir);

        let mut recorded: Vec<(String, Vec<String>)> = Vec::new();
        let set_args = ManageAlarmArgs {
            action: "set".to_string(),
            time: Some("05:30".to_string()),
            label: Some("Subuh".to_string()),
        };

        let set_out =
            execute_manage_alarm_internal(&set_args, Some(&script_dir), |prog, args| {
                recorded.push((prog.to_string(), args.to_vec()));
                Ok(CmdRunOutput {
                    success: true,
                    stdout: "Running timer as unit: waish-alarm-subuh.timer".to_string(),
                    stderr: String::new(),
                })
            })
            .expect("set alarm must succeed");

        assert!(set_out.contains("status: alarm_set"));
        assert!(set_out.contains("waish-alarm-subuh.timer"));
        assert!(set_out.contains("*-*-* 05:30:00"));
        assert!(script_dir.join("waish-alarm-subuh.sh").exists());
        assert!(
            recorded.iter().any(|(p, a)| p == "systemd-run"
                && a.iter().any(|x| x == "--user")
                && a.iter().any(|x| x == "--on-calendar=*-*-* 05:30:00")
                && a.iter().any(|x| x == "--unit=waish-alarm-subuh")),
            "expected systemd-run invocation, got: {recorded:?}"
        );

        let list_args = ManageAlarmArgs {
            action: "list".to_string(),
            time: None,
            label: None,
        };
        let list_out = execute_manage_alarm_with_runner(&list_args, |prog, args| {
            assert_eq!(prog, "systemctl");
            assert!(args.iter().any(|a| a == "list-timers"));
            Ok(CmdRunOutput {
                success: true,
                stdout: "NEXT LEFT LAST PASSED UNIT ACTIVATES\nwaish-alarm-subuh.timer".to_string(),
                stderr: String::new(),
            })
        })
        .unwrap();
        assert!(list_out.contains("status: listed"));
        assert!(list_out.contains("waish-alarm-subuh.timer"));

        recorded.clear();
        let cancel_args = ManageAlarmArgs {
            action: "cancel".to_string(),
            time: None,
            label: None,
        };
        let cancel_out =
            execute_manage_alarm_internal(&cancel_args, Some(&script_dir), |prog, args| {
                recorded.push((prog.to_string(), args.to_vec()));
                Ok(CmdRunOutput {
                    success: true,
                    stdout: String::new(),
                    stderr: String::new(),
                })
            })
            .expect("cancel alarm must succeed");

        assert!(cancel_out.contains("status: alarm_cancelled"));
        assert!(!script_dir.join("waish-alarm-subuh.sh").exists());
        assert_eq!(recorded.len(), 2);
        assert_eq!(recorded[0].0, "systemctl");
        assert_eq!(
            recorded[0].1,
            vec!["--user", "stop", "waish-alarm-*.timer", "waish-alarm-*.service"]
        );
        assert_eq!(recorded[1].0, "systemctl");
        assert_eq!(recorded[1].1, vec!["--user", "reset-failed"]);
        assert!(!recorded.iter().any(|(p, _)| p == "find"));

        let _ = fs::remove_dir_all(&script_dir);
    }
}
