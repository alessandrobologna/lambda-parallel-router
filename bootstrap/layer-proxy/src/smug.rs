use bytes::Bytes;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug)]
pub struct VirtualInvocation {
    pub id: String,
    pub event_bytes: Bytes,
}

#[derive(Debug, Deserialize)]
struct OuterBatchEnvelope {
    v: u8,
    #[serde(default)]
    batch: Vec<Value>,
}

pub fn parse_outer_smug_batch(body: &[u8]) -> anyhow::Result<Option<Vec<VirtualInvocation>>> {
    let env: OuterBatchEnvelope = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => return Ok(None),
    };

    if env.v != 1 {
        return Ok(None);
    }

    if env.batch.is_empty() {
        return Ok(Some(Vec::new()));
    }

    let mut out = Vec::with_capacity(env.batch.len());
    let mut seen = HashSet::<String>::with_capacity(env.batch.len());
    for item in env.batch {
        let id = item
            .get("requestContext")
            .and_then(|v| v.get("requestId"))
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("batch item missing requestContext.requestId"))?;

        if !seen.insert(id.to_string()) {
            anyhow::bail!("duplicate requestContext.requestId in batch ({id})");
        }

        let event_bytes = serde_json::to_vec(&item)?;
        out.push(VirtualInvocation {
            id: id.to_string(),
            event_bytes: Bytes::from(event_bytes),
        });
    }

    Ok(Some(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_outer_batch_rejects_non_json() {
        let got = parse_outer_smug_batch(b"nope").unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn parse_outer_batch_rejects_wrong_version() {
        let got = parse_outer_smug_batch(br#"{"v":2,"batch":[]}"#).unwrap();
        assert!(got.is_none());
    }

    #[test]
    fn parse_outer_batch_extracts_ids() {
        let got = parse_outer_smug_batch(
            br#"{"v":1,"batch":[{"requestContext":{"requestId":"a"}},{"requestContext":{"requestId":"b"}}]}"#,
        )
        .unwrap()
        .unwrap();

        assert_eq!(got.len(), 2);
        assert_eq!(got[0].id, "a");
        assert_eq!(got[1].id, "b");
    }

    #[test]
    fn parse_outer_batch_rejects_duplicate_ids() {
        let err = parse_outer_smug_batch(
            br#"{"v":1,"batch":[{"requestContext":{"requestId":"a"}},{"requestContext":{"requestId":"a"}}]}"#,
        )
        .unwrap_err();

        assert!(err.to_string().contains("duplicate"));
    }
}
