//! Universal document engine for `.xlsx`/`.ods`/`.xls`/`.csv` (`rust_xlsxwriter` + `calamine`), `.docx` (`docx-rs`), `.pdf` (`pdf-extract`), and editor hot-reload (`hot_edit`).

use std::fs;
use std::io::{Cursor, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use calamine::{open_workbook_auto, Data, Reader};
use docx_rs::{
    DocumentChild, Docx, Paragraph, Run, Table, TableCell, TableCellContent, TableChild, TableRow,
    TableRowChild,
};
use regex::Regex;
use rust_xlsxwriter::{Format, Formula, Workbook};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::config::settings::{DesktopConfig, Settings};
use crate::engine::shadow_git::ShadowGit;
use crate::executor::pty::shell_single_quote;
use crate::wizard::detector::{self, DesktopBackend};

use super::desktop::{
    self, create_window_backend, HyprlandBackend, WindowBackend, WindowInfo,
};
use super::runner::{resolve_real_home_dir, sanitize_path_for_user};
use super::workspace_tracker::{self, SessionWorkspaceState};

pub const PDF_MAX_EXTRACT_BYTES: usize = 16 * 1024;
pub const PDF_MAX_EXTRACT_LINES: usize = 200;
const HOT_EDIT_CLOSE_WAIT: Duration = Duration::from_millis(1500);
const HOT_EDIT_POLL_STEP: Duration = Duration::from_millis(150);

fn default_write_action() -> String {
    "create".to_string()
}

pub fn atomic_write_bytes(path: &Path, bytes: &[u8], mode: u32) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    if !parent.as_os_str().is_empty() {
        fs::create_dir_all(parent)?;
    }
    let fname = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("waish_doc");
    let tmp = parent.join(format!(".{fname}.waish_tmp_{}", std::process::id()));
    {
        let mut f = fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .mode(mode)
            .open(&tmp)?;
        f.write_all(bytes)?;
        f.sync_all()?;
    }
    fs::rename(&tmp, path)
}

pub fn verify_written_file(path: &Path) -> crate::Result<u64> {
    let meta = fs::metadata(path)
        .map_err(|e| format!("Verifikasi fisik gagal untuk {}: {e}", path.display()))?;
    if !meta.is_file() || meta.len() == 0 {
        return Err(format!("File hasil {} kosong atau tidak valid.", path.display()).into());
    }
    Ok(meta.len())
}

pub fn resolve_document_target_path(raw_path: &str, companion_user_name: &str) -> PathBuf {
    let trimmed = raw_path.trim();
    if !trimmed.starts_with('/')
        && !trimmed.starts_with("~/")
        && !trimmed.starts_with("./")
        && !trimmed.starts_with("../")
        && !trimmed.contains('/')
    {
        let home = resolve_real_home_dir();
        return PathBuf::from(home).join(trimmed);
    }
    sanitize_path_for_user(trimmed, companion_user_name)
}

/// Arguments for `manage_spreadsheet`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManageSpreadsheetArgs {
    #[serde(default = "default_write_action")]
    pub action: String,
    pub path: String,
    #[serde(default)]
    pub sheet_name: Option<String>,
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default, alias = "table_data")]
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub sequence: Option<String>,
}

/// Arguments for `manage_document`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManageDocumentArgs {
    #[serde(default = "default_write_action")]
    pub action: String,
    pub path: String,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub paragraphs: Vec<String>,
    #[serde(default)]
    pub sheet_name: Option<String>,
    #[serde(default)]
    pub headers: Vec<String>,
    #[serde(default, alias = "table_data")]
    pub rows: Vec<Vec<Value>>,
    #[serde(default)]
    pub sequence: Option<String>,
    #[serde(default)]
    pub editor: Option<String>,
    #[serde(default)]
    pub workspace: Option<i32>,
}

impl ManageSpreadsheetArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON manage_spreadsheet tidak valid: {e}"))?;
        parsed.action = parsed.action.trim().to_ascii_lowercase();
        parsed.path = parsed.path.trim().to_string();
        if parsed.path.is_empty() {
            return Err("field `path` wajib diisi pada manage_spreadsheet.".to_string());
        }
        Ok(parsed)
    }

    pub fn into_document_args(self) -> ManageDocumentArgs {
        ManageDocumentArgs {
            action: self.action,
            path: self.path,
            title: None,
            content: String::new(),
            paragraphs: Vec::new(),
            sheet_name: self.sheet_name,
            headers: self.headers,
            rows: self.rows,
            sequence: self.sequence,
            editor: None,
            workspace: None,
        }
    }
}

impl ManageDocumentArgs {
    pub fn from_json_str(raw: &str) -> Result<Self, String> {
        let mut parsed: Self = serde_json::from_str(raw)
            .map_err(|e| format!("argumen JSON manage_document tidak valid: {e}"))?;
        parsed.action = parsed.action.trim().to_ascii_lowercase();
        parsed.path = parsed.path.trim().to_string();
        if parsed.path.is_empty() {
            return Err("field `path` wajib diisi pada manage_document.".to_string());
        }
        Ok(parsed)
    }
}

/// Summary of a document written to disk.
#[derive(Debug, Clone)]
pub struct WrittenDocumentSummary {
    pub path: PathBuf,
    pub format: String,
    pub rows_or_items: usize,
    pub bytes: u64,
}

pub fn expand_sequence_spec(spec: &str) -> Option<Vec<i64>> {
    let clean = spec
        .trim()
        .strip_prefix("sequence:")
        .or_else(|| spec.trim().strip_prefix("seq:"))
        .unwrap_or(spec.trim())
        .trim();

    let (start_s, end_s, inclusive) = if let Some((a, b)) = clean.split_once("..=") {
        (a.trim(), b.trim(), true)
    } else if let Some((a, b)) = clean.split_once("..") {
        (a.trim(), b.trim(), true)
    } else {
        return None;
    };

    let start: i64 = start_s.parse().ok()?;
    let end: i64 = end_s.parse().ok()?;
    let actual_end = if inclusive { end } else { end.saturating_sub(1) };

    if actual_end < start {
        return None;
    }
    let count = (actual_end - start + 1).min(10_000) as usize;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        out.push(start + i as i64);
    }
    Some(out)
}

