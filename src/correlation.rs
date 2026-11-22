use std::collections::VecDeque;
use std::ops::Range;

use chrono::{DateTime, Utc};
use kodiak_core::KodiakResult;
use serde::{Deserialize, Serialize};

use crate::bss::{Decoded, Position};
use crate::output::Source;

use crate::capture::FRAME_MS;

/// A packet starting within 300 ms of the carrier's end belongs to the same
/// transmission (the radio's own trailing ID).
const SAME_CARRIER_FRAMES: u64 = 300 / FRAME_MS as u64;

/// Correlation timing for trailing BSS identifiers.
#[derive(Clone, Copy, Debug)]
pub struct Timing {
    /// How long to wait after speech for competing voice or BSS evidence.
    pub window_ms: u64,
    /// Longest gap between the end of the carrier and the start of the BSS
    /// packet that still identifies the speaker. Radios such as the VR-N76
    /// may key up again 1–2.5 s after the voice to send the packet.
    pub trailing_gap_ms: u64,
}

impl Timing {
    pub const DEFAULT_WINDOW_MS: u64 = 5_000;
    pub const DEFAULT_TRAILING_GAP_MS: u64 = 3_000;

    pub fn window_frames(self) -> u64 {
        self.window_ms.div_ceil(FRAME_MS as u64)
    }

    pub fn gap_frames(self) -> u64 {
        self.trailing_gap_ms.div_ceil(FRAME_MS as u64)
    }
}

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
        /// GPS fix carried by the attributed BSS packet.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        position: Option<Position>,
        /// Kodiak's chat, intent or rejection. Absent in older events.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kodiak: Option<Box<KodiakResult>>,
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

#[derive(Debug, PartialEq)]
pub struct Attribution {
    pub radio_id: String,
    pub display_name: String,
    pub packet_id: String,
    pub position: Option<Position>,
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
    trailing_gap: u64,
}

impl AudioHistory {
    pub fn new(window: u64, trailing_gap: u64) -> Self {
        Self {
            packets: VecDeque::new(),
            gaps: VecDeque::new(),
            window,
            trailing_gap,
        }
    }

    pub fn packet(&mut self, packet: Decoded) {
        self.packets.push_back(packet);
    }

    /// Records lost blocks. A loss before both lanes is reported by each
    /// with the same range; it is merged rather than stored twice.
    pub fn gap(&mut self, gap: Range<u64>) {
        if let Some(last) = self
            .gaps
            .back_mut()
            .filter(|last| last.start <= gap.start && last.end >= gap.start)
        {
            last.end = last.end.max(gap.end);
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
        let candidates: Vec<&Decoded> = self
            .packets
            .iter()
            .filter(|packet| {
                follows(audio, &packet.frames)
                    && packet.frames.start <= rf_end + self.trailing_gap
                    && packet.sender.as_deref().and_then(radio_id).is_some()
            })
            .collect();
        // A packet sent while the speaker still held the carrier is stronger
        // evidence than one in a separate, later burst: only the radio holding
        // the channel can send inside it. On 21 November 2026 BLACKY's packet
        // ended its own transmission and Corvo's beacon followed 1 s later.
        let within: Vec<&Decoded> = candidates
            .iter()
            .copied()
            .filter(|packet| packet.frames.start <= rf_end + SAME_CARRIER_FRAMES)
            .collect();
        let candidates = if within.is_empty() {
            candidates
        } else {
            within
        };
        let mut selected: Option<Attribution> = None;
        for packet in candidates {
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
                position: packet.position,
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
    fn a_packet_in_the_speakers_own_carrier_beats_a_later_burst() {
        let mut history = AudioHistory::new(250, 150);
        // BLACKY's ID ends its own transmission; Corvo's beacon follows.
        history.packet(packet("BLACKY", 30));
        history.packet(packet("Corvo", 80));
        let found = history.resolve(&(10..30), 32, &std::iter::empty(), None);
        assert_eq!(found.unwrap().radio_id, "BLACKY");
        // Two different IDs inside the same carrier stay ambiguous.
        history.packet(packet("Corvo", 33));
        assert_eq!(
            history.resolve(&(10..30), 32, &std::iter::empty(), None),
            Err(Unresolved::AmbiguousIdentifier)
        );
        // With no packet inside the carrier, separate bursts still compete.
        let mut later = AudioHistory::new(250, 150);
        later.packet(packet("BLACKY", 60));
        later.packet(packet("Corvo", 90));
        assert_eq!(
            later.resolve(&(10..30), 30, &std::iter::empty(), None),
            Err(Unresolved::AmbiguousIdentifier)
        );
    }

    #[test]
    fn a_separate_burst_after_the_voice_counts_within_the_configured_gap() {
        // VR-N76 on 21 November 2026: the voice carrier ended, then the radio
        // keyed up again about 1.1 s later (55 blocks) to send its BSS packet.
        let mut strict = AudioHistory::new(250, 15);
        let mut adapted = AudioHistory::new(250, 100);
        for history in [&mut strict, &mut adapted] {
            history.packet(packet("Corvo", 85));
        }
        assert_eq!(
            strict.resolve(&(10..30), 30, &std::iter::empty(), None),
            Err(Unresolved::MissingIdentifier)
        );
        assert_eq!(
            adapted
                .resolve(&(10..30), 30, &std::iter::empty(), None)
                .unwrap()
                .radio_id,
            "CORVO"
        );
        // Another transmission inside the gap makes the owner ambiguous.
        assert_eq!(
            adapted.resolve(&(10..30), 30, &std::iter::once(40..60), None),
            Err(Unresolved::AmbiguousIdentifier)
        );
        assert_eq!(
            adapted.resolve(&(10..30), 30, &std::iter::empty(), Some(40)),
            Err(Unresolved::AmbiguousIdentifier)
        );
        // Beyond the gap the packet is not the speaker's.
        assert_eq!(
            AudioHistory::new(250, 50).resolve(&(10..30), 30, &std::iter::empty(), None),
            Err(Unresolved::MissingIdentifier)
        );
    }

    #[test]
    fn trailing_identifiers_only_and_ambiguity_are_explicit() {
        let mut history = AudioHistory::new(25, 15);
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
        let mut history = AudioHistory::new(250, 15);
        history.packet(packet("Corvo", 50));
        assert_eq!(
            history.resolve(&(20..30), 30, &std::iter::empty(), None),
            Err(Unresolved::MissingIdentifier)
        );
    }
}
