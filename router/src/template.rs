//! Tiny `${VAR}` substitution helper for YAML/JSON templates.
//!
//! This is used for deployment-friendly configuration where the container image can ship a static
//! config/spec file that references environment variables injected by the runtime (for example,
//! an App Runner service can inject the deployed Lambda ARN into the router's spec at startup).

/// Render a template by replacing `${NAME}` placeholders with values provided by `lookup`.
///
/// Supported placeholder syntax:
/// - `${NAME}` where `NAME` matches `[A-Za-z_][A-Za-z0-9_]*`.
///
/// Any placeholder with an unset variable results in an error.
pub fn render_env_template_with(
    input: &str,
    mut lookup: impl FnMut(&str) -> Option<String>,
) -> anyhow::Result<String> {
    let bytes = input.as_bytes();
    let mut out = String::with_capacity(input.len());

    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == b'$' && i + 1 < bytes.len() && bytes[i + 1] == b'{' {
            let start = i + 2;
            let Some(end_rel) = bytes[start..].iter().position(|b| *b == b'}') else {
                anyhow::bail!("unterminated placeholder");
            };
            let end = start + end_rel;
            let name = std::str::from_utf8(&bytes[start..end])?;
            validate_env_name(name)?;

            let Some(value) = lookup(name) else {
                anyhow::bail!("missing environment variable: {name}");
            };

            out.push_str(&value);
            i = end + 1;
            continue;
        }

        out.push(bytes[i] as char);
        i += 1;
    }

    Ok(out)
}

fn validate_env_name(name: &str) -> anyhow::Result<()> {
    if name.is_empty() {
        anyhow::bail!("empty placeholder name");
    }

    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        anyhow::bail!("empty placeholder name");
    };
    if !(first.is_ascii_alphabetic() || first == '_') {
        anyhow::bail!("invalid placeholder name: {name}");
    }
    for ch in chars {
        if !(ch.is_ascii_alphanumeric() || ch == '_') {
            anyhow::bail!("invalid placeholder name: {name}");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn replaces_single_placeholder() {
        let env = HashMap::from([("NAME".to_string(), "world".to_string())]);
        let out = render_env_template_with("hello ${NAME}", |k| env.get(k).cloned()).unwrap();
        assert_eq!(out, "hello world");
    }

    #[test]
    fn replaces_multiple_placeholders() {
        let env = HashMap::from([
            ("A".to_string(), "1".to_string()),
            ("B".to_string(), "2".to_string()),
        ]);
        let out = render_env_template_with("${A}-${B}", |k| env.get(k).cloned()).unwrap();
        assert_eq!(out, "1-2");
    }

    #[test]
    fn missing_placeholder_is_an_error() {
        let env = HashMap::<String, String>::new();
        assert!(render_env_template_with("${MISSING}", |k| env.get(k).cloned()).is_err());
    }

    #[test]
    fn invalid_placeholder_name_is_an_error() {
        let env = HashMap::from([("X".to_string(), "ok".to_string())]);
        assert!(render_env_template_with("${123}", |k| env.get(k).cloned()).is_err());
        assert!(render_env_template_with("${X-Y}", |k| env.get(k).cloned()).is_err());
    }

    #[test]
    fn unterminated_placeholder_is_an_error() {
        let env = HashMap::<String, String>::new();
        assert!(render_env_template_with("${X", |k| env.get(k).cloned()).is_err());
    }
}