pub fn json_cell_to_string(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

pub fn build_effective_table_rows(
    headers: &[String],
    rows: &[Vec<Value>],
    sequence: Option<&str>,
) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = rows
        .iter()
        .map(|r| r.iter().map(json_cell_to_string).collect())
        .collect();

    if let Some(nums) = sequence
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(expand_sequence_spec)
    {
        let col_count = headers.len().max(1);
        for n in nums {
            let mut row = Vec::with_capacity(col_count);
            row.push(n.to_string());
            for _ in 1..col_count {
                row.push(String::new());
            }
            out.push(row);
        }
    }

    if out.len() == 1 && out[0].len() == 1 && out[0][0].trim().starts_with("sequence:") {
        if let Some(nums) = expand_sequence_spec(&out[0][0]) {
            let col_count = headers.len().max(1);
            out.clear();
            for n in nums {
                let mut row = Vec::with_capacity(col_count);
                row.push(n.to_string());
                for _ in 1..col_count {
                    row.push(String::new());
                }
                out.push(row);
            }
        }
    }

    out
}

pub fn build_xlsx_bytes(
    sheet_name: &str,
    rows: &[Vec<String>],
    bold_first_row: bool,
) -> crate::Result<Vec<u8>> {
    let mut workbook = Workbook::new();
    let worksheet = workbook.add_worksheet();

    let safe_sheet: String = sheet_name
        .chars()
        .filter(|c| !matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\'))
        .take(31)
        .collect();
    let safe_sheet = safe_sheet.trim();
    if !safe_sheet.is_empty() {
        let _ = worksheet.set_name(safe_sheet);
    }

    let bold_fmt = Format::new().set_bold();

    for (r_idx, row) in rows.iter().enumerate() {
        let Ok(r_u32) = u32::try_from(r_idx) else {
            break;
        };
        let is_header = r_idx == 0 && bold_first_row;
        for (c_idx, val) in row.iter().enumerate() {
            let Ok(c_u16) = u16::try_from(c_idx) else {
                break;
            };
            let trimmed = val.trim();
            if trimmed.starts_with('=') && trimmed.len() > 1 {
                if is_header {
                    worksheet.write_formula_with_format(
                        r_u32,
                        c_u16,
                        Formula::new(trimmed),
                        &bold_fmt,
                    )?;
                } else {
                    worksheet.write_formula(r_u32, c_u16, Formula::new(trimmed))?;
                }
            } else if !trimmed.is_empty()
                && !(trimmed.len() > 1 && trimmed.starts_with('0') && !trimmed.starts_with("0."))
                && trimmed.parse::<f64>().ok().is_some_and(f64::is_finite)
            {
                let num = trimmed.parse::<f64>().unwrap_or(0.0);
                if is_header {
                    worksheet.write_number_with_format(r_u32, c_u16, num, &bold_fmt)?;
                } else {
                    worksheet.write_number(r_u32, c_u16, num)?;
                }
            } else if is_header {
                worksheet.write_string_with_format(r_u32, c_u16, val, &bold_fmt)?;
            } else {
                worksheet.write_string(r_u32, c_u16, val)?;
            }
        }
    }

    worksheet.autofit();
    let buf = workbook.save_to_buffer()?;
    Ok(buf)
}

fn calamine_cell_to_string(cell: &Data) -> String {
    match cell {
        Data::Empty => String::new(),
        Data::String(s) => s.clone(),
        Data::Int(i) => i.to_string(),
        Data::Float(f) => {
            if f.fract() == 0.0 && f.abs() < (i64::MAX as f64) {
                format!("{}", *f as i64)
            } else {
                f.to_string()
            }
        }
        Data::Bool(b) => b.to_string(),
        Data::DateTime(dt) => dt.to_string(),
        Data::DateTimeIso(s) | Data::DurationIso(s) => s.clone(),
        Data::Error(e) => format!("#{e:?}"),
    }
}

pub fn read_spreadsheet_grid_calamine(
    path: &Path,
    preferred_sheet: Option<&str>,
) -> crate::Result<Vec<Vec<String>>> {
    let mut workbook = open_workbook_auto(path)
        .map_err(|e| format!("Gagal membuka workbook {}: {e}", path.display()))?;
    let sheet_names = workbook.sheet_names();
    if sheet_names.is_empty() {
        return Ok(Vec::new());
    }

    let chosen_sheet = preferred_sheet
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .and_then(|req| {
            sheet_names
                .iter()
                .find(|name| name.eq_ignore_ascii_case(req))
                .cloned()
        })
        .unwrap_or_else(|| sheet_names[0].clone());

    let range = workbook
        .worksheet_range(&chosen_sheet)
        .map_err(|e| format!("Gagal membaca sheet `{chosen_sheet}` di {}: {e}", path.display()))?;
    let formulas_opt = workbook.worksheet_formula(&chosen_sheet).ok();

    let data_end = range.end();
    let f_end = formulas_opt.as_ref().and_then(|f| f.end());
    let max_pos = match (data_end, f_end) {
        (Some((r1, c1)), Some((r2, c2))) => Some((r1.max(r2), c1.max(c2))),
        (Some(p), None) | (None, Some(p)) => Some(p),
        (None, None) => None,
    };
    let Some((end_r, end_c)) = max_pos else {
        return Ok(Vec::new());
    };

    let start_r = range
        .start()
        .map(|s| s.0)
        .into_iter()
        .chain(formulas_opt.as_ref().and_then(|f| f.start()).map(|s| s.0))
        .min()
        .unwrap_or(0);
    let start_c = range
        .start()
        .map(|s| s.1)
        .into_iter()
        .chain(formulas_opt.as_ref().and_then(|f| f.start()).map(|s| s.1))
        .min()
        .unwrap_or(0);

    let total_h = (end_r - start_r + 1) as usize;
    let total_w = (end_c - start_c + 1) as usize;
    let mut rows = Vec::with_capacity(total_h);

    for abs_r in start_r..=end_r {
        let mut row_vals = Vec::with_capacity(total_w);
        for abs_c in start_c..=end_c {
            if let Some(f_str) = formulas_opt
                .as_ref()
                .and_then(|f_range| f_range.get_value((abs_r, abs_c)))
            {
                let clean_f = f_str.trim();
                if !clean_f.is_empty() {
                    if clean_f.starts_with('=') {
                        row_vals.push(clean_f.to_string());
                    } else {
                        row_vals.push(format!("={clean_f}"));
                    }
                    continue;
                }
            }
            let val_str = range
                .get_value((abs_r, abs_c))
                .map(calamine_cell_to_string)
                .unwrap_or_default();
            row_vals.push(val_str);
        }
        rows.push(row_vals);
    }

    Ok(rows)
}

pub fn encode_csv_bytes(rows: &[Vec<String>]) -> Vec<u8> {
    let mut out = String::new();
    for row in rows {
        let encoded: Vec<String> = row
            .iter()
            .map(|cell| {
                if cell.contains([',', '"', '\n', '\r']) {
                    format!("\"{}\"", cell.replace('"', "\"\""))
                } else {
                    cell.clone()
                }
            })
            .collect();
        out.push_str(&encoded.join(","));
        out.push('\n');
    }
    out.into_bytes()
}

pub fn read_csv_grid(path: &Path) -> crate::Result<Vec<Vec<String>>> {
    let raw = fs::read_to_string(path)?;
    Ok(raw.lines().map(parse_csv_line).collect())
}

fn parse_csv_line(line: &str) -> Vec<String> {
    let mut cells = Vec::new();
    let mut cur = String::new();
    let mut chars = line.chars().peekable();
    let mut in_quotes = false;

    while let Some(ch) = chars.next() {
        if in_quotes {
            if ch == '"' {
                if chars.peek() == Some(&'"') {
                    cur.push('"');
                    let _ = chars.next();
                } else {
                    in_quotes = false;
                }
            } else {
                cur.push(ch);
            }
        } else if ch == '"' {
            in_quotes = true;
        } else if ch == ',' {
            cells.push(cur);
            cur = String::new();
        } else {
            cur.push(ch);
        }
    }
    cells.push(cur);
    cells
}

fn format_grid_preview(path: &Path, grid: &[Vec<String>]) -> String {
    let shown = grid.len().min(60);
    let mut out = format!(
        "status: success\npath: {}\ntotal_rows: {} (menampilkan {shown} baris)\n---\n",
        path.display(),
        grid.len()
    );
    for (idx, row) in grid.iter().take(shown).enumerate() {
        out.push_str(&format!("{:>3} | {}\n", idx + 1, row.join(" | ")));
    }
    if grid.len() > shown {
        out.push_str(&format!(
            "[Truncated: {} baris lainnya disembunyikan]\n",
            grid.len() - shown
        ));
    }
    out
}

pub fn build_docx_bytes(
    title: Option<&str>,
    lines: &[String],
    headers: &[String],
    table_rows: &[Vec<String>],
) -> crate::Result<Vec<u8>> {
    let mut docx = Docx::new();

    if let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) {
        docx = docx.add_paragraph(
            Paragraph::new().add_run(Run::new().add_text(t).bold().size(36)),
        );
    }

    for line in lines {
        let trimmed = line.trim_end();
        if let Some(h3) = trimmed.strip_prefix("### ") {
            docx = docx.add_paragraph(
                Paragraph::new().add_run(Run::new().add_text(h3.trim()).bold().size(24)),
            );
        } else if let Some(h2) = trimmed.strip_prefix("## ") {
            docx = docx.add_paragraph(
                Paragraph::new().add_run(Run::new().add_text(h2.trim()).bold().size(28)),
            );
        } else if let Some(h1) = trimmed.strip_prefix("# ") {
            docx = docx.add_paragraph(
                Paragraph::new().add_run(Run::new().add_text(h1.trim()).bold().size(32)),
            );
        } else if let Some(bullet) = trimmed
            .strip_prefix("- ")
            .or_else(|| trimmed.strip_prefix("* "))
        {
            docx = docx.add_paragraph(
                Paragraph::new().add_run(Run::new().add_text(format!("• {}", bullet.trim()))),
            );
        } else {
            docx = docx.add_paragraph(Paragraph::new().add_run(Run::new().add_text(trimmed)));
        }
    }

    if !headers.is_empty() || !table_rows.is_empty() {
        let mut docx_rows = Vec::new();
        if !headers.is_empty() {
            let cells: Vec<TableCell> = headers
                .iter()
                .map(|h| {
                    TableCell::new()
                        .add_paragraph(Paragraph::new().add_run(Run::new().add_text(h).bold()))
                })
                .collect();
            docx_rows.push(TableRow::new(cells));
        }
        for row in table_rows {
            let cells: Vec<TableCell> = row
                .iter()
                .map(|val| {
                    TableCell::new()
                        .add_paragraph(Paragraph::new().add_run(Run::new().add_text(val)))
                })
                .collect();
            docx_rows.push(TableRow::new(cells));
        }
        docx = docx.add_table(Table::new(docx_rows));
    }

    let mut buf = Vec::with_capacity(4096);
    docx.build()
        .pack(Cursor::new(&mut buf))
        .map_err(|e| format!("Gagal memaketkan arsip .docx: {e}"))?;
    Ok(buf)
}

