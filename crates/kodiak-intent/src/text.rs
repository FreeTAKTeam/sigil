//! Tokenization and spoken-number parsing shared by the vocabularies.

/// One word of a command, lower-cased for matching with its original spelling.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub word: String,
    pub original: String,
}

/// Splits text into words.
///
/// Letters and digits form words; everything else separates them, except that
/// `:` and `.` between digits are kept (`3:30`, `1.5`), a thousands comma is
/// dropped (`1,000`), a `-` that starts a number is kept (`-73.5`), and a
/// number glued to letters is split (`200m`).
///
/// A comma counts as a thousands separator only when exactly three digits
/// follow it and no decimal point precedes it, so `45.5,-73.5` and
/// `45.5017,73.5673` stay two numbers.
#[must_use]
pub fn tokenize(text: &str) -> Vec<Token> {
    let characters: Vec<char> = text.chars().collect();
    let mut tokens = Vec::new();
    let mut current = String::new();
    let digit = |index: usize| characters.get(index).is_some_and(char::is_ascii_digit);
    let thousands = |index: usize, current: &str| {
        let run = characters[index + 1..]
            .iter()
            .take_while(|c| c.is_ascii_digit())
            .count();
        run == 3
            && !current.contains('.')
            && characters
                .get(index + 4)
                .is_none_or(|c| *c != '.' && !c.is_ascii_digit())
    };
    let flush = |current: &mut String, tokens: &mut Vec<Token>| {
        if !current.is_empty() {
            tokens.push(Token {
                word: current.to_lowercase(),
                original: std::mem::take(current),
            });
        }
    };
    for (index, &character) in characters.iter().enumerate() {
        let between_digits = index > 0 && digit(index - 1) && digit(index + 1);
        if character.is_alphanumeric() {
            let boundary = current.chars().last().is_some_and(|last| {
                last.is_alphanumeric() && last.is_ascii_digit() != character.is_ascii_digit()
            });
            // Keep alphanumeric words like "b52" whole; split "200m" only.
            if boundary
                && current
                    .chars()
                    .all(|c| c.is_ascii_digit() || c == '.' || c == ':' || c == '-')
            {
                flush(&mut current, &mut tokens);
            }
            current.push(character);
        } else if between_digits && (character == ':' || character == '.') {
            current.push(character);
        } else if character == '-'
            && digit(index + 1)
            && (index == 0 || !characters[index - 1].is_alphanumeric())
        {
            flush(&mut current, &mut tokens);
            current.push(character);
        } else if !(between_digits && character == ',' && thousands(index, &current)) {
            flush(&mut current, &mut tokens);
        }
    }
    flush(&mut current, &mut tokens);
    tokens
}

/// Number words and clock words, which corrections must never change.
pub const NUMBER_WORDS: &[&str] = &[
    "zero",
    "oh",
    "one",
    "two",
    "three",
    "four",
    "five",
    "six",
    "seven",
    "eight",
    "nine",
    "niner",
    "ten",
    "eleven",
    "twelve",
    "thirteen",
    "fourteen",
    "fifteen",
    "sixteen",
    "seventeen",
    "eighteen",
    "nineteen",
    "twenty",
    "thirty",
    "forty",
    "fifty",
    "sixty",
    "seventy",
    "eighty",
    "ninety",
    "hundred",
    "thousand",
    "point",
    "and",
    "half",
    "past",
    "o",
    "clock",
    "oclock",
    "degree",
    "degrees",
    "minute",
    "minutes",
    "min",
    "mins",
    "people",
    "persons",
    "person",
    "pax",
];

fn unit(word: &str) -> Option<u32> {
    Some(match word {
        "zero" => 0,
        "one" => 1,
        "two" => 2,
        "three" => 3,
        "four" => 4,
        "five" => 5,
        "six" => 6,
        "seven" => 7,
        "eight" => 8,
        "nine" => 9,
        _ => return None,
    })
}

fn teen(word: &str) -> Option<u32> {
    Some(match word {
        "ten" => 10,
        "eleven" => 11,
        "twelve" => 12,
        "thirteen" => 13,
        "fourteen" => 14,
        "fifteen" => 15,
        "sixteen" => 16,
        "seventeen" => 17,
        "eighteen" => 18,
        "nineteen" => 19,
        _ => return None,
    })
}

fn tens(word: &str) -> Option<u32> {
    Some(match word {
        "twenty" => 20,
        "thirty" => 30,
        "forty" => 40,
        "fifty" => 50,
        "sixty" => 60,
        "seventy" => 70,
        "eighty" => 80,
        "ninety" => 90,
        _ => return None,
    })
}

/// A single digit spoken or written, used for digit-by-digit bearings.
#[must_use]
pub fn single_digit(word: &str) -> Option<u32> {
    if word.len() == 1 {
        return word.chars().next()?.to_digit(10);
    }
    match word {
        "oh" | "o" => Some(0),
        "niner" => Some(9),
        _ => unit(word),
    }
}

/// Whether a word could start a spoken or written number.
#[must_use]
pub fn starts_number(word: &str) -> bool {
    decimal(word).is_some() || unit(word).is_some() || teen(word).is_some() || tens(word).is_some()
}

fn decimal(word: &str) -> Option<f64> {
    (word.chars().next()?.is_ascii_digit() && !word.contains(':'))
        .then(|| word.parse::<f64>().ok())
        .flatten()
        .filter(|value| value.is_finite())
}

