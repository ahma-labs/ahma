//! Serde helpers for `SystemTime` as RFC 3339 text.
//!
//! The spelling is [`ahma_common::timestamp::rfc3339_local`]'s: chrono's
//! `to_rfc3339()` at the local offset, as every earlier version wrote it, so
//! operation JSON keeps its shape and readers on either side of the upgrade
//! (the TUI's `parse_rfc3339_field`) parse it unchanged (R-TIME.1).
use ahma_common::timestamp;
use serde::{self, Deserialize, Deserializer, Serializer};
use std::time::SystemTime;

/// Serializes a `SystemTime` to an RFC 3339 string.
pub fn serialize<S>(time: &SystemTime, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    let text = timestamp::rfc3339_local(*time).map_err(serde::ser::Error::custom)?;
    serializer.serialize_str(&text)
}

/// Deserializes an RFC 3339 string to a `SystemTime`.
pub fn deserialize<'de, D>(deserializer: D) -> Result<SystemTime, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer)?;
    timestamp::parse_rfc3339(&s).map_err(serde::de::Error::custom)
}

/// A module for serializing and deserializing `Option<SystemTime>`.
pub mod option {
    use super::*;
    use serde::{Deserialize, Deserializer, Serializer};

    /// Serializes an `Option<SystemTime>` to an RFC 3339 string.
    pub fn serialize<S>(time: &Option<SystemTime>, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match time {
            Some(t) => {
                let text = timestamp::rfc3339_local(*t).map_err(serde::ser::Error::custom)?;
                serializer.serialize_some(&text)
            }
            None => serializer.serialize_none(),
        }
    }

    /// Deserializes an `Option` from an RFC 3339 string to `Option<SystemTime>`.
    pub fn deserialize<'de, D>(deserializer: D) -> Result<Option<SystemTime>, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Temp(#[serde(with = "super")] SystemTime);

        let opt: Option<Temp> = Option::deserialize(deserializer)?;
        Ok(opt.map(|Temp(st)| st))
    }
}

#[cfg(test)]
mod tests {
    use serde::{Deserialize, Serialize};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct Times {
        #[serde(with = "super")]
        start: SystemTime,
        #[serde(with = "super::option", default)]
        end: Option<SystemTime>,
    }

    /// 2026-11-21T09:05:03.120Z.
    fn t() -> SystemTime {
        UNIX_EPOCH + Duration::new(1_795_251_903, 120_000_000)
    }

    /// Values earlier versions wrote (chrono `DateTime<Local>::to_rfc3339()`,
    /// at whatever offset the writer had) still read back to the same instant.
    #[test]
    fn reads_what_chrono_wrote() {
        let old =
            r#"{"start":"2026-11-21T10:05:03.120+01:00","end":"2026-11-21T04:05:03.120-05:00"}"#;
        let got: Times = serde_json::from_str(old).unwrap();
        assert_eq!(
            got,
            Times {
                start: t(),
                end: Some(t())
            }
        );
        let none: Times =
            serde_json::from_str(r#"{"start":"2026-11-21T09:05:03.120+00:00","end":null}"#)
                .unwrap();
        assert_eq!(none.end, None);
        let missing: Times =
            serde_json::from_str(r#"{"start":"2026-11-21T09:05:03.120Z"}"#).unwrap();
        assert_eq!(missing.end, None);
    }

    /// The written spelling is chrono's: numeric offset (never `Z`), and a
    /// 3/6/9-digit fraction — `.120`, not jiff's `.12`.
    #[test]
    fn writes_chrono_spelling_and_round_trips() {
        let v = Times {
            start: t(),
            end: None,
        };
        let json = serde_json::to_value(&v).unwrap();
        let start = json["start"].as_str().unwrap();
        assert!(start.contains(":03.120"), "{start}");
        assert!(!start.ends_with('Z'), "{start}");
        let offset = &start[start.len() - 6..];
        assert!(
            matches!(offset.as_bytes(), [b'+' | b'-', _, _, b':', _, _]),
            "±HH:MM offset: {start}"
        );
        assert!(json["end"].is_null());
        let back: Times = serde_json::from_value(json).unwrap();
        assert_eq!(back, v);
    }

    #[test]
    fn rejects_text_without_an_offset() {
        assert!(serde_json::from_str::<Times>(r#"{"start":"2026-11-21T09:05:03"}"#).is_err());
    }
}
