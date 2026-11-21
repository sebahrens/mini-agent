//! Tolerant argument parsing shared by the built-in tool argument structs
//! (mini-agent-sskdu.257, .183).
//!
//! Models, small ones especially, often send numbers and booleans as strings
//! (`"limit":"200"`, `"no_ignore":"true"`) or integral floats (`200.0`). The
//! helpers accept those spellings and, for anything else, name the field:
//! `limit: expected an integer, got the string "abc"`.

use serde::de::{Deserialize, Deserializer, Error};
use serde_json::Value;

fn describe(value: &Value) -> String {
    match value {
        Value::String(text) => format!("the string {text:?}"),
        Value::Number(number) => format!("the number {number}"),
        Value::Bool(flag) => format!("the boolean {flag}"),
        Value::Array(_) => "an array".to_string(),
        Value::Object(_) => "an object".to_string(),
        Value::Null => "null".to_string(),
    }
}

fn integral_float(number: f64) -> Option<u64> {
    // 2^53 is the largest range in which every integer is exact in an f64.
    (number >= 0.0 && number.fract() == 0.0 && number <= 9_007_199_254_740_992.0)
        .then_some(number as u64)
}

fn to_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .or_else(|| number.as_f64().and_then(integral_float)),
        Value::String(text) => {
            let text = text.trim();
            text.parse::<u64>()
                .ok()
                .or_else(|| text.parse::<f64>().ok().and_then(integral_float))
        }
        _ => None,
    }
}

/// An optional non-negative integer that also accepts `"200"` and `200.0`.
pub(crate) fn optional_integer<'de, D, T>(
    deserializer: D,
    field: &str,
) -> Result<Option<T>, D::Error>
where
    D: Deserializer<'de>,
    T: TryFrom<u64>,
{
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    if value.is_null() {
        return Ok(None);
    }
    to_u64(&value)
        .and_then(|number| T::try_from(number).ok())
        .map(Some)
        .ok_or_else(|| {
            D::Error::custom(format!(
                "{field}: expected a non-negative integer, got {}",
                describe(&value)
            ))
        })
}

fn boolean<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<bool>, D::Error> {
    let Some(value) = Option::<Value>::deserialize(deserializer)? else {
        return Ok(None);
    };
    match &value {
        Value::Null => Ok(None),
        Value::Bool(flag) => Ok(Some(*flag)),
        Value::String(text) => match text.trim().to_ascii_lowercase().as_str() {
            "true" => Ok(Some(true)),
            "false" => Ok(Some(false)),
            _ => Err(D::Error::custom(format!(
                "expected a boolean (true or false), got {}",
                describe(&value)
            ))),
        },
        _ => Err(D::Error::custom(format!(
            "expected a boolean (true or false), got {}",
            describe(&value)
        ))),
    }
}

/// A boolean that defaults to false and also accepts `"true"` / `"false"`.
pub(crate) fn flag<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    boolean(deserializer).map(Option::unwrap_or_default)
}

/// An optional boolean that also accepts `"true"` / `"false"`.
pub(crate) fn optional_flag<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<bool>, D::Error> {
    boolean(deserializer)
}

/// Define a named `deserialize_with` function for one integer field so the
/// error message can name it.
macro_rules! integer_field {
    ($name:ident, $field:literal, $ty:ty) => {
        pub(crate) fn $name<'de, D: serde::Deserializer<'de>>(
            deserializer: D,
        ) -> Result<Option<$ty>, D::Error> {
            $crate::agent::tools::lenient::optional_integer(deserializer, $field)
        }
    };
}

integer_field!(offset_usize, "offset", usize);
integer_field!(offset_u64, "offset", u64);
integer_field!(limit_usize, "limit", usize);
integer_field!(depth_usize, "depth", usize);
integer_field!(context_lines_usize, "context_lines", usize);
integer_field!(character_offset_usize, "character_offset", usize);
integer_field!(character_limit_usize, "character_limit", usize);
integer_field!(max_results_u64, "max_results", u64);
integer_field!(since_u64, "since", u64);
integer_field!(timeout_ms_u64, "timeout_ms", u64);

