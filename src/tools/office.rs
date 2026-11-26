//! Legacy compatibility layer delegating office operations to `crate::tools::document`.

#[cfg(test)]
mod tests {
    use crate::config::settings::Settings;
    use crate::tools::document::{
        execute_manage_document, execute_manage_spreadsheet, ManageDocumentArgs,
        ManageSpreadsheetArgs,
    };
    use serde_json::json;
    use std::fs;

    #[test]
    fn native_xlsx_and_docx_roundtrip_without_python_dependency() {
        let dir = std::env::temp_dir().join(format!("waish-office-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let settings = Settings::default();
        let xlsx_path = dir.join("anggaran.xlsx");

        let create_args = ManageSpreadsheetArgs {
            action: "create".to_string(),
            path: xlsx_path.to_string_lossy().into_owned(),
            sheet_name: Some("Keuangan".to_string()),
            headers: vec!["Pos".to_string(), "Nominal".to_string()],
            rows: vec![vec![json!("Kopi"), json!(25000)]],
            sequence: None,
        };
        let out = execute_manage_spreadsheet(&create_args, &settings).unwrap();
        assert!(out.contains("verified_on_disk: true"));
        assert!(xlsx_path.exists());

        let append_args = ManageSpreadsheetArgs {
            action: "append_rows".to_string(),
            path: xlsx_path.to_string_lossy().into_owned(),
            sheet_name: Some("Keuangan".to_string()),
            headers: vec![],
            rows: vec![vec![json!("Internet"), json!(350000)]],
            sequence: None,
        };
        execute_manage_spreadsheet(&append_args, &settings).unwrap();

        let read_args = ManageSpreadsheetArgs {
            action: "read".to_string(),
            path: xlsx_path.to_string_lossy().into_owned(),
            sheet_name: Some("Keuangan".to_string()),
            headers: vec![],
            rows: vec![],
            sequence: None,
        };
        let table = execute_manage_spreadsheet(&read_args, &settings).unwrap();
        assert!(table.contains("Pos | Nominal"));
        assert!(table.contains("Kopi | 25000"));
        assert!(table.contains("Internet | 350000"));

        let docx_path = dir.join("ringkasan.docx");
        let doc_create = ManageDocumentArgs {
            action: "create".to_string(),
            path: docx_path.to_string_lossy().into_owned(),
            title: Some("Laporan Mingguan".to_string()),
            content: "## Ringkasan\n- Item satu beres\nParagraf penutup.".to_string(),
            paragraphs: vec![],
            sheet_name: None,
            headers: vec![],
            rows: vec![],
            sequence: None,
            editor: None,
            workspace: None,
        };
        let doc_out = execute_manage_document(&doc_create, &settings).unwrap();
        assert!(doc_out.contains("verified_on_disk: true"));

        let doc_read = ManageDocumentArgs {
            action: "read".to_string(),
            path: docx_path.to_string_lossy().into_owned(),
            title: None,
            content: String::new(),
            paragraphs: vec![],
            sheet_name: None,
            headers: vec![],
            rows: vec![],
            sequence: None,
            editor: None,
            workspace: None,
        };
        let doc_text = execute_manage_document(&doc_read, &settings).unwrap();
        assert!(doc_text.contains("Laporan Mingguan"));
        assert!(doc_text.contains("Item satu beres"));
        assert!(doc_text.contains("Paragraf penutup."));

        let _ = fs::remove_dir_all(&dir);
    }
}
