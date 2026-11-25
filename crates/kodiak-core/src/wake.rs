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

    /// Whether `word` is one of the configured spellings, ignoring case.
    #[must_use]
    pub fn is_form(&self, word: &str) -> bool {
        self.forms.contains(&word.to_lowercase())
    }

    /// Removes activation words (any configured spelling) that start `text`,
    /// so a repeated activation word ("Kodiak Kodiak routine …") is not part
    /// of the command. Returns the text after the last one removed, or `text`
    /// unchanged when it does not start with one.
    #[must_use]
    pub fn strip_leading<'a>(&self, text: &'a str) -> &'a str {
        let mut rest = text;
        loop {
            let Some(start) = rest.find(char::is_alphanumeric) else {
                return rest;
            };
            let word = &rest[start..];
            let end = word
                .find(|c: char| !c.is_alphanumeric())
                .unwrap_or(word.len());
            if !self.is_form(&word[..end]) {
                return rest;
            }
            rest = &word[end..];
        }
    }

    /// Returns the text after the activation word when it is the first word
    /// of `text`, or `None` otherwise: a message that does not start with
    /// the activation word is chat, even if the word appears later ("I saw a
    /// kodiak bear"). The returned command may be empty.
    #[must_use]
    pub fn command<'a>(&self, text: &'a str) -> Option<&'a str> {
        let start = text.find(char::is_alphanumeric)?;
        let word = &text[start..];
        let end = word
            .find(|c: char| !c.is_alphanumeric())
            .unwrap_or(word.len());
        self.is_form(&word[..end]).then(|| &word[end..])
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
        assert_eq!(wake.command("KODIAK, list users"), Some(", list users"));
        assert_eq!(wake.command("hey Kodiak, list users"), None);
        assert_eq!(wake.command("I saw a kodiak bear yesterday"), None);
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

    #[test]
    fn leading_repeats_of_any_form_are_stripped() {
        let wake = WakeWord::default();
        assert_eq!(
            wake.strip_leading(" Kodiak Routine Heading Home"),
            " Routine Heading Home"
        );
        assert_eq!(
            wake.strip_leading(", kodiac, KODIAK. list users"),
            ". list users"
        );
        for unchanged in [
            " urgent need food.",
            "list kodiak users",
            "kodiak2 list",
            "",
        ] {
            assert_eq!(wake.strip_leading(unchanged), unchanged, "{unchanged}");
        }
        assert_eq!(wake.strip_leading("kodiak"), "");
        let falcon = WakeWord::with_forms(["falcon"]);
        assert_eq!(falcon.strip_leading(" Falcon list users"), " list users");
        assert_eq!(
            falcon.strip_leading(" kodiak list users"),
            " kodiak list users"
        );
    }
}
