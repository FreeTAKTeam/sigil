use std::collections::VecDeque;
use std::ops::Range;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::bss::{Decoded, Position};
use crate::output::Source;

const TRAILING_RF_GAP_FRAMES: u64 = 300 / crate::capture::FRAME_MS as u64;

#[derive(Serialize, Deserialize)]
pub struct GatewayEvent {
    pub schema: String,
    pub id: String,
    pub timestamp: DateTime<Utc>,
    pub frequency_hz: u32,
    pub source: Source,
    #[serde(flatten)]
    pub kind: Kind,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Kind {
    RadioSeen {
        radio_id: String,
        display_name: String,
    },
    Position {
        radio_id: String,
        display_name: String,
        position: Position,
    },
    Transcript {
        radio_id: Option<String>,
        display_name: Option<String>,
        bss_event_id: Option<String>,
        unresolved_reason: Option<Unresolved>,
        started_at: DateTime<Utc>,
        text: String,
    },
    ProcessingFailure {
        stage: String,
        detail: String,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Unresolved {
    MissingIdentifier,
    AmbiguousIdentifier,
    IncompleteAudio,
}

#[derive(Debug, Eq, PartialEq)]
pub struct Attribution {
    pub radio_id: String,
    pub display_name: String,
    pub packet_id: String,
}

pub fn radio_id(sender: &str) -> Option<String> {
    let sender = sender.trim();
    if sender.is_empty()
        || sender.len() > 32
        || !sender.as_bytes()[0].is_ascii_alphanumeric()
        || !sender
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
    {
        return None;
    }
    Some(sender.to_ascii_uppercase())
}

pub struct AudioHistory {
    packets: VecDeque<Decoded>,
    gaps: VecDeque<Range<u64>>,
    window: u64,
}

impl AudioHistory {
    pub fn new(window: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            gaps: VecDeque::new(),
            window,
        }
    }

    pub fn packet(&mut self, packet: Decoded) {
        self.packets.push_back(packet);
    }

    pub fn gap(&mut self, gap: Range<u64>) {
        if let Some(last) = self.gaps.back_mut().filter(|last| last.end == gap.start) {
            last.end = gap.end;
        } else {
            self.gaps.push_back(gap);
        }
    }

    pub fn expire_before(&mut self, frame: u64) {
        self.packets.retain(|packet| packet.frames.end >= frame);
        self.gaps.retain(|gap| gap.end >= frame);
    }

    pub fn resolve(
        &self,
        audio: &Range<u64>,
        rf_end: u64,
        other_voice: &(impl Iterator<Item = Range<u64>> + Clone),
        active_voice_start: Option<u64>,
    ) -> Result<Attribution, Unresolved> {
        let deadline = audio.end + self.window;
        if self
            .gaps
            .iter()
            .any(|gap| gap.start < deadline && gap.end > audio.start)
        {
            return Err(Unresolved::IncompleteAudio);
        }
        let follows = |voice: &Range<u64>, packet: &Range<u64>| {
            // The last audible block can overlap the beginning of the AFSK mask by one clock tick.
            packet.start + 1 >= voice.end && packet.end <= voice.end + self.window
        };
        let mut selected: Option<Attribution> = None;
        for packet in self.packets.iter().filter(|packet| {
            follows(audio, &packet.frames) && packet.frames.start <= rf_end + TRAILING_RF_GAP_FRAMES
        }) {
            let Some(label) = &packet.sender else {
                continue;
            };
            let Some(normalized) = radio_id(label) else {
                continue;
            };
            if selected
                .as_ref()
                .is_some_and(|current| current.radio_id != normalized)
                || other_voice.clone().any(|other| {
                    follows(&other, &packet.frames)
                        || (other.start < packet.frames.end && other.end > audio.end)
                })
                || active_voice_start
                    .is_some_and(|start| start > audio.start && start < packet.frames.end)
            {
                return Err(Unresolved::AmbiguousIdentifier);
            }
            selected = Some(Attribution {
                radio_id: normalized,
                display_name: label.trim().to_owned(),
                packet_id: packet.id.clone(),
            });
        }
        selected.ok_or(Unresolved::MissingIdentifier)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn packet(sender: &str, start: u64) -> Decoded {
        Decoded {
            id: format!("packet-{start}"),
            at: Utc::now(),
            sender: Some(sender.into()),
            frames: start..start + 5,
            position: None,
            duplicate: false,
        }
    }

    #[test]
    fn trailing_identifiers_only_and_ambiguity_are_explicit() {
        let mut history = AudioHistory::new(25);
        history.packet(packet("Corvo", 10));
        assert_eq!(
            history.resolve(&(20..30), 30, &std::iter::empty(), None),
            Err(Unresolved::MissingIdentifier)
        );
        history.packet(packet(" Blacky ", 34));
        assert_eq!(
            history
                .resolve(&(20..30), 30, &std::iter::empty(), None)
                .unwrap()
                .radio_id,
            "BLACKY"
        );
        assert_eq!(
            history.resolve(&(20..30), 30, &std::iter::once(31..34), None),
            Err(Unresolved::AmbiguousIdentifier)
        );
        history.packet(packet("Corvo", 40));
        assert_eq!(
            history.resolve(&(20..30), 30, &std::iter::empty(), None),
            Err(Unresolved::AmbiguousIdentifier)
        );
        history.gap(32..33);
        assert_eq!(
            history.resolve(&(20..30), 30, &std::iter::empty(), None),
            Err(Unresolved::IncompleteAudio)
        );
        assert_eq!(radio_id("../../R4"), None);
        let mut history = AudioHistory::new(250);
        history.packet(packet("Corvo", 50));
        assert_eq!(
            history.resolve(&(20..30), 30, &std::iter::empty(), None),
            Err(Unresolved::MissingIdentifier)
        );
    }
}
