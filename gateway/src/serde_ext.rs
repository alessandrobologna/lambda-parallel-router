//! Serde helpers for CloudFormation-friendly parsing.
//!
//! When passing arbitrary objects through CloudFormation (for example, as custom resource
//! properties), numeric YAML scalars are commonly converted to strings. These helpers allow
//! parsing integers from either JSON/YAML numbers or strings.

use std::fmt;

use serde::de::{self, Deserializer, Visitor};

pub fn de_u64_or_string<'de, D>(deserializer: D) -> Result<u64, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;

    impl<'de> Visitor<'de> for V {
        type Value = u64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an integer (u64) or a string containing an integer")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if v < 0 {
                return Err(E::custom("expected a non-negative integer"));
            }
            Ok(v as u64)
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !v.is_finite() || v < 0.0 {
                return Err(E::custom("expected a non-negative integer"));
            }
            if v.fract() != 0.0 {
                return Err(E::custom("expected an integer without a fractional part"));
            }
            Ok(v as u64)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let trimmed = v.trim();
            trimmed
                .parse::<u64>()
                .map_err(|_| E::custom("expected a string containing an integer"))
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(V)
}

pub fn de_usize_or_string<'de, D>(deserializer: D) -> Result<usize, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;

    impl<'de> Visitor<'de> for V {
        type Value = usize;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an integer (usize) or a string containing an integer")
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            usize::try_from(v).map_err(|_| E::custom("integer is too large for usize"))
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if v < 0 {
                return Err(E::custom("expected a non-negative integer"));
            }
            self.visit_u64(v as u64)
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            if !v.is_finite() || v < 0.0 {
                return Err(E::custom("expected a non-negative integer"));
            }
            if v.fract() != 0.0 {
                return Err(E::custom("expected an integer without a fractional part"));
            }
            self.visit_u64(v as u64)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let trimmed = v.trim();
            let parsed = trimmed
                .parse::<u64>()
                .map_err(|_| E::custom("expected a string containing an integer"))?;
            self.visit_u64(parsed)
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(V)
}

pub fn de_option_u64_or_string<'de, D>(deserializer: D) -> Result<Option<u64>, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;

    impl<'de> Visitor<'de> for V {
        type Value = Option<u64>;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("an optional integer (u64) or a string containing an integer")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            de_u64_or_string(deserializer).map(Some)
        }
    }

    deserializer.deserialize_option(V)
}

pub fn de_f64_or_string<'de, D>(deserializer: D) -> Result<f64, D::Error>
where
    D: Deserializer<'de>,
{
    struct V;

    impl<'de> Visitor<'de> for V {
        type Value = f64;

        fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
            formatter.write_str("a number (f64) or a string containing a number")
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E> {
            Ok(v)
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E> {
            Ok(v as f64)
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E> {
            Ok(v as f64)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let trimmed = v.trim();
            trimmed
                .parse::<f64>()
                .map_err(|_| E::custom("expected a string containing a number"))
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&v)
        }
    }

    deserializer.deserialize_any(V)
}

#[cfg(test)]
mod tests {
    use serde::Deserialize;

    use super::*;

    #[derive(Debug, Deserialize)]
    struct T {
        #[serde(deserialize_with = "de_u64_or_string")]
        v: u64,
        #[serde(deserialize_with = "de_usize_or_string")]
        n: usize,
        #[serde(default, deserialize_with = "de_option_u64_or_string")]
        o: Option<u64>,
        #[serde(deserialize_with = "de_f64_or_string")]
        f: f64,
    }

    #[test]
    fn parses_numbers_and_strings() {
        let yaml = br#"
v: "25"
n: "4"
o: 12
f: "1.5"
"#;
        let t: T = serde_yaml::from_slice(yaml).unwrap();
        assert_eq!(t.v, 25);
        assert_eq!(t.n, 4);
        assert_eq!(t.o, Some(12));
        assert_eq!(t.f, 1.5);
    }

    #[test]
    fn option_none_is_ok() {
        let yaml = br#"
v: 1
n: 1
f: 0.5
"#;
        let t: T = serde_yaml::from_slice(yaml).unwrap();
        assert_eq!(t.o, None);
    }
}
