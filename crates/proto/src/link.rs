//! Device linking (API contract v1.4): user codes and what the owner is shown.

pub const USER_CODE_ALPHABET: &str = "BCDFGHJKLMNPQRSTVWXZ";
pub const USER_CODE_LEN: usize = 8;
pub const START_MIN_INTERVAL_S: f64 = 60.0; // start a new link at most once a minute
pub const SLOW_DOWN_S: f64 = 5.0; // added to the poll interval after 429 slow_down
pub const CODE_LOG_S: f64 = 30.0; // how often the waiting code is logged

/// 'wdjb-mjht', 'WDJBMJHT' and 'w d j b m j h t' all give 'WDJBMJHT'. None if invalid.
pub fn normalize_user_code(code: &str) -> Option<String> {
    let letters: String = code.to_uppercase().chars().filter(|c| *c != ' ' && *c != '-').collect();
    if letters.chars().count() != USER_CODE_LEN || letters.chars().any(|c| !USER_CODE_ALPHABET.contains(c)) {
        return None;
    }
    Some(letters)
}

/// The JSON form: anything that is not a string is invalid.
pub fn normalize_user_code_value(code: &serde_json::Value) -> Option<String> {
    code.as_str().and_then(normalize_user_code)
}

/// XXXX-XXXX for display. A code the headend sent in another form is shown as it came.
pub fn format_user_code(code: &str) -> String {
    match normalize_user_code(code) {
        Some(norm) => format!("{}-{}", &norm[..4], &norm[4..]),
        None => code.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_code_normalization() {
        for given in ["wdjb-mjht", "WDJBMJHT", "w d j b m j h t", "WDJB-MJHT"] {
            assert_eq!(normalize_user_code(given).as_deref(), Some("WDJBMJHT"));
            assert_eq!(format_user_code(given), "WDJB-MJHT");
        }
    }

    #[test]
    fn invalid_user_codes() {
        for bad in ["WDJB-MJHA", "WDJB-MJH", "WDJB-MJHTB", "WDJB_MJHT", ""] {
            assert_eq!(normalize_user_code(bad), None, "{bad}");
        }
        assert_eq!(normalize_user_code_value(&serde_json::Value::Null), None);
        assert_eq!(format_user_code("abc"), "abc");
    }
}
