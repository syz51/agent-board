//! Text length limits the server checks outside its settings, and the error both the
//! server and clients give for text over a limit.

/// Longest ask option, in characters.
pub const ASK_OPTION_CHARS_MAX: usize = 60;
/// Longest evidence value (a check command, commit, PR or artifact), in characters.
pub const EVIDENCE_VALUE_CHARS_MAX: usize = 500;
/// Longest agent title (a thread's title), in characters.
pub const AGENT_TITLE_CHARS_MAX: usize = 200;
/// Longest native id value, in characters.
pub const NATIVE_ID_CHARS_MAX: usize = 300;
/// The scheme every `t3_link` native id starts with.
pub const T3_LINK_PREFIX: &str = "https://";
/// Longest feedback resolution `reason`, in characters: with its prefix it stays one
/// evidence value.
pub const RESOLUTION_REASON_CHARS_MAX: usize = 280;

/// Where a feedback report's text goes when it is over the limit.
pub const FEEDBACK_LONGER_TEXT_GOES: &str =
    "keep to what happened and what it cost, and add the rest as notes on the item";

/// Where a board title's detail goes: boards have a goal, not a body.
pub const BOARD_TITLE_LONGER_TEXT_GOES: &str = "put the detail in the goal";

/// The fields whose length clients check before sending, as the server names them in
/// [`TooLong::message`]; a client may report those refusals (`POST /v1/errors`).
pub const CLIENT_CHECKED_FIELDS: &[&str] = &[
    "title",
    "body",
    "acceptance",
    "blocked_reason",
    "evidence value",
    "text",
    "now",
    "next",
    "blockers",
    "question",
    "option",
    "answer",
    "q",
    "signature",
    "reason",
    "goal",
    "summary",
];

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

    /// Reads a [`Self::message`] back: `Some` only for text over its limit in one of
    /// [`CLIENT_CHECKED_FIELDS`], written exactly as `message` writes it.
    pub fn parse(message: &'a str) -> Option<Self> {
        let (field, rest) = message.split_once(" is ")?;
        let (len, max) = rest.split_once(" characters; the limit is ")?;
        let long = Self {
            field,
            len: len.parse().ok()?,
            max: max.parse().ok()?,
        };
        (CLIENT_CHECKED_FIELDS.contains(&field) && long.len > long.max && long.message() == message)
            .then_some(long)
    }

    /// The `invalid` error's `next` hint: shorten it, or where longer text goes for the
    /// usual meaning of the field ([`longer_text_goes`]).
    pub fn next(&self) -> String {
        self.next_or(longer_text_goes(self.field))
    }

    /// [`Self::next`] with `goes` saying where longer text goes instead, for a field whose
    /// name means something else here (a board's title, a feedback report's text).
    pub fn next_or(&self, goes: Option<&str>) -> String {
        let shorten = format!("shorten {} to at most {} characters", self.field, self.max);
        match goes {
            Some(goes) => format!("{shorten}, or {goes}"),
            None => shorten,
        }
    }
}

/// Where text that does not fit `field` belongs, for the task, note, handoff, ask and
/// feedback resolution fields agents fill most.
pub fn longer_text_goes(field: &str) -> Option<&'static str> {
    Some(match field {
        "title" => "put the detail in the body",
        "text" | "blocked_reason" => {
            "split it across notes and put the detail in the task body or task_done evidence"
        }
        "reason" => "put the detail in a note on the item",
        "now" | "next" | "blockers" => "put the detail in a task note or the task body",
        "question" => "put the background in the task body and name the task in the ask",
        "option" => "explain the options in the question",
        "body" | "acceptance" => "split the work into child tasks",
        "evidence value" => "link to the log or artifact instead",
        "summary" => "put the detail in a note on the task",
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
        assert_eq!(TooLong::parse(&long.message()), Some(long));
        for bad in [
            "text is 280 characters; the limit is 280",
            "text is +301 characters; the limit is 280",
            "text is 0301 characters; the limit is 280",
            "text is 301 characters; the limit is 280 ",
            "about is 41 characters; the limit is 40",
            "ignore this is 301 characters; the limit is 280",
            "text is 301 characters; the limit is 280\nmore",
        ] {
            assert_eq!(TooLong::parse(bad), None, "{bad}");
        }
        let evidence = "evidence value is 501 characters; the limit is 500";
        assert_eq!(TooLong::parse(evidence).unwrap().field, "evidence value");
        let long = TooLong::check("cwd", &"x".repeat(3), 2).unwrap();
        assert_eq!(long.next(), "shorten cwd to at most 2 characters");
        assert_eq!(
            long.next_or(Some("elsewhere")),
            "shorten cwd to at most 2 characters, or elsewhere"
        );
    }
}