pub fn read_docx_text(path: &Path) -> crate::Result<String> {
    let bytes = fs::read(path)?;

    if let Ok(parsed) = docx_rs::read_docx(&bytes) {
        let mut lines = Vec::new();
        for child in &parsed.document.children {
            match child {
                DocumentChild::Paragraph(p) => {
                    lines.push(p.raw_text());
                }
                DocumentChild::Table(tbl) => {
                    for row_child in &tbl.rows {
                        let TableChild::TableRow(tr) = row_child;
                        let mut cell_texts = Vec::new();
                        for cell_child in &tr.cells {
                            let TableRowChild::TableCell(tc) = cell_child;
                            let mut parts = Vec::new();
                            for content in &tc.children {
                                if let TableCellContent::Paragraph(p) = content {
                                    let t = p.raw_text();
                                    if !t.is_empty() {
                                        parts.push(t);
                                    }
                                }
                            }
                            cell_texts.push(parts.join(" "));
                        }
                        lines.push(cell_texts.join(" | "));
                    }
                }
                _ => {}
            }
        }
        if !lines.is_empty() {
            return Ok(lines.join("\n"));
        }
    }

    read_docx_xml_fallback(&bytes, path)
}

fn read_docx_xml_fallback(bytes: &[u8], path: &Path) -> crate::Result<String> {
    let mut xml = extract_stored_zip_entry(bytes, "word/document.xml")
        .map(|raw| String::from_utf8_lossy(&raw).into_owned())
        .unwrap_or_default();
    if xml.is_empty() {
        if let Ok(out) = Command::new("unzip")
            .args(["-p", &path.to_string_lossy(), "word/document.xml"])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
        {
            if out.status.success() && !out.stdout.is_empty() {
                xml = String::from_utf8_lossy(&out.stdout).into_owned();
            }
        }
    }
    if xml.is_empty() {
        return Err(format!("Gagal membaca `word/document.xml` dari {}", path.display()).into());
    }

    let p_re = Regex::new(r"(?s)<w:p\b[^>]*>(.*?)</w:p>").expect("valid w:p regex");
    let t_re = Regex::new(r"(?s)<w:t\b[^>]*>(.*?)</w:t>").expect("valid w:t regex");
    let mut paragraphs = Vec::new();
    for p_cap in p_re.captures_iter(&xml) {
        let mut line = String::new();
        for t_cap in t_re.captures_iter(&p_cap[1]) {
            line.push_str(&xml_unescape(&t_cap[1]));
        }
        paragraphs.push(line);
    }
    Ok(paragraphs.join("\n"))
}

