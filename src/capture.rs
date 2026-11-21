use std::collections::VecDeque;

use chrono::{DateTime, Utc};

use crate::audio::SAMPLE_RATE;

pub const FRAME_MS: usize = 20;
pub const AUDIO_FRAME_LEN: usize = SAMPLE_RATE as usize * FRAME_MS / 1_000;

const ATTACK_FRAMES: usize = 2;
const PREROLL_FRAMES: usize = 3;
const HANGOVER_FRAMES: usize = 300 / FRAME_MS;
const MIN_OPEN_FRAMES: usize = 100 / FRAME_MS;
const MAX_SAMPLES: usize = 30 * SAMPLE_RATE as usize;

pub struct Clip {
    pub started_at: DateTime<Utc>,
    pub ended_at: DateTime<Utc>,
    pub first_frame: u64,
    pub signal_first_frame: u64,
    pub signal_last_frame: u64,
    pub audio: Vec<f32>,
}

pub enum CaptureEvent {
    Started(DateTime<Utc>),
    Completed(Clip),
}

struct Recording {
    clip: Clip,
    open_frames: usize,
    closed_frames: usize,
}

pub struct Capture {
    preroll: VecDeque<[f32; AUDIO_FRAME_LEN]>,
    onset: Option<DateTime<Utc>>,
    recording: Option<Recording>,
    next_frame: u64,
}

impl Capture {
    pub fn new() -> Self {
        Self::at_frame(0)
    }

    pub fn at_frame(next_frame: u64) -> Self {
        Self {
            preroll: VecDeque::with_capacity(PREROLL_FRAMES),
            onset: None,
            recording: None,
            next_frame,
        }
    }

    pub fn push(
        &mut self,
        open: bool,
        frame: &[f32; AUDIO_FRAME_LEN],
        at: DateTime<Utc>,
    ) -> Option<CaptureEvent> {
        let frame_index = self.next_frame;
        self.next_frame += 1;
        if let Some(recording) = &mut self.recording {
            if open {
                recording.clip.audio.extend_from_slice(frame);
                recording.clip.ended_at = at;
                recording.clip.signal_last_frame = frame_index;
                recording.open_frames += 1;
                recording.closed_frames = 0;
            } else {
                // Keep short fades in the timing without feeding FM hiss to Whisper.
                recording.clip.audio.extend([0.0; AUDIO_FRAME_LEN]);
                recording.closed_frames += 1;
            }
            if recording.closed_frames >= HANGOVER_FRAMES
                || recording.clip.audio.len() >= MAX_SAMPLES
            {
                return self.finish().map(CaptureEvent::Completed);
            }
            return None;
        }

        let previous_onset = self.onset.take();
        if open {
            if let Some(start) = previous_onset {
                let mut audio = Vec::with_capacity(SAMPLE_RATE as usize * 2);
                let first_frame = frame_index - self.preroll.len() as u64;
                for previous in self.preroll.drain(..) {
                    audio.extend(previous);
                }
                audio.extend_from_slice(frame);
                self.recording = Some(Recording {
                    clip: Clip {
                        started_at: start,
                        ended_at: at,
                        first_frame,
                        signal_first_frame: frame_index - 1,
                        signal_last_frame: frame_index,
                        audio,
                    },
                    open_frames: ATTACK_FRAMES,
                    closed_frames: 0,
                });
                return Some(CaptureEvent::Started(start));
            }
            self.onset = Some(at);
        }
        if self.preroll.len() == PREROLL_FRAMES {
            self.preroll.pop_front();
        }
        self.preroll.push_back(*frame);
        None
    }

    pub fn finish(&mut self) -> Option<Clip> {
        let mut recording = self.recording.take()?;
        // Preserve one short silent block after the last carrier to retain its edge.
        let trim_frames = recording.closed_frames.saturating_sub(1);
        let audio = &mut recording.clip.audio;
        audio.truncate(audio.len() - trim_frames * AUDIO_FRAME_LEN);
        (recording.open_frames >= MIN_OPEN_FRAMES).then_some(recording.clip)
    }

    pub fn active(&self) -> bool {
        self.recording.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_transmissions_close_and_rearm_with_short_fades() {
        let mut capture = Capture::new();
        let audio = [0.5; AUDIO_FRAME_LEN];
        let noise = [0.9; AUDIO_FRAME_LEN];
        let time = Utc::now();
        for _ in 0..2 {
            assert!(capture.push(false, &noise, time).is_none());
            assert!(capture.push(true, &audio, time).is_none());
            assert!(matches!(
                capture.push(true, &audio, time),
                Some(CaptureEvent::Started(at)) if at == time
            ));
            // An RF fade shorter than the hangover must stay in the same clip.
            assert!(capture.push(false, &noise, time).is_none());
            for _ in 0..3 {
                assert!(capture.push(true, &audio, time).is_none());
            }
            for _ in 1..HANGOVER_FRAMES {
                assert!(capture.push(false, &noise, time).is_none());
            }
            let Some(CaptureEvent::Completed(clip)) = capture.push(false, &noise, time) else {
                panic!("squelch close did not complete the transmission");
            };
            assert_eq!(clip.audio.len(), 8 * AUDIO_FRAME_LEN);
            assert_eq!(
                &clip.audio[3 * AUDIO_FRAME_LEN..4 * AUDIO_FRAME_LEN],
                &[0.0; AUDIO_FRAME_LEN]
            );
            assert!(capture.finish().is_none());
        }
    }

    #[test]
    fn brief_carrier_spike_is_not_submitted() {
        let mut capture = Capture::new();
        let frame = [0.0; AUDIO_FRAME_LEN];
        let time = Utc::now();
        assert!(capture.push(true, &frame, time).is_none());
        for _ in 0..HANGOVER_FRAMES {
            assert!(capture.push(false, &frame, time).is_none());
        }
        assert!(capture.finish().is_none());
    }
}
