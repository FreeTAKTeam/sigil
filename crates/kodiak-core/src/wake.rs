//! Activation-word detection on transcribed text.

/// Activation word used when none is configured.
pub const DEFAULT_WAKE_WORD: &str = "kodiak";

/// Default spellings: the activation word plus spellings that the bundled
/// recognizers were observed to produce for it.
pub const DEFAULT_WAKE_FORMS: &[&str] =
    &[DEFAULT_WAKE_WORD, "kodiac", "codiak", "codiac", "codeiac"];

/// Detects the activation word as a distinct, case-insensitive word.
///
/// A word is a maximal run of alphanumeric characters, so `Kodiak.`,
/// `kodiak,` and `KODIAK` match while `Kodiaks` and `Kodiak2` do not.
/// Speech recognizers sometimes spell a name differently; extra spellings can
/// be added with [`WakeWord::with_forms`].
#[derive(Clone, Debug)]
pub struct WakeWord {
    forms: Vec<String>,
}

impl Default for WakeWord {
    fn default() -> Self {
        Self {
            forms: DEFAULT_WAKE_FORMS
                .iter()
                .map(|form| (*form).to_owned())
                .collect(),
        }
    }
}

impl WakeWord {
    /// Uses the given spellings. Empty or non-alphanumeric forms are ignored;
    /// if none remain, the default word is used.
    #[must_use]
    pub fn with_forms<I, S>(forms: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let forms: Vec<String> = forms
            .into_iter()
            .map(|form| form.as_ref().trim().to_lowercase())
            .filter(|form| !form.is_empty() && form.chars().all(char::is_alphanumeric))
            .collect();
        if forms.is_empty() {
            Self::default()
        } else {
            Self { forms }
        }
    }

    /// The primary activation word (the first configured spelling).
    #[must_use]
    pub fn word(&self) -> &str {
        &self.forms[0]
    }

    /// Returns the text that follows the first activation word, or `None` when
    /// the activation word is absent. The returned command may be empty.
    #[must_use]
    pub fn command<'a>(&self, text: &'a str) -> Option<&'a str> {
        let mut start = None;
        for (index, character) in text
            .char_indices()
            .chain(std::iter::once((text.len(), ' ')))
        {
            match (start, character.is_alphanumeric()) {
                (None, true) => start = Some(index),
                (Some(from), false) => {
                    let word = &text[from..index];
                    if self.forms.iter().any(|form| word.to_lowercase() == *form) {
                        return Some(&text[index..]);
                    }
                    start = None;
                }
                _ => {}
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_distinct_word_activates() {
        let wake = WakeWord::default();
        assert_eq!(
            wake.command("Kodiak. Urgent. Need Food."),
            Some(". Urgent. Need Food.")
        );
        assert_eq!(wake.command("hey KODIAK, list users"), Some(", list users"));
        assert_eq!(wake.command("kodiak"), Some(""));
        assert_eq!(wake.command("Codeiac. Safety."), Some(". Safety."));
        for chat in [
            "The Kodiaks are here",
            "kodiak2 list users",
            "We need food",
            "",
        ] {
            assert_eq!(wake.command(chat), None, "{chat}");
        }
        let custom = WakeWord::with_forms(["Kodiak", "Kodiac", " "]);
        assert_eq!(custom.command("Kodiac list users"), Some(" list users"));
    }
}
