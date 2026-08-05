use crate::field::context::{Date, Time};

/// Format a date using an OOXML date/time format string (§17.16.4.1).
///
/// Supports common patterns:
/// - `d`, `dd` — day of month (1 vs 01)
/// - `M`, `MM`, `MMM`, `MMMM` — month (3, 03, Mar, March)
/// - `yy`, `yyyy` — year (26, 2026)
/// - `H`, `HH` — 24-hour (7, 07)
/// - `h`, `hh` — 12-hour (7, 07)
/// - `m`, `mm` — minute (5, 05)
/// - `s`, `ss` — second (3, 03)
/// - `AM/PM`, `am/pm`
///
/// Literal text can be included in single quotes: `'at' h:mm`.
pub fn format_date(date: &Date, pattern: &str) -> String {
    let mut result = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        // Literal text in single quotes
        if chars[i] == '\'' {
            i += 1;
            while i < len && chars[i] != '\'' {
                result.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1; // skip closing quote
            }
            continue;
        }

        // Year
        if chars[i] == 'y' {
            let count = count_run(&chars, i, 'y');
            if count >= 4 {
                result.push_str(&format!("{:04}", date.year));
            } else {
                result.push_str(&format!("{:02}", date.year % 100));
            }
            i += count;
            continue;
        }

        // Month (uppercase M to distinguish from minute)
        if chars[i] == 'M' {
            let count = count_run(&chars, i, 'M');
            match count {
                1 => result.push_str(&date.month.to_string()),
                2 => result.push_str(&format!("{:02}", date.month)),
                3 => result.push_str(short_month_name(date.month)),
                _ => result.push_str(long_month_name(date.month)),
            }
            i += count;
            continue;
        }

        // Day
        if chars[i] == 'd' {
            let count = count_run(&chars, i, 'd');
            if count >= 2 {
                result.push_str(&format!("{:02}", date.day));
            } else {
                result.push_str(&date.day.to_string());
            }
            i += count;
            continue;
        }

        // Passthrough for other characters (separators like /, -, space)
        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Format a time using an OOXML date/time format string.
pub fn format_time(time: &Time, pattern: &str) -> String {
    let has_ampm = pattern.contains("AM/PM") || pattern.contains("am/pm");

    let (display_hour, ampm) = if has_ampm {
        let (h, period) = to_12hour(time.hour);
        (h, Some(period))
    } else {
        (time.hour, None)
    };

    let mut result = String::new();
    let chars: Vec<char> = pattern.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        if chars[i] == '\'' {
            i += 1;
            while i < len && chars[i] != '\'' {
                result.push(chars[i]);
                i += 1;
            }
            if i < len {
                i += 1;
            }
            continue;
        }

        // AM/PM — compare on the char slice, not byte offsets: `i` is a char
        // index, so `pattern[i..i+5]` would misalign (and can panic) after any
        // multi-byte character earlier in the pattern.
        if chars[i..].starts_with(&['A', 'M', '/', 'P', 'M']) {
            result.push_str(ampm.unwrap_or("AM"));
            i += 5;
            continue;
        }
        if chars[i..].starts_with(&['a', 'm', '/', 'p', 'm']) {
            result.push_str(&ampm.unwrap_or("am").to_ascii_lowercase());
            i += 5;
            continue;
        }

        // 24-hour (H)
        if chars[i] == 'H' {
            let count = count_run(&chars, i, 'H');
            if count >= 2 {
                result.push_str(&format!("{:02}", time.hour));
            } else {
                result.push_str(&time.hour.to_string());
            }
            i += count;
            continue;
        }

        // 12-hour (h)
        if chars[i] == 'h' {
            let count = count_run(&chars, i, 'h');
            if count >= 2 {
                result.push_str(&format!("{:02}", display_hour));
            } else {
                result.push_str(&display_hour.to_string());
            }
            i += count;
            continue;
        }

        // Minute
        if chars[i] == 'm' {
            let count = count_run(&chars, i, 'm');
            if count >= 2 {
                result.push_str(&format!("{:02}", time.minute));
            } else {
                result.push_str(&time.minute.to_string());
            }
            i += count;
            continue;
        }

        // Second
        if chars[i] == 's' {
            let count = count_run(&chars, i, 's');
            if count >= 2 {
                result.push_str(&format!("{:02}", time.second));
            } else {
                result.push_str(&time.second.to_string());
            }
            i += count;
            continue;
        }

        result.push(chars[i]);
        i += 1;
    }

    result
}

