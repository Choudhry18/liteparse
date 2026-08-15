use serde::{Deserialize, Deserializer};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct IntegerMeasure {
    value: i64,
    negative: bool,
}

impl IntegerMeasure {
    pub(crate) fn value(self) -> i64 {
        self.value
    }

    pub(crate) fn is_negative(&self) -> bool {
        self.negative
    }
}

impl<'de> Deserialize<'de> for IntegerMeasure {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        parse_integer_measure(&raw).map_err(serde::de::Error::custom)
    }
}

fn parse_integer_measure(raw: &str) -> Result<IntegerMeasure, &'static str> {
    if let Ok(value) = raw.parse::<i64>() {
        return Ok(IntegerMeasure {
            value,
            negative: raw.starts_with('-'),
        });
    }

    let (negative, unsigned) = match raw.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, raw.strip_prefix('+').unwrap_or(raw)),
    };
    let (whole, fraction) = unsigned
        .split_once('.')
        .ok_or("expected an integer or decimal measurement")?;
    if whole.is_empty()
        || fraction.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || !fraction.bytes().all(|b| b.is_ascii_digit())
    {
        return Err("invalid decimal measurement");
    }

    let magnitude = whole
        .parse::<i128>()
        .map_err(|_| "measurement is outside the supported range")?;
    let rounded_magnitude = magnitude
        .checked_add(i128::from(fraction.as_bytes()[0] >= b'5'))
        .ok_or("measurement is outside the supported range")?;
    let signed = if negative {
        -rounded_magnitude
    } else {
        rounded_magnitude
    };
    let value = i64::try_from(signed).map_err(|_| "measurement is outside the supported range")?;
    Ok(IntegerMeasure { value, negative })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Deserialize)]
    struct Value {
        #[serde(rename = "@val")]
        val: IntegerMeasure,
    }

    fn parse(raw: &str) -> Result<i64, quick_xml::DeError> {
        quick_xml::de::from_str::<Value>(&format!(r#"<x val="{raw}"/>"#))
            .map(|value| value.val.value())
    }

    #[test]
    fn accepts_integer_and_decimal_measurements() {
        assert_eq!(parse("0").unwrap(), 0);
        assert_eq!(parse("-120").unwrap(), -120);
        assert_eq!(parse("100.0").unwrap(), 100);
        assert_eq!(parse("252.00000000000003").unwrap(), 252);
        assert_eq!(parse("283.46456692913375").unwrap(), 283);
    }

    #[test]
    fn rounds_half_away_from_zero() {
        assert_eq!(parse("1.5").unwrap(), 2);
        assert_eq!(parse("-1.5").unwrap(), -2);
        assert_eq!(parse("1.499").unwrap(), 1);
        assert_eq!(parse("-1.499").unwrap(), -1);
    }

    #[test]
    fn rejects_non_decimal_and_out_of_range_values() {
        for raw in [
            "",
            ".5",
            "1.",
            "1e2",
            "NaN",
            "inf",
            "abc",
            "9223372036854775808",
            "-9223372036854775809",
        ] {
            assert!(parse(raw).is_err(), "{raw:?} must be rejected");
        }
    }
}