/// Shell deadline in milliseconds. Values from 1 to 999 are refused with the
/// seconds/milliseconds hint: a model thinking in seconds (`"timeout": 120`)
/// would otherwise get a 120 ms deadline that kills the command at once.
pub(crate) fn shell_timeout_ms<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<u64>, D::Error> {
    let value = optional_integer::<D, u64>(deserializer, "timeout_ms")?;
    if let Some(ms) = value.filter(|ms| (1..1000).contains(ms)) {
        return Err(D::Error::custom(format!(
            "timeout_ms is in milliseconds; {ms} ms is under one second. Did you mean {}?",
            ms * 1000
        )));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    use crate::agent::tools::{
        BashArgs, FindFilesArgs, GrepArgs, JobStatusArgs, ListDirArgs, ReadArgs,
    };
    use serde_json::json;

    fn error_of<T: serde::de::DeserializeOwned>(value: serde_json::Value) -> String {
        match serde_json::from_value::<T>(value) {
            Ok(_) => panic!("expected an error"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn strings_and_integral_floats_are_accepted_for_numbers_and_booleans() {
        let read: ReadArgs = serde_json::from_value(
            json!({"path":"a.rs","limit":"200","offset":3.0,"raw_notebook":"true"}),
        )
        .unwrap();
        assert_eq!(
            (read.limit, read.offset, read.raw_notebook),
            (Some(200), Some(3), true)
        );
        let grep: GrepArgs = serde_json::from_value(json!({
            "pattern":"x","max_results":"5","context_lines":2.0,"offset":"7","count":"false","no_ignore":true
        }))
        .unwrap();
        assert_eq!(
            (grep.max_results, grep.context_lines, grep.offset),
            (Some(5), Some(2), Some(7))
        );
        assert!(!grep.count && grep.no_ignore);
        let find: FindFilesArgs =
            serde_json::from_value(json!({"pattern":"*","limit":"9","offset":"1"})).unwrap();
        assert_eq!((find.limit, find.offset), (Some(9), Some(1)));
        let list: ListDirArgs = serde_json::from_value(json!({"depth":"2","limit":null})).unwrap();
        assert_eq!((list.depth, list.limit), (Some(2), None));
        let job: JobStatusArgs =
            serde_json::from_value(json!({"timeout_ms":"1500","since":"10"})).unwrap();
        assert_eq!((job.timeout_ms, job.since), (Some(1500), Some(10)));
        let shell: BashArgs =
            serde_json::from_value(json!({"command":"ls","timeout":"5000","background":"true"}))
                .unwrap();
        assert_eq!((shell.timeout, shell.background), (Some(5000), true));
    }

    #[test]
    fn type_errors_name_the_field_and_the_offending_value() {
        let error = error_of::<ReadArgs>(json!({"path":"a.rs","limit":"abc"}));
        assert!(
            error.contains(r#"limit: expected a non-negative integer, got the string "abc""#),
            "{error}"
        );
        let error = error_of::<ReadArgs>(json!({"path":"a.rs","offset":1.5}));
        assert!(
            error.contains("offset: expected a non-negative integer, got the number 1.5"),
            "{error}"
        );
        let error = error_of::<ReadArgs>(json!({"path":"a.rs","limit":-1}));
        assert!(
            error.contains("limit: expected a non-negative integer"),
            "{error}"
        );
        let error = error_of::<GrepArgs>(json!({"pattern":"x","count":"maybe"}));
        assert!(error.contains("expected a boolean"), "{error}");
    }

    // mini-agent-sskdu.183: shell timeouts are milliseconds; sub-second values
    // from a model thinking in seconds get the hint instead of a 30 ms deadline.
    #[test]
    fn shell_timeout_below_one_second_is_refused_with_a_hint() {
        let error = error_of::<BashArgs>(json!({"command":"sleep 1","timeout":30}));
        assert!(
            error.contains("milliseconds") && error.contains("Did you mean 30000"),
            "{error}"
        );
        let aliased: BashArgs =
            serde_json::from_value(json!({"command":"sleep 1","timeout_ms":30_000})).unwrap();
        assert_eq!(aliased.timeout, Some(30_000));
        // Zero keeps its own validation message in the tool.
        let zero: BashArgs = serde_json::from_value(json!({"command":"x","timeout":0})).unwrap();
        assert_eq!(zero.timeout, Some(0));
    }
}
