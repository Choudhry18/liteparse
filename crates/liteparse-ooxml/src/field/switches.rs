use crate::field::ast::CommonSwitches;
use crate::field::error::FieldParseError;
use crate::field::parse::Token;

/// Consume common formatting switches (`\*`, `\#`, `\@`) from a token stream.
///
/// Returns the consumed switches and any tokens that were not common switches
/// (i.e., field-specific switches or arguments).
pub(crate) fn extract_common_switches(
    tokens: &[Token],
) -> Result<(CommonSwitches, Vec<Token>), FieldParseError> {
    let mut switches = CommonSwitches::default();
    let mut remaining = Vec::new();
    let mut i = 0;

    while i < tokens.len() {
        match &tokens[i] {
            Token::Switch(ch, pos) if *ch == '*' => {
                i += 1;
                let value = take_switch_value(tokens, &mut i, "\\*", *pos)?;
                switches.format = Some(value);
            }
            Token::Switch(ch, pos) if *ch == '#' => {
                i += 1;
                let value = take_switch_value(tokens, &mut i, "\\#", *pos)?;
                switches.numeric_format = Some(value);
            }
            Token::Switch(ch, pos) if *ch == '@' => {
                i += 1;
                let value = take_switch_value(tokens, &mut i, "\\@", *pos)?;
                switches.date_format = Some(value);
            }
            _ => {
                remaining.push(tokens[i].clone());
                i += 1;
            }
        }
    }

    Ok((switches, remaining))
}

/// Take a switch value — either the next quoted string or word token.
fn take_switch_value(
    tokens: &[Token],
    i: &mut usize,
    switch_name: &str,
    _pos: usize,
) -> Result<String, FieldParseError> {
    if *i < tokens.len() {
        match &tokens[*i] {
            Token::Quoted(s, _) | Token::Word(s, _) => {
                let val = s.clone();
                *i += 1;
                Ok(val)
            }
            _ => Err(FieldParseError::MissingSwitchValue {
                switch: switch_name.to_string(),
            }),
        }
    } else {
        Err(FieldParseError::MissingSwitchValue {
            switch: switch_name.to_string(),
        })
    }
}

/// Return whether a flag switch (`\<ch>`, no value) is present anywhere in the
/// token stream. A pure predicate — it does not consume or reorder tokens.
pub(crate) fn has_flag(tokens: &[Token], ch: char) -> bool {
    tokens
        .iter()
        .any(|t| matches!(t, Token::Switch(c, _) if *c == ch))
}

/// Find a switch with the given character and consume its value argument.
/// Returns `Some(value)` if found, `None` if not present.
pub(crate) fn take_switch_with_value(
    tokens: &mut Vec<Token>,
    ch: char,
) -> Result<Option<String>, FieldParseError> {
    let pos = tokens
        .iter()
        .position(|t| matches!(t, Token::Switch(c, _) if *c == ch));

    let Some(idx) = pos else {
        return Ok(None);
    };

    let switch_pos = match &tokens[idx] {
        Token::Switch(_, p) => *p,
        _ => unreachable!(),
    };

    // Remove the switch token
    tokens.remove(idx);

    // The value should now be at `idx` (shifted down)
    if idx < tokens.len() {
        match &tokens[idx] {
            Token::Quoted(s, _) | Token::Word(s, _) => {
                let val = s.clone();
                tokens.remove(idx);
                Ok(Some(val))
            }
            _ => Err(FieldParseError::MissingSwitchValue {
                switch: format!("\\{ch}"),
            }),
        }
    } else {
        Err(FieldParseError::MissingSwitchValue {
            switch: format!("\\{ch} at position {switch_pos}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_captures_all_three_common_switches() {
        // \* MERGEFORMAT  \# "0.00"  \@ "dd/MM/yyyy"
        let tokens = vec![
            Token::Switch('*', 0),
            Token::Word("MERGEFORMAT".into(), 2),
            Token::Switch('#', 14),
            Token::Quoted("0.00".into(), 16),
            Token::Switch('@', 22),
            Token::Quoted("dd/MM/yyyy".into(), 24),
        ];
        let (sw, remaining) = extract_common_switches(&tokens).unwrap();
        assert_eq!(sw.format.as_deref(), Some("MERGEFORMAT"));
        assert_eq!(sw.numeric_format.as_deref(), Some("0.00"));
        assert_eq!(sw.date_format.as_deref(), Some("dd/MM/yyyy"));
        assert!(remaining.is_empty());
    }

    #[test]
    fn extract_leaves_field_specific_tokens() {
        // \o "1-3" is field-specific and must pass through untouched.
        let tokens = vec![
            Token::Switch('o', 0),
            Token::Quoted("1-3".into(), 2),
            Token::Switch('*', 8),
            Token::Word("Upper".into(), 10),
        ];
        let (sw, remaining) = extract_common_switches(&tokens).unwrap();
        assert_eq!(sw.format.as_deref(), Some("Upper"));
        assert_eq!(remaining.len(), 2);
        assert!(matches!(&remaining[0], Token::Switch('o', _)));
        assert!(matches!(&remaining[1], Token::Quoted(s, _) if s == "1-3"));
    }

    #[test]
    fn extract_missing_value_is_error() {
        let tokens = vec![Token::Switch('*', 0)];
        assert!(matches!(
            extract_common_switches(&tokens),
            Err(FieldParseError::MissingSwitchValue { .. })
        ));
    }

    #[test]
    fn has_flag_is_a_pure_predicate() {
        let tokens = vec![Token::Switch('h', 0), Token::Word("x".into(), 2)];
        assert!(has_flag(&tokens, 'h'));
        assert!(!has_flag(&tokens, 'z'));
        // Does not consume: the stream is unchanged (it takes `&[Token]`).
        assert_eq!(tokens.len(), 2);
    }

    #[test]
    fn take_switch_with_value_found_removes_switch_and_value() {
        let mut tokens = vec![
            Token::Switch('l', 0),
            Token::Quoted("bookmark".into(), 2),
            Token::Word("keep".into(), 12),
        ];
        let v = take_switch_with_value(&mut tokens, 'l').unwrap();
        assert_eq!(v.as_deref(), Some("bookmark"));
        // Only the trailing unrelated token remains.
        assert_eq!(tokens.len(), 1);
        assert!(matches!(&tokens[0], Token::Word(s, _) if s == "keep"));
    }

    #[test]
    fn take_switch_with_value_absent_returns_none() {
        let mut tokens = vec![Token::Word("x".into(), 0)];
        assert_eq!(take_switch_with_value(&mut tokens, 'l').unwrap(), None);
        assert_eq!(tokens.len(), 1); // untouched
    }

    #[test]
    fn take_switch_with_value_at_end_is_error() {
        let mut tokens = vec![Token::Switch('l', 0)];
        assert!(matches!(
            take_switch_with_value(&mut tokens, 'l'),
            Err(FieldParseError::MissingSwitchValue { .. })
        ));
    }

    #[test]
    fn take_switch_with_value_wrong_value_type_is_error() {
        // \l immediately followed by another switch — no usable value.
        let mut tokens = vec![Token::Switch('l', 0), Token::Switch('m', 2)];
        assert!(matches!(
            take_switch_with_value(&mut tokens, 'l'),
            Err(FieldParseError::MissingSwitchValue { .. })
        ));
    }
}
