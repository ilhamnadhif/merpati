//! Iteration data files (FR-U2): CSV with a header row, or a JSON array of
//! objects. Each row/object becomes the data scope of one iteration.

use std::path::Path;

use indexmap::IndexMap;
use serde_json::Value;

pub type DataRow = IndexMap<String, Value>;

#[derive(Debug, thiserror::Error)]
pub enum DataError {
    #[error("could not read data file {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("invalid CSV data: {0}")]
    Csv(String),
    #[error("invalid JSON data: {0}")]
    Json(String),
    #[error("JSON data must be an array of objects")]
    JsonShape,
}

pub fn load(path: &Path) -> Result<Vec<DataRow>, DataError> {
    let text =
        std::fs::read_to_string(path).map_err(|source| DataError::Io { path: path.display().to_string(), source })?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
    let is_json =
        path.extension().map(|e| e.eq_ignore_ascii_case("json")).unwrap_or(false) || text.trim_start().starts_with('[');
    if is_json { parse_json(text) } else { parse_csv(text) }
}

pub fn parse_csv(text: &str) -> Result<Vec<DataRow>, DataError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    // Whitespace around delimiters is ignored, like newman (`trim: true`).
    let mut rdr = csv::ReaderBuilder::new().flexible(true).trim(csv::Trim::All).from_reader(text.as_bytes());
    let headers: Vec<String> =
        rdr.headers().map_err(|e| DataError::Csv(e.to_string()))?.iter().map(str::to_string).collect();
    let mut rows = Vec::new();
    for rec in rdr.records() {
        let rec = rec.map_err(|e| DataError::Csv(e.to_string()))?;
        if rec.iter().all(str::is_empty) {
            continue;
        }
        let mut row = DataRow::new();
        for (i, h) in headers.iter().enumerate() {
            row.insert(h.clone(), Value::String(rec.get(i).unwrap_or("").to_string()));
        }
        rows.push(row);
    }
    Ok(rows)
}

pub fn parse_json(text: &str) -> Result<Vec<DataRow>, DataError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let v: Value = serde_json::from_str(text).map_err(|e| DataError::Json(e.to_string()))?;
    let Value::Array(items) = v else { return Err(DataError::JsonShape) };
    items
        .into_iter()
        .map(|item| match item {
            Value::Object(o) => Ok(o.into_iter().collect()),
            _ => Err(DataError::JsonShape),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn csv_rows() {
        let rows = parse_csv("\u{feff}user,pass\nada,\"p,1\"\n\nbob,x\n").unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["pass"], Value::String("p,1".into()));
        assert_eq!(rows[1]["user"], Value::String("bob".into()));
    }

    #[test]
    fn csv_ignores_whitespace_around_delimiters() {
        // newman parses CSV with `trim: true`: `user, pass` names the column `pass`.
        let rows = parse_csv("user, pass\r\nada , p1\r\n   \r\n").unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].keys().collect::<Vec<_>>(), vec!["user", "pass"]);
        assert_eq!(rows[0]["user"], Value::String("ada".into()));
        assert_eq!(rows[0]["pass"], Value::String("p1".into()));
    }

    #[test]
    fn json_rows_keep_types() {
        let rows = parse_json(r#"[{"id": 1, "ok": true}, {"id": 2}]"#).unwrap();
        assert_eq!(rows[0]["id"], Value::from(1));
        assert_eq!(rows[0]["ok"], Value::Bool(true));
        assert!(parse_json("{}").is_err());
    }

    #[test]
    fn json_with_byte_order_mark() {
        let rows = parse_json("\u{feff}[{\"user\": \"ada\"}]").unwrap();
        assert_eq!(rows[0]["user"], Value::String("ada".into()));
        // Detected as JSON by content even without a .json extension.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("users.txt");
        std::fs::write(&path, "\u{feff}[{\"user\": \"bob\"}]").unwrap();
        assert_eq!(load(&path).unwrap()[0]["user"], Value::String("bob".into()));
    }
}
