use std::collections::HashMap;

use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct StreamRecord {
    pub v: u8,
    pub id: String,
    #[serde(rename = "statusCode")]
    pub status_code: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub cookies: Vec<String>,
    #[serde(default)]
    pub body: String,
    #[serde(rename = "isBase64Encoded", default)]
    pub is_base64_encoded: bool,
}

pub fn encode_record_line(record: &StreamRecord) -> anyhow::Result<Vec<u8>> {
    let mut out = serde_json::to_vec(record)?;
    out.push(b'\n');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_record_line_terminates_newline() {
        let rec = StreamRecord {
            v: 1,
            id: "a".into(),
            status_code: 200,
            headers: HashMap::new(),
            cookies: Vec::new(),
            body: "".into(),
            is_base64_encoded: false,
        };

        let line = encode_record_line(&rec).unwrap();
        assert_eq!(line.last().copied(), Some(b'\n'));
    }
}