// Local ZIP file header layout (signature 0x04034b50, method at +8, comp_size at +18, name_len at +26, extra_len at +28).
fn extract_stored_zip_entry(zip_bytes: &[u8], target_entry: &str) -> Option<Vec<u8>> {
    let mut pos = 0usize;
    while pos + 30 <= zip_bytes.len() {
        let sig = u32::from_le_bytes(zip_bytes[pos..pos + 4].try_into().ok()?);
        if sig != 0x0403_4b50 {
            break;
        }
        let method = u16::from_le_bytes(zip_bytes[pos + 8..pos + 10].try_into().ok()?);
        let comp_size =
            u32::from_le_bytes(zip_bytes[pos + 18..pos + 22].try_into().ok()?) as usize;
        let name_len =
            u16::from_le_bytes(zip_bytes[pos + 26..pos + 28].try_into().ok()?) as usize;
        let extra_len =
            u16::from_le_bytes(zip_bytes[pos + 28..pos + 30].try_into().ok()?) as usize;

        let name_start = pos + 30;
        let data_start = name_start.checked_add(name_len)?.checked_add(extra_len)?;
        let data_end = data_start.checked_add(comp_size)?;
        if data_end > zip_bytes.len() {
            return None;
        }

        let entry_name = std::str::from_utf8(&zip_bytes[name_start..name_start + name_len]).ok()?;
        if entry_name == target_entry {
            return if method == 0 {
                Some(zip_bytes[data_start..data_end].to_vec())
            } else {
                None
            };
        }
        pos = data_end;
    }
    None
}

