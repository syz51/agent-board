// Shared by build.rs and the crate's tests (`include!`), so the rule is tested.

/// The commit to record: the `BOARD_COMMIT` override when set, else the commit git reports
/// for this repository. Reduced to its first 12 hex digits in lowercase; anything else,
/// including an override that is not a commit, is `unknown`.
pub fn normalize_commit(override_value: Option<&str>, from_git: Option<&str>) -> String {
    let raw = match override_value.map(str::trim) {
        Some(value) if !value.is_empty() => Some(value),
        _ => from_git.map(str::trim),
    };
    match raw {
        Some(value) if value.len() >= 12 && value.chars().all(|c| c.is_ascii_hexdigit()) => {
            value[..12].to_ascii_lowercase()
        }
        _ => "unknown".to_owned(),
    }
}
