//! Text length limits the server checks outside its settings, and the error both the
//! server and clients give for text over a limit.

/// Longest ask option, in characters.
pub const ASK_OPTION_CHARS_MAX: usize = 60;
/// Longest evidence value (a check command, commit, PR or artifact), in characters.
pub const EVIDENCE_VALUE_CHARS_MAX: usize = 500;

/// A text field over its length limit, counted in characters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TooLong<'a> {
    pub field: &'a str,
    pub len: usize,
    pub max: usize,
}

impl<'a> TooLong<'a> {
    /// `Some` when `value` has more than `max` characters.
    pub fn check(field: &'a str, value: &str, max: usize) -> Option<Self> {
        let len = value.chars().count();
        (len > max).then_some(Self { field, len, max })
    }

    /// The `invalid` error's message.
    pub fn message(&self) -> String {
        format!(
            "{} is {} characters; the limit is {}",
            self.field, self.len, self.max
        )
    }

    /// The `invalid` error's `next` hint: shorten it, or where longer text goes.
    pub fn next(&self) -> String {
        self.next_or(longer_text_goes(self.field))
    }

    /// [`Self::next`] with `goes` saying where longer text goes instead.
    pub fn next_or(&self, goes: Option<&str>) -> String {
        let shorten = format!("shorten {} to at most {} characters", self.field, self.max);
        match goes {
            Some(goes) => format!("{shorten}, or {goes}"),
            None => shorten,
        }
    }
}

/// Where text that does not fit `field` belongs, for the fields agents fill most.
fn longer_text_goes(field: &str) -> Option<&'static str> {
    Some(match field {
        "title" => "put the detail in the body",
        "text" | "reason" | "blocked_reason" => {
            "split it across notes and put the detail in the task body or task_done evidence"
        }
        "now" | "next" | "blockers" => "put the detail in a task note or the task body",
        "question" => "put the background in the task body and name the task in the ask",
        "option" => "explain the options in the question",
        "body" | "acceptance" => "split the work into child tasks",
        "evidence value" => "link to the log or artifact instead",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn over_long_text_names_the_limit_and_where_longer_text_goes() {
        assert_eq!(TooLong::check("text", &"x".repeat(280), 280), None);
        let long = TooLong::check("text", &"é".repeat(301), 280).unwrap();
        assert_eq!(long.message(), "text is 301 characters; the limit is 280");
        assert_eq!(
            long.next(),
            "shorten text to at most 280 characters, or split it across notes and put the \
             detail in the task body or task_done evidence"
        );
        let long = TooLong::check("cwd", &"x".repeat(3), 2).unwrap();
        assert_eq!(long.next(), "shorten cwd to at most 2 characters");
        assert_eq!(
            long.next_or(Some("elsewhere")),
            "shorten cwd to at most 2 characters, or elsewhere"
        );
    }
}