fn xml_unescape(raw: &str) -> String {
    raw.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

pub fn truncate_extracted_pdf_text(raw: &str, max_bytes: usize, max_lines: usize) -> (String, bool) {
    let mut out = String::new();
    let mut truncated = false;

    for (line_idx, line) in raw.lines().enumerate() {
        if line_idx >= max_lines {
            truncated = true;
            break;
        }
        let sep_len = usize::from(!out.is_empty());
        if out.len() + sep_len + line.len() > max_bytes {
            let remaining = max_bytes.saturating_sub(out.len() + sep_len);
            if remaining > 0 {
                if !out.is_empty() {
                    out.push('\n');
                }
                let mut valid_end = 0usize;
                for (b_idx, ch) in line.char_indices() {
                    if b_idx + ch.len_utf8() <= remaining {
                        valid_end = b_idx + ch.len_utf8();
                    } else {
                        break;
                    }
                }
                out.push_str(&line[..valid_end]);
            }
            truncated = true;
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }

    if truncated {
        out.push_str("\n[Truncated]");
    }
    (out, truncated)
}

pub fn extract_pdf_text_bounded(path: &Path) -> crate::Result<String> {
    let bytes = fs::read(path)?;
    if bytes.is_empty() {
        return Err(format!("File PDF kosong: {}", path.display()).into());
    }

    let extracted_res = std::panic::catch_unwind(|| pdf_extract::extract_text_from_mem(&bytes));
    let raw_text = match extracted_res {
        Ok(Ok(txt)) if !txt.trim().is_empty() => txt,
        _ => extract_pdf_literal_strings_fallback(&bytes).ok_or_else(|| {
            format!(
                "Gagal mengekstrak teks terbaca dari PDF {} (kemungkinan hasil scan gambar murni atau terenkripsi).",
                path.display()
            )
        })?,
    };

    let (bounded, truncated) =
        truncate_extracted_pdf_text(&raw_text, PDF_MAX_EXTRACT_BYTES, PDF_MAX_EXTRACT_LINES);
    let line_count = bounded.lines().count();

    Ok(format!(
        "status: success\n\
         format: pdf\n\
         path: {}\n\
         lines_shown: {line_count}\n\
         truncated: {truncated}\n\
         ---\n\
         {bounded}",
        path.display()
    ))
}

fn extract_pdf_literal_strings_fallback(bytes: &[u8]) -> Option<String> {
    let lossy = String::from_utf8_lossy(bytes);
    let tj_re = Regex::new(r"\(([^()\\]*(?:\\.[^()\\]*)*)\)\s*Tj").ok()?;
    let mut chunks = Vec::new();
    for cap in tj_re.captures_iter(&lossy) {
        let unescaped = cap[1]
            .replace("\\(", "(")
            .replace("\\)", ")")
            .replace("\\n", "\n")
            .replace("\\\\", "\\");
        if !unescaped.trim().is_empty() {
            chunks.push(unescaped);
        }
    }
    if chunks.is_empty() {
        None
    } else {
        Some(chunks.join("\n"))
    }
}

fn collect_document_lines(args: &ManageDocumentArgs) -> Vec<String> {
    let mut out = Vec::new();
    if !args.content.trim().is_empty() {
        out.extend(args.content.lines().map(str::to_string));
    }
    out.extend(args.paragraphs.iter().cloned());
    out
}

fn render_plain_document(title: Option<&str>, lines: &[String]) -> String {
    let mut out = String::new();
    if let Some(t) = title.map(str::trim).filter(|s| !s.is_empty()) {
        out.push_str("# ");
        out.push_str(t);
        out.push_str("\n\n");
    }
    out.push_str(&lines.join("\n"));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

pub fn write_or_update_document_on_disk(
    args: &ManageDocumentArgs,
    settings: &Settings,
    shadow_git: Option<&ShadowGit>,
    append_mode: bool,
) -> crate::Result<WrittenDocumentSummary> {
    let target = resolve_document_target_path(&args.path, &settings.general.user_real_name);
    let ext = target
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("docx")
        .to_ascii_lowercase();

    if ext == "pdf" {
        return Err(
            "Format `.pdf` bersifat baca-saja (ekstraksi teks). Untuk membuat dokumen baru, gunakan `.docx` atau `.xlsx`."
                .into(),
        );
    }

    if let Some(parent) = target.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)?;
    }

    let is_spreadsheet = matches!(ext.as_str(), "xlsx" | "csv");
    if is_spreadsheet {
        let is_csv = ext == "csv";
        if is_csv && target.exists() {
            if let Some(sg) = shadow_git {
                let _ = sg.snapshot_before_edit(&target);
            }
        }

        let sheet_title = args
            .sheet_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("Sheet1");

        let generated_rows =
            build_effective_table_rows(&args.headers, &args.rows, args.sequence.as_deref());

        let (all_rows, has_header) = if append_mode && target.exists() {
            let mut existing = if is_csv {
                read_csv_grid(&target).unwrap_or_default()
            } else {
                read_spreadsheet_grid_calamine(&target, Some(sheet_title)).unwrap_or_default()
            };
            let had_existing = !existing.is_empty();
            if !had_existing && !args.headers.is_empty() {
                existing.push(args.headers.clone());
            }
            existing.extend(generated_rows);
            (existing, had_existing || !args.headers.is_empty())
        } else {
            let mut fresh = Vec::new();
            let has_hdr = !args.headers.is_empty();
            if has_hdr {
                fresh.push(args.headers.clone());
            }
            fresh.extend(generated_rows);
            (fresh, has_hdr)
        };

        let bytes = if is_csv {
            encode_csv_bytes(&all_rows)
        } else {
            build_xlsx_bytes(sheet_title, &all_rows, has_header)?
        };
        atomic_write_bytes(&target, &bytes, 0o644)?;
        let disk_len = verify_written_file(&target)?;

        return Ok(WrittenDocumentSummary {
            path: target,
            format: if is_csv {
                "csv".to_string()
            } else {
                "xlsx".to_string()
            },
            rows_or_items: all_rows.len(),
            bytes: disk_len,
        });
    }

    let is_docx = ext == "docx";
    if !is_docx && target.exists() {
        if let Some(sg) = shadow_git {
            let _ = sg.snapshot_before_edit(&target);
        }
    }

    let new_lines = collect_document_lines(args);
    let body_lines = if append_mode && target.exists() {
        let prev = if is_docx {
            read_docx_text(&target).unwrap_or_default()
        } else {
            fs::read_to_string(&target).unwrap_or_default()
        };
        let mut merged: Vec<String> = prev.lines().map(str::to_string).collect();
        merged.extend(new_lines);
        merged
    } else {
        new_lines
    };

    let table_rows =
        build_effective_table_rows(&args.headers, &args.rows, args.sequence.as_deref());

    let bytes = if is_docx {
        let effective_title = if append_mode && target.exists() {
            None
        } else {
            args.title.as_deref()
        };
        build_docx_bytes(effective_title, &body_lines, &args.headers, &table_rows)?
    } else {
        let mut plain = render_plain_document(args.title.as_deref(), &body_lines);
        if !args.headers.is_empty() || !table_rows.is_empty() {
            if !args.headers.is_empty() {
                plain.push_str(&format!("\n| {} |\n", args.headers.join(" | ")));
            }
            for r in &table_rows {
                plain.push_str(&format!("| {} |\n", r.join(" | ")));
            }
        }
        plain.into_bytes()
    };

    atomic_write_bytes(&target, &bytes, 0o644)?;
    let disk_len = verify_written_file(&target)?;

    Ok(WrittenDocumentSummary {
        path: target,
        format: if is_docx { "docx".to_string() } else { ext },
        rows_or_items: body_lines.len() + table_rows.len(),
        bytes: disk_len,
    })
}

pub fn read_any_document(
    target: &Path,
    preferred_sheet: Option<&str>,
) -> crate::Result<String> {
    if !target.exists() {
        return Err(format!("File dokumen tidak ditemukan: {}", target.display()).into());
    }
    let ext = target
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    match ext.as_str() {
        "xlsx" | "ods" | "xls" => {
            let grid = read_spreadsheet_grid_calamine(target, preferred_sheet)?;
            Ok(format_grid_preview(target, &grid))
        }
        "csv" => {
            let grid = read_csv_grid(target)?;
            Ok(format_grid_preview(target, &grid))
        }
        "docx" => {
            let text = read_docx_text(target)?;
            let lines: Vec<&str> = text.lines().collect();
            let preview_count = lines.len().min(150);
            let preview = lines[..preview_count].join("\n");
            let trunc_note = if lines.len() > preview_count {
                "\n[Truncated]"
            } else {
                ""
            };
            Ok(format!(
                "status: success\nformat: docx\npath: {}\ntotal_lines: {}\n---\n{}{}",
                target.display(),
                lines.len(),
                preview,
                trunc_note
            ))
        }
        "pdf" => extract_pdf_text_bounded(target),
        _ => {
            let text = fs::read_to_string(target)?;
            let lines: Vec<&str> = text.lines().collect();
            let preview_count = lines.len().min(150);
            let preview = lines[..preview_count].join("\n");
            Ok(format!(
                "status: success\npath: {}\ntotal_lines: {}\n---\n{}",
                target.display(),
                lines.len(),
                preview
            ))
        }
    }
}

pub fn execute_manage_spreadsheet(
    args: &ManageSpreadsheetArgs,
    settings: &Settings,
) -> crate::Result<String> {
    let doc_args = args.clone().into_document_args();
    execute_manage_document_sync(&doc_args, settings, None, "manage_spreadsheet")
}

#[cfg(test)]
pub fn execute_manage_document(
    args: &ManageDocumentArgs,
    settings: &Settings,
) -> crate::Result<String> {
    execute_manage_document_sync(args, settings, None, "manage_document")
}

fn execute_manage_document_sync(
    args: &ManageDocumentArgs,
    settings: &Settings,
    shadow_git: Option<&ShadowGit>,
    tool_label: &str,
) -> crate::Result<String> {
    let target = resolve_document_target_path(&args.path, &settings.general.user_real_name);
    match args.action.as_str() {
        "read" => read_any_document(&target, args.sheet_name.as_deref()),
        "create" | "write" => {
            let summary = write_or_update_document_on_disk(args, settings, shadow_git, false)?;
            Ok(format!(
                "status: success\n\
                 tool: {tool_label}\n\
                 action: {}\n\
                 format: {}\n\
                 path: {}\n\
                 rows_or_paragraphs: {}\n\
                 bytes: {}\n\
                 verified_on_disk: true",
                args.action,
                summary.format,
                summary.path.display(),
                summary.rows_or_items,
                summary.bytes
            ))
        }
        "append" | "append_rows" => {
            let summary = write_or_update_document_on_disk(args, settings, shadow_git, true)?;
            Ok(format!(
                "status: success\n\
                 tool: {tool_label}\n\
                 action: {}\n\
                 format: {}\n\
                 path: {}\n\
                 total_items: {}\n\
                 bytes: {}\n\
                 verified_on_disk: true",
                args.action,
                summary.format,
                summary.path.display(),
                summary.rows_or_items,
                summary.bytes
            ))
        }
        other => Err(format!(
            "Aksi `{other}` tidak dikenal pada {tool_label}. Gunakan: create, write, append, append_rows, read, atau hot_edit."
        )
        .into()),
    }
}

pub async fn execute_manage_document_with_context(
    args: &ManageDocumentArgs,
    settings: &Settings,
    shadow_git: &ShadowGit,
    ws_state: &SessionWorkspaceState,
) -> crate::Result<String> {
    if args.action == "hot_edit" {
        return execute_hot_edit_document(
            args,
            settings,
            Some(shadow_git),
            ws_state,
            Some(&settings.desktop),
            false,
        )
        .await;
    }
    execute_manage_document_sync(args, settings, Some(shadow_git), "manage_document")
}

pub fn binary_exists_in_path(bin: &str) -> bool {
    if bin.contains('/') {
        return Path::new(bin).is_file();
    }
    let Some(path_os) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_os).any(|dir| dir.join(bin).is_file())
}

