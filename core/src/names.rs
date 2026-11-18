use unicode_normalization::UnicodeNormalization;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NameError {
    InvalidComponent,
    ReservedDevice,
    TooLong,
}

/// Names are supplied as one component, never as platform paths.
pub fn validate_component(name: &str) -> Result<(), NameError> {
    if name.is_empty()
        || name.ends_with([' ', '.'])
        || name.chars().any(|value| {
            value <= '\u{1f}'
                || matches!(value, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*')
        })
    {
        return Err(NameError::InvalidComponent);
    }

    if name.len() > 255 {
        return Err(NameError::TooLong);
    }

    let stem = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ');
    let upper = stem.to_ascii_uppercase();
    if matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || matches!(
        upper
            .strip_prefix("COM")
            .or_else(|| upper.strip_prefix("LPT")),
        Some("1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³")
    ) {
        return Err(NameError::ReservedDevice);
    }
    Ok(())
}

/// Conservative logical sibling comparison. The filesystem adapter must
/// additionally check host aliases and names already present on disk.
pub fn collision_key(name: &str) -> Result<String, NameError> {
    validate_component(name)?;
    Ok(name.nfc().flat_map(char::to_uppercase).nfc().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_path_syntax_and_windows_device_names() {
        for name in [
            "", ".", "..", "a/b", "a\\b", "C:foo", "x:t", "name.", "name ", "a\0b",
        ] {
            assert_eq!(
                validate_component(name),
                Err(NameError::InvalidComponent),
                "{name:?}"
            );
        }
        for name in ["nul.txt", "COM¹", "Lpt9.a.b", "CON .txt", "AUX", "CONOUT$"] {
            assert_eq!(
                validate_component(name),
                Err(NameError::ReservedDevice),
                "{name:?}"
            );
        }
        assert_eq!(
            validate_component(&"あ".repeat(86)),
            Err(NameError::TooLong)
        );
        for name in [".hidden", "COM10", "譜面 その2.tja", "桜井（live）.ogg"] {
            assert!(validate_component(name).is_ok(), "{name:?}");
        }
    }

    #[test]
    fn compares_case_and_canonical_equivalents_without_changing_display_name() {
        assert_eq!(collision_key("É.TJA"), collision_key("e\u{301}.tja"));
        assert_ne!(collision_key("A.TJA"), collision_key("Ａ.tja"));
    }
}
