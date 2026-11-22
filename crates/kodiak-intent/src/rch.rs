//! RCH command recognition: a command must match a phrase completely.

use std::collections::BTreeMap;

use kodiak_core::{Category, Intent, RchCommand, RejectReason, Rejection};

use crate::text::Token;
use crate::vocabulary::{Rch, phrase_words};

enum Target {
    Command(usize),
    Unsupported(usize),
}

pub struct Parser {
    vocabulary: Rch,
    phrases: Vec<(Vec<String>, Target)>,
}

impl Parser {
    pub fn new(vocabulary: Rch) -> Self {
        let mut phrases = Vec::new();
        for (index, command) in vocabulary.commands.iter().enumerate() {
            for phrase in &command.phrases {
                phrases.push((phrase_words(phrase), Target::Command(index)));
            }
        }
        for (index, entry) in vocabulary.unsupported.iter().enumerate() {
            for phrase in &entry.phrases {
                phrases.push((phrase_words(phrase), Target::Unsupported(index)));
            }
        }
        // Longest phrase first, so "set nickname to" wins over "nickname".
        phrases.sort_by_key(|(words, _)| std::cmp::Reverse(words.len()));
        Self {
            vocabulary,
            phrases,
        }
    }

    /// Returns `None` when the words are not an RCH command.
    pub fn parse(&self, tokens: &[Token]) -> Option<Result<Intent, Rejection>> {
        let tokens: Vec<&Token> = tokens
            .iter()
            .filter(|token| !self.vocabulary.fillers.contains(&token.word))
            .collect();
        for (phrase, target) in &self.phrases {
            if tokens.len() < phrase.len()
                || !tokens
                    .iter()
                    .zip(phrase)
                    .all(|(token, word)| token.word == *word)
            {
                continue;
            }
            let rest = &tokens[phrase.len()..];
            match target {
                Target::Command(index) => {
                    let command = &self.vocabulary.commands[*index];
                    let mut arguments = BTreeMap::new();
                    match &command.argument {
                        None if !rest.is_empty() => continue,
                        None => {}
                        Some(name) => {
                            let skip = rest
                                .iter()
                                .take_while(|t| command.argument_lead.contains(&t.word))
                                .count();
                            let value: Vec<&str> =
                                rest[skip..].iter().map(|t| t.original.as_str()).collect();
                            if value.is_empty() {
                                return Some(Err(Rejection::new(
                                    RejectReason::MissingParameter,
                                    Some(Category::Rch),
                                    format!("{} needs a {name}", command.intent),
                                )));
                            }
                            arguments.insert(name.clone(), value.join(" "));
                        }
                    }
                    return Some(Ok(Intent::RchCommand(RchCommand {
                        command: command.intent.clone(),
                        arguments,
                    })));
                }
                Target::Unsupported(index) => {
                    let entry = &self.vocabulary.unsupported[*index];
                    if !rest.is_empty() && !entry.prefix {
                        continue;
                    }
                    return Some(Err(Rejection::new(
                        RejectReason::UnsupportedCommand,
                        Some(Category::Rch),
                        format!("{}: {}", entry.name, entry.reason),
                    )));
                }
            }
        }
        None
    }
}