pub fn resolve_gui_opener_command(target_path: &Path, preferred_editor: Option<&str>) -> String {
    let quoted_file = shell_single_quote(&target_path.to_string_lossy());

    if let Some(ed) = preferred_editor.map(str::trim).filter(|s| !s.is_empty()) {
        let clean_ed = desktop::hyprland::strip_forced_qt_wayland_for_onlyoffice(ed);
        return format!("{clean_ed} {quoted_file}");
    }

    let ext = target_path
        .extension()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    let candidates: &[&str] = match ext.as_str() {
        "xlsx" | "xls" | "ods" | "csv" | "docx" | "doc" | "odt" | "pptx" | "odp" => &[
            "onlyoffice-desktopeditors",
            "desktopeditors",
            "libreoffice",
            "soffice",
            "xdg-open",
        ],
        "pdf" => &[
            "zathura",
            "evince",
            "okular",
            "papers",
            "onlyoffice-desktopeditors",
            "firefox",
            "xdg-open",
        ],
        "png" | "jpg" | "jpeg" | "webp" | "gif" | "svg" => {
            &["imv", "loupe", "feh", "gwenview", "eog", "xdg-open"]
        }
        "mp4" | "mkv" | "webm" | "mp3" | "flac" | "ogg" | "wav" => {
            &["mpv", "vlc", "celluloid", "xdg-open"]
        }
        _ => &["onlyoffice-desktopeditors", "libreoffice", "xdg-open"],
    };

    let chosen = candidates
        .iter()
        .copied()
        .find(|b| binary_exists_in_path(b))
        .unwrap_or("xdg-open");

    format!("{chosen} {quoted_file}")
}

pub fn infer_editor_from_window_class(window_class: &str) -> Option<&'static str> {
    let low = window_class.trim().to_ascii_lowercase();
    if low.contains("desktopeditors") || low.contains("onlyoffice") {
        Some("onlyoffice-desktopeditors")
    } else if low.contains("libreoffice") || low.contains("soffice") {
        Some("libreoffice")
    } else if low.contains("zathura") {
        Some("zathura")
    } else if low.contains("evince") {
        Some("evince")
    } else if low.contains("okular") {
        Some("okular")
    } else {
        None
    }
}

pub fn find_open_document_windows(windows: &[WindowInfo], target_path: &Path) -> Vec<WindowInfo> {
    let Some(file_name) = target_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
    else {
        return Vec::new();
    };
    let stem = target_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(|s| s.to_ascii_lowercase())
        .unwrap_or_default();

    windows
        .iter()
        .filter(|w| {
            let title_low = w.title.to_ascii_lowercase();
            let class_low = w.class.to_ascii_lowercase();
            if title_low.contains(&file_name) {
                return true;
            }
            let is_office_class = class_low.contains("desktopeditors")
                || class_low.contains("onlyoffice")
                || class_low.contains("libreoffice")
                || class_low.contains("soffice");
            is_office_class && !stem.is_empty() && stem.len() >= 4 && title_low.contains(&stem)
        })
        .cloned()
        .collect()
}

pub async fn launch_command_in_workspace_auto(
    command: &str,
    target_ws: i32,
    desktop_cfg: Option<&DesktopConfig>,
    backend: &dyn WindowBackend,
) -> Result<Option<WindowInfo>, String> {
    let kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);
    if kind == DesktopBackend::Hyprland {
        HyprlandBackend::new()
            .launch_app_in_workspace_async(command, target_ws)
            .await
    } else {
        backend.launch_app_in_workspace(command, target_ws)
    }
}