// What the previous word was, to keep only well-formed sequences.
#[derive(PartialEq)]
enum Last {
    Start,
    Unit,
    Tens,
    Hundred,
    Thousand,
}

/// Parses the number that starts at `words[0]`, returning its value and the
/// number of words used. Accepts digits (`200`, `1.5`), number words
/// (`two hundred`, `one thousand five hundred`, `twenty five`,
/// `one point five`) and `a hundred` / `a thousand`.
///
/// A unit or teen may only be followed by `hundred`, `thousand` or `point`,
/// so `three thirty` reads as `3` followed by another number.
#[must_use]
pub fn number(words: &[&str]) -> Option<(f64, usize)> {
    let first = *words.first()?;
    if let Some(value) = decimal(first) {
        return Some(match words.get(1) {
            Some(&"hundred") => (value * 100.0, 2),
            Some(&"thousand") => (value * 1000.0, 2),
            _ => (value, 1),
        });
    }
    let mut index = 0;
    if first == "a" && matches!(words.get(1), Some(&("hundred" | "thousand"))) {
        index = 1;
    } else if !(unit(first).is_some() || teen(first).is_some() || tens(first).is_some()) {
        return None;
    }
    let mut total = 0_u32;
    let mut current = u32::from(first == "a");
    let mut last = Last::Start;
    while let Some(&word) = words.get(index) {
        if let Some(value) = unit(word) {
            if !matches!(
                last,
                Last::Start | Last::Tens | Last::Hundred | Last::Thousand
            ) {
                break;
            }
            current = current.checked_add(value)?;
            last = Last::Unit;
        } else if let Some(value) = teen(word) {
            if !matches!(last, Last::Start | Last::Hundred | Last::Thousand) {
                break;
            }
            current = current.checked_add(value)?;
            last = Last::Unit;
        } else if let Some(value) = tens(word) {
            if !matches!(last, Last::Start | Last::Hundred | Last::Thousand) {
                break;
            }
            current = current.checked_add(value)?;
            last = Last::Tens;
        } else if word == "hundred" && matches!(last, Last::Unit | Last::Tens) {
            current = current.checked_mul(100)?;
            last = Last::Hundred;
        } else if word == "hundred" && last == Last::Start && current == 1 {
            current = 100;
            last = Last::Hundred;
        } else if word == "thousand" && last != Last::Thousand && (current > 0) {
            total = current
                .checked_mul(1000)
                .and_then(|v| total.checked_add(v))?;
            current = 0;
            last = Last::Thousand;
        } else if word == "and" && last == Last::Hundred {
            // "two hundred and fifty"
        } else if word == "point" && matches!(last, Last::Unit | Last::Tens) {
            let mut fraction = String::new();
            let mut used = 0;
            while let Some(digit) = words.get(index + 1 + used).and_then(|w| single_digit(w)) {
                fraction.push(char::from_digit(digit, 10)?);
                used += 1;
            }
            if fraction.is_empty() {
                break;
            }
            let value = format!("{}.{fraction}", total.checked_add(current)?)
                .parse()
                .ok()?;
            return Some((value, index + 1 + used));
        } else {
            break;
        }
        index += 1;
    }
    // A trailing "and" is not part of the number.
    while index > 0 && words[index - 1] == "and" {
        index -= 1;
    }
    let value = total.checked_add(current)?;
    (last != Last::Start).then_some((f64::from(value), index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(text: &str) -> Vec<String> {
        tokenize(text).into_iter().map(|token| token.word).collect()
    }

    fn parse(text: &str) -> Option<(f64, usize)> {
        let words = words(text);
        let refs: Vec<&str> = words.iter().map(String::as_str).collect();
        number(&refs)
    }

    #[test]
    fn tokenizer_keeps_numbers_and_splits_units() {
        assert_eq!(
            words("Kodiak, 3:30; 200m! 1.5km 1,000 o'clock."),
            [
                "kodiak", "3:30", "200", "m", "1.5", "km", "1000", "o", "clock"
            ]
        );
        assert_eq!(words("E.A.M. status"), ["e", "a", "m", "status"]);
        assert_eq!(words("45.5017,-73.5673"), ["45.5017", "-73.5673"]);
        assert_eq!(words("45.5017,73.5673"), ["45.5017", "73.5673"]);
        assert_eq!(words("12,500 m 1,000,000"), ["12500", "m", "1000000"]);
        assert_eq!(words("1-2-3, -5"), ["1", "2", "3", "-5"]);
        assert_eq!(tokenize("Bravo Six")[1].original, "Six");
    }

    #[test]
    fn spoken_numbers() {
        assert_eq!(parse("two hundred meters"), Some((200.0, 2)));
        assert_eq!(parse("one thousand five hundred"), Some((1500.0, 4)));
        assert_eq!(parse("twenty five"), Some((25.0, 2)));
        assert_eq!(parse("two hundred and fifty"), Some((250.0, 4)));
        assert_eq!(parse("one point five km"), Some((1.5, 3)));
        assert_eq!(parse("a hundred"), Some((100.0, 2)));
        assert_eq!(parse("3 hundred"), Some((300.0, 2)));
        assert_eq!(parse("three thirty"), Some((3.0, 1)));
        assert_eq!(parse("thirty two hundred"), Some((3200.0, 3)));
        assert_eq!(parse("twelve hundred"), Some((1200.0, 2)));
        assert_eq!(parse("hundred"), None);
        assert_eq!(parse("meters"), None);
        assert_eq!(parse("3:30"), None);
    }
}