/// Format a number using an OOXML numeric format string (§17.16.4.1).
///
/// Basic support: `0` = digit (pad with zero), `#` = digit (no pad).
pub fn format_number(value: f64, pattern: &str) -> String {
    // Find decimal point in pattern
    let parts: Vec<&str> = pattern.split('.').collect();

    if parts.len() == 2 {
        let decimal_places = parts[1].len();
        format!("{:.prec$}", value, prec = decimal_places)
    } else if pattern.contains('0') || pattern.contains('#') {
        // Integer format: round (like the decimal branch's `{:.prec$}`) rather
        // than truncate toward zero. `.round()` first also avoids a "-0" result
        // from `-0.4 as i64`.
        format!("{}", value.round() as i64)
    } else {
        value.to_string()
    }
}

/// Apply a general format switch (`\* FORMAT`) to a string value.
///
/// For `ROMAN`/`ALPHABETIC` the *case of the switch keyword* selects the output
/// case (`\* roman` → `iv`, `\* ROMAN` → `IV`), per §17.16.4.1.
pub fn apply_general_format(value: &str, format: &str) -> String {
    // Lowercase output when the keyword itself is written lowercase.
    let lowercase = format
        .chars()
        .find(|c| c.is_ascii_alphabetic())
        .is_some_and(|c| c.is_ascii_lowercase());
    match format.to_ascii_uppercase().as_str() {
        "UPPER" => value.to_ascii_uppercase(),
        "LOWER" => value.to_ascii_lowercase(),
        // §17.16.4.1: FirstCap capitalizes the first letter of the *first* word;
        // Caps capitalizes the first letter of *every* word (title case).
        "FIRSTCAP" => first_cap(value),
        "CAPS" => title_case(value),
        "MERGEFORMAT" => value.to_string(), // preserve existing formatting, no-op for text
        "ALPHABETIC" => match value.parse::<u32>() {
            Ok(n) => to_alphabetic(n, lowercase),
            Err(_) => value.to_string(),
        },
        "ROMAN" => match value.parse::<u32>() {
            Ok(n) => to_roman(n, lowercase),
            Err(_) => value.to_string(),
        },
        _ => value.to_string(),
    }
}

/// Capitalize the first letter of the first word only (`\* FirstCap`).
fn first_cap(value: &str) -> String {
    let mut chars = value.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut s = c.to_uppercase().to_string();
            s.extend(chars);
            s
        }
    }
}

/// Capitalize the first letter of every word (`\* Caps` — title case). A word is
/// a maximal run of alphabetic characters; the rest of each word is left as-is.
fn title_case(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut at_word_start = true;
    for c in value.chars() {
        if c.is_alphabetic() {
            if at_word_start {
                out.extend(c.to_uppercase());
            } else {
                out.push(c);
            }
            at_word_start = false;
        } else {
            out.push(c);
            at_word_start = true;
        }
    }
    out
}

fn count_run(chars: &[char], start: usize, ch: char) -> usize {
    chars[start..].iter().take_while(|&&c| c == ch).count()
}

fn to_12hour(hour: u32) -> (u32, &'static str) {
    match hour {
        0 => (12, "AM"),
        1..=11 => (hour, "AM"),
        12 => (12, "PM"),
        _ => (hour - 12, "PM"),
    }
}

fn short_month_name(month: u32) -> &'static str {
    match month {
        1 => "Jan",
        2 => "Feb",
        3 => "Mar",
        4 => "Apr",
        5 => "May",
        6 => "Jun",
        7 => "Jul",
        8 => "Aug",
        9 => "Sep",
        10 => "Oct",
        11 => "Nov",
        12 => "Dec",
        _ => "???",
    }
}

fn long_month_name(month: u32) -> &'static str {
    match month {
        1 => "January",
        2 => "February",
        3 => "March",
        4 => "April",
        5 => "May",
        6 => "June",
        7 => "July",
        8 => "August",
        9 => "September",
        10 => "October",
        11 => "November",
        12 => "December",
        _ => "???",
    }
}

fn to_roman(mut n: u32, lowercase: bool) -> String {
    const TABLE: &[(u32, &str)] = &[
        (1000, "M"),
        (900, "CM"),
        (500, "D"),
        (400, "CD"),
        (100, "C"),
        (90, "XC"),
        (50, "L"),
        (40, "XL"),
        (10, "X"),
        (9, "IX"),
        (5, "V"),
        (4, "IV"),
        (1, "I"),
    ];
    let mut result = String::new();
    for &(value, numeral) in TABLE {
        while n >= value {
            result.push_str(numeral);
            n -= value;
        }
    }
    if lowercase {
        result.to_ascii_lowercase()
    } else {
        result
    }
}