pub async fn hot_edit_document_with_backend<L, Fut>(
    args: &ManageDocumentArgs,
    settings: &Settings,
    shadow_git: Option<&ShadowGit>,
    ws_state: &SessionWorkspaceState,
    backend: &dyn WindowBackend,
    launch_fn: L,
    append_mode: bool,
) -> crate::Result<String>
where
    L: FnOnce(String, i32) -> Fut,
    Fut: std::future::Future<Output = Result<Option<WindowInfo>, String>>,
{
    let target = resolve_document_target_path(&args.path, &settings.general.user_real_name);

    let initial_windows = backend.list_windows().unwrap_or_default();
    let matched_wins = find_open_document_windows(&initial_windows, &target);

    let mut detected_ws: Option<i32> = None;
    let mut detected_editor: Option<&'static str> = None;
    let mut closed_count = 0usize;

    if !matched_wins.is_empty() {
        detected_ws = matched_wins.iter().map(|w| w.workspace).find(|&ws| ws > 0);
        detected_editor = matched_wins
            .iter()
            .find_map(|w| infer_editor_from_window_class(&w.class));

        for win in &matched_wins {
            let selector = if win.id.starts_with("0x") {
                format!("address:{}", win.id)
            } else {
                win.id.clone()
            };
            if backend.close_window(&selector).is_ok() {
                closed_count += 1;
            }
        }

        let deadline = Instant::now() + HOT_EDIT_CLOSE_WAIT;
        while Instant::now() < deadline {
            let cur = backend.list_windows().unwrap_or_default();
            if find_open_document_windows(&cur, &target).is_empty() {
                break;
            }
            tokio::time::sleep(HOT_EDIT_POLL_STEP).await;
        }
    }

    let summary = write_or_update_document_on_disk(args, settings, shadow_git, append_mode)?;

    let target_ws = detected_ws
        .or_else(|| args.workspace.filter(|&w| w > 0))
        .unwrap_or_else(|| {
            workspace_tracker::resolve_target_workspace_via_backend(None, ws_state, backend)
                .unwrap_or(2)
        });

    let preferred_editor = args.editor.as_deref().or(detected_editor);
    let launch_cmd = resolve_gui_opener_command(&summary.path, preferred_editor);
    let reopened = launch_fn(launch_cmd.clone(), target_ws)
        .await
        .map_err(|e| -> Box<dyn std::error::Error + Send + Sync> { e.into() })?;

    if let Some(win_info) = reopened {
        let norm_addr = desktop::normalize_window_address(&win_info.id)
            .unwrap_or_else(|| format!("address:{}", win_info.id));
        Ok(format!(
            "status: success\n\
             action: hot_edit\n\
             path: {}\n\
             format: {}\n\
             bytes: {}\n\
             verified_on_disk: true\n\
             closed_previous_windows: {closed_count}\n\
             verified_window: true\n\
             address: {norm_addr}\n\
             workspace_id: {}\n\
             class: {}\n\
             title: {}\n\
             do_not_poll_clients: true",
            summary.path.display(),
            summary.format,
            summary.bytes,
            win_info.workspace,
            win_info.class,
            win_info.title
        ))
    } else {
        Ok(format!(
            "status: success\n\
             action: hot_edit\n\
             path: {}\n\
             format: {}\n\
             bytes: {}\n\
             verified_on_disk: true\n\
             closed_previous_windows: {closed_count}\n\
             verified_window: false\n\
             workspace_id: {target_ws}\n\
             command: {launch_cmd}\n\
             do_not_poll_clients: true\n\
             note: File berhasil diperbarui di disk dan editor diluncurkan ulang ke workspace {target_ws}.",
            summary.path.display(),
            summary.format,
            summary.bytes
        ))
    }
}

pub async fn execute_hot_edit_document(
    args: &ManageDocumentArgs,
    settings: &Settings,
    shadow_git: Option<&ShadowGit>,
    ws_state: &SessionWorkspaceState,
    desktop_cfg: Option<&DesktopConfig>,
    append_mode: bool,
) -> crate::Result<String> {
    let backend_kind = desktop_cfg
        .map(DesktopConfig::effective_backend)
        .unwrap_or_else(detector::detect_desktop_backend);
    let backend = create_window_backend(backend_kind);
    let backend_for_launch = create_window_backend(backend_kind);
    let cfg_clone = desktop_cfg.cloned();

    hot_edit_document_with_backend(
        args,
        settings,
        shadow_git,
        ws_state,
        backend.as_ref(),
        move |cmd, ws| async move {
            launch_command_in_workspace_auto(
                &cmd,
                ws,
                cfg_clone.as_ref(),
                backend_for_launch.as_ref(),
            )
            .await
        },
        append_mode,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct MockHotReloadBackend {
        windows: Mutex<Vec<WindowInfo>>,
        closed_selectors: Mutex<Vec<String>>,
    }

    impl WindowBackend for MockHotReloadBackend {
        fn get_active_workspace(&self) -> Result<i32, String> {
            Ok(1)
        }

        fn list_workspaces(&self) -> Result<Vec<i32>, String> {
            Ok(vec![1, 4])
        }

        fn list_windows(&self) -> Result<Vec<WindowInfo>, String> {
            Ok(self.windows.lock().unwrap().clone())
        }

        fn close_window(&self, target: &str) -> Result<String, String> {
            self.closed_selectors.lock().unwrap().push(target.to_string());
            let clean = target.trim_start_matches("address:");
            self.windows
                .lock()
                .unwrap()
                .retain(|w| w.id != clean && w.id != target);
            Ok("ok".to_string())
        }

        fn close_workspaces(&self, _workspace_ids: &[i32]) -> Result<String, String> {
            Ok("ok".to_string())
        }

        fn launch_app_in_workspace(
            &self,
            _command: &str,
            _workspace: i32,
        ) -> Result<Option<WindowInfo>, String> {
            Ok(None)
        }
    }

    #[test]
    fn rust_xlsxwriter_and_calamine_roundtrip_with_formulas_and_sequences() {
        let dir = std::env::temp_dir().join(format!("waish-doc-xlsx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let settings = Settings::default();
        let xlsx_path = dir.join("aktuaris.xlsx");

        let create_args = ManageDocumentArgs {
            action: "create".to_string(),
            path: xlsx_path.to_string_lossy().into_owned(),
            title: None,
            content: String::new(),
            paragraphs: Vec::new(),
            sheet_name: Some("Tabel".to_string()),
            headers: vec!["Item".to_string(), "Nominal".to_string()],
            rows: vec![
                vec![json!("Kopi"), json!(25000)],
                vec![json!("Server"), json!(175000)],
                vec![json!("Total"), json!("=SUM(B2:B3)")],
            ],
            sequence: None,
            editor: None,
            workspace: None,
        };

        let write_out = execute_manage_document(&create_args, &settings).unwrap();
        assert!(write_out.contains("verified_on_disk: true"));
        assert!(write_out.contains("format: xlsx"));

        let read_args = ManageDocumentArgs {
            action: "read".to_string(),
            path: xlsx_path.to_string_lossy().into_owned(),
            title: None,
            content: String::new(),
            paragraphs: Vec::new(),
            sheet_name: Some("Tabel".to_string()),
            headers: Vec::new(),
            rows: Vec::new(),
            sequence: None,
            editor: None,
            workspace: None,
        };
        let table = execute_manage_document(&read_args, &settings).unwrap();
        assert!(table.contains("Item | Nominal"));
        assert!(table.contains("Kopi | 25000"));
        assert!(table.contains("Server | 175000"));
        assert!(table.contains("Total | =SUM(B2:B3)"));

        let seq_rows = build_effective_table_rows(&["No".to_string()], &[], Some("sequence:1..50"));
        assert_eq!(seq_rows.len(), 50);
        assert_eq!(seq_rows[0][0], "1");
        assert_eq!(seq_rows[49][0], "50");

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn docx_rs_roundtrip_preserves_headings_paragraphs_and_tables() {
        let dir = std::env::temp_dir().join(format!("waish-doc-docx-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let settings = Settings::default();
        let docx_path = dir.join("proposal.docx");

        let create_args = ManageDocumentArgs {
            action: "create".to_string(),
            path: docx_path.to_string_lossy().into_owned(),
            title: Some("Proposal Sistem".to_string()),
            content: "## Latar Belakang\n- Zero shelling out\nParagraf utama berjalan mulus."
                .to_string(),
            paragraphs: Vec::new(),
            sheet_name: None,
            headers: vec!["Komponen".to_string(), "Target".to_string()],
            rows: vec![vec![json!("Fast-Path"), json!("< 10ms")]],
            sequence: None,
            editor: None,
            workspace: None,
        };

        let out = execute_manage_document(&create_args, &settings).unwrap();
        assert!(out.contains("verified_on_disk: true"));
        assert!(out.contains("format: docx"));

        let read_out = read_any_document(&docx_path, None).unwrap();
        assert!(read_out.contains("Proposal Sistem"));
        assert!(read_out.contains("Latar Belakang"));
        assert!(read_out.contains("Zero shelling out"));
        assert!(read_out.contains("Fast-Path | < 10ms"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn pdf_extraction_and_truncation_guard_work_reliably() {
        let long_text: String = (1..=260)
            .map(|i| format!("Baris dokumen PDF nomor {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (bounded, truncated) =
            truncate_extracted_pdf_text(&long_text, PDF_MAX_EXTRACT_BYTES, PDF_MAX_EXTRACT_LINES);
        assert!(truncated);
        assert!(bounded.ends_with("[Truncated]"));
        assert!(bounded.lines().count() <= PDF_MAX_EXTRACT_LINES + 1);

        let dir = std::env::temp_dir().join(format!("waish-doc-pdf-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let pdf_path = dir.join("sample.pdf");

        let minimal_pdf = b"%PDF-1.4\n1 0 obj << /Type /Catalog /Pages 2 0 R >> endobj\n2 0 obj << /Type /Pages /Kids [3 0 R] /Count 1 >> endobj\n3 0 obj << /Type /Page /Parent 2 0 R /MediaBox [0 0 300 144] /Contents 4 0 R >> endobj\n4 0 obj << /Length 44 >> stream\nBT /F1 12 Tf 72 72 Td (Halo PDF Waish) Tj ET\nendstream endobj\nxref\n0 5\n0000000000 65535 f \n0000000009 00000 n \n0000000058 00000 n \n0000000115 00000 n \n0000000203 00000 n \ntrailer << /Size 5 /Root 1 0 R >>\nstartxref\n297\n%%EOF\n";
        fs::write(&pdf_path, minimal_pdf).unwrap();

        let extracted = read_any_document(&pdf_path, None).unwrap();
        assert!(extracted.contains("format: pdf"));
        assert!(extracted.contains("Halo PDF Waish"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn hot_edit_closes_active_editor_updates_file_and_reopens_same_workspace() {
        let dir = std::env::temp_dir().join(format!("waish-hot-edit-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let xlsx_path = dir.join("tabel_actuaria.xlsx");
        let settings = Settings::default();
        let ws_state = SessionWorkspaceState::new();

        let mock_backend = MockHotReloadBackend {
            windows: Mutex::new(vec![WindowInfo {
                id: "0xabc123".to_string(),
                class: "DesktopEditors".to_string(),
                initial_class: "DesktopEditors".to_string(),
                title: "tabel_actuaria.xlsx - ONLYOFFICE".to_string(),
                workspace: 6,
            }]),
            closed_selectors: Mutex::new(Vec::new()),
        };

        let edit_args = ManageDocumentArgs {
            action: "hot_edit".to_string(),
            path: xlsx_path.to_string_lossy().into_owned(),
            title: None,
            content: String::new(),
            paragraphs: Vec::new(),
            sheet_name: Some("Revisi".to_string()),
            headers: vec!["Usia".to_string(), "Premi".to_string()],
            rows: vec![vec![json!(25), json!(450000)]],
            sequence: None,
            editor: None,
            workspace: None,
        };

        let result = hot_edit_document_with_backend(
            &edit_args,
            &settings,
            None,
            &ws_state,
            &mock_backend,
            |cmd, ws| async move {
                assert_eq!(ws, 6, "must reopen in exact previous workspace 6");
                assert!(
                    cmd.contains("onlyoffice-desktopeditors"),
                    "must preserve inferred OnlyOffice editor: {cmd}"
                );
                assert!(!cmd.contains("QT_QPA_PLATFORM=wayland"));
                Ok(Some(WindowInfo {
                    id: "0xdef456".to_string(),
                    class: "DesktopEditors".to_string(),
                    initial_class: "DesktopEditors".to_string(),
                    title: "tabel_actuaria.xlsx - ONLYOFFICE".to_string(),
                    workspace: ws,
                }))
            },
            false,
        )
        .await
        .unwrap();

        assert!(result.contains("action: hot_edit"));
        assert!(result.contains("closed_previous_windows: 1"));
        assert!(result.contains("verified_window: true"));
        assert!(result.contains("workspace_id: 6"));
        assert_eq!(
            mock_backend.closed_selectors.lock().unwrap().as_slice(),
            &["address:0xabc123".to_string()]
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