fn to_alphabetic(n: u32, lowercase: bool) -> String {
    if n == 0 {
        return String::new();
    }
    let base = if lowercase { b'a' } else { b'A' };
    let mut result = Vec::new();
    let mut val = n - 1;
    loop {
        result.push(base + (val % 26) as u8);
        if val < 26 {
            break;
        }
        val = val / 26 - 1;
    }
    result.reverse();
    String::from_utf8(result).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_date_basic() {
        let date = Date {
            year: 2026,
            month: 3,
            day: 5,
        };
        assert_eq!(format_date(&date, "dd/MM/yyyy"), "05/03/2026");
        assert_eq!(format_date(&date, "d/M/yy"), "5/3/26");
        assert_eq!(format_date(&date, "MMMM d, yyyy"), "March 5, 2026");
    }

    #[test]
    fn format_time_24h() {
        let time = Time {
            hour: 14,
            minute: 5,
            second: 9,
        };
        assert_eq!(format_time(&time, "HH:mm:ss"), "14:05:09");
        assert_eq!(format_time(&time, "H:m"), "14:5");
    }

    #[test]
    fn format_time_12h() {
        let time = Time {
            hour: 14,
            minute: 30,
            second: 0,
        };
        assert_eq!(format_time(&time, "h:mm AM/PM"), "2:30 PM");
    }

    #[test]
    fn format_number_decimal() {
        assert_eq!(format_number(1.2345, "0.00"), "1.23");
        assert_eq!(format_number(42.0, "0.000"), "42.000");
    }

    #[test]
    fn general_format_upper() {
        assert_eq!(apply_general_format("hello", "Upper"), "HELLO");
        assert_eq!(apply_general_format("hello", "Lower"), "hello");
        assert_eq!(
            apply_general_format("hello world", "FirstCap"),
            "Hello world"
        );
    }

    #[test]
    fn roman_numerals() {
        assert_eq!(to_roman(1, false), "I");
        assert_eq!(to_roman(4, false), "IV");
        assert_eq!(to_roman(14, false), "XIV");
        assert_eq!(to_roman(2026, false), "MMXXVI");
    }

    #[test]
    fn alphabetic_numbering() {
        assert_eq!(to_alphabetic(1, false), "A");
        assert_eq!(to_alphabetic(26, false), "Z");
        assert_eq!(to_alphabetic(27, false), "AA");
    }

    #[test]
    fn general_format_roman_case_follows_switch() {
        // §17.16.4.1: `\* roman` → lowercase, `\* ROMAN` → uppercase.
        assert_eq!(apply_general_format("4", "roman"), "iv");
        assert_eq!(apply_general_format("4", "ROMAN"), "IV");
        assert_eq!(apply_general_format("4", "Roman"), "IV");
        // Non-numeric input is passed through unchanged.
        assert_eq!(apply_general_format("abc", "roman"), "abc");
    }

    #[test]
    fn general_format_alphabetic_case_follows_switch() {
        assert_eq!(apply_general_format("1", "alphabetic"), "a");
        assert_eq!(apply_general_format("1", "ALPHABETIC"), "A");
        assert_eq!(apply_general_format("28", "alphabetic"), "ab");
    }

    #[test]
    fn general_format_caps_is_title_case_firstcap_is_not() {
        // Caps capitalizes every word; FirstCap only the first.
        assert_eq!(apply_general_format("hello world", "Caps"), "Hello World");
        assert_eq!(
            apply_general_format("hello world", "FirstCap"),
            "Hello world"
        );
        // Existing capitals inside a word are left alone.
        assert_eq!(
            apply_general_format("the fbi report", "Caps"),
            "The Fbi Report"
        );
    }

    #[test]
    fn number_integer_format_rounds_not_truncates() {
        assert_eq!(format_number(2.7, "0"), "3");
        assert_eq!(format_number(2.4, "0"), "2");
        assert_eq!(format_number(-2.7, "0"), "-3");
        // `.round()` first avoids a "-0" result.
        assert_eq!(format_number(-0.4, "0"), "0");
    }

    #[test]
    fn time_ampm_after_multibyte_literal_does_not_panic() {
        let time = Time {
            hour: 14,
            minute: 30,
            second: 0,
        };
        // A multi-byte char in a literal shifts byte vs char offsets; the AM/PM
        // match must still align (previously this byte-sliced and could panic).
        assert_eq!(format_time(&time, "'é' h AM/PM"), "é 2 PM");
    }
}
