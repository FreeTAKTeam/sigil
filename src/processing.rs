use std::collections::VecDeque;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::bss::{self, Decoded};
use crate::capture::{AUDIO_FRAME_LEN, Capture, CaptureEvent, Clip, FRAME_MS};
use crate::correlation::{Attribution, AudioHistory, GatewayEvent, Kind, Unresolved, radio_id};
use crate::output::{Output, Source, log};

const AUDIO_QUEUE: usize = 256;
const ASR_QUEUE: usize = 16;
const EVENT_QUEUE: usize = 256;
const MAX_CLIP_FRAMES: u64 = 30_000 / FRAME_MS as u64 + 16;

struct Frame<T> {
    index: u64,
    at: DateTime<Utc>,
    audio: T,
}

type VoiceFrame = Frame<(bool, [f32; AUDIO_FRAME_LEN])>;

pub struct Ingress {
    next: u64,
    live: bool,
    voice: Option<SyncSender<VoiceFrame>>,
    data: Option<SyncSender<Frame<Vec<f32>>>>,
    dropped_voice: u64,
    dropped_data: u64,
}

impl Ingress {
    pub fn push(
        &mut self,
        open: bool,
        audio: &[f32; AUDIO_FRAME_LEN],
        raw: &[f32],
        at: DateTime<Utc>,
    ) {
        let index = self.next;
        self.next += 1;
        if let Some(voice) = &self.voice {
            self.dropped_voice += u64::from(!offer(
                voice,
                Frame {
                    index,
                    at,
                    audio: (open, *audio),
                },
                self.live,
            ));
        }
        if let Some(data) = &self.data {
            self.dropped_data += u64::from(!offer(
                data,
                Frame {
                    index,
                    at,
                    audio: raw.to_vec(),
                },
                self.live,
            ));
        }
    }
}

fn offer<T>(queue: &SyncSender<T>, value: T, live: bool) -> bool {
    if live {
        queue.try_send(value).is_ok()
    } else {
        queue.send(value).is_ok()
    }
}

enum Event {
    VoiceStart {
        frame: u64,
        at: DateTime<Utc>,
    },
    VoiceClip {
        clip: Clip,
        through: u64,
    },
    VoiceProgress {
        through: u64,
        active: bool,
    },
    DataProgress {
        through: u64,
        packets: Vec<Decoded>,
        masks: VecDeque<Range<u64>>,
    },
    Gap {
        lane: Lane,
        frames: Range<u64>,
    },
    Done(Lane, Option<String>),
    Inferred {
        id: String,
        result: Result<String>,
    },
}

#[derive(Clone, Copy)]
enum Lane {
    Voice,
    Data,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Self::Voice => "voice_capture",
            Self::Data => "bss",
        }
    }
}

struct Job {
    id: String,
    clip: Clip,
}

struct Pending {
    id: String,
    span: Range<u64>,
    rf_end: u64,
    started_at: DateTime<Utc>,
    ended_at: DateTime<Utc>,
    text: Option<Result<String>>,
    attribution: Option<Result<Attribution, Unresolved>>,
}

pub fn run<F>(
    output: &Output,
    source: Source,
    timeout_ms: u64,
    transcribe: Option<F>,
    decoder: Option<bss::Decoder>,
    input: impl FnOnce(&mut Ingress) -> Result<()>,
) -> Result<()>
where
    F: FnMut(&Clip) -> Result<String> + Send,
{
    let has_voice = transcribe.is_some();
    let has_data = decoder.is_some();
    let window = timeout_ms.div_ceil(FRAME_MS as u64);
    thread::scope(|scope| {
        let (send_events, events) = sync_channel(EVENT_QUEUE);
        let (send_voice, voice) = sync_channel(AUDIO_QUEUE);
        let (send_data, data) = sync_channel(AUDIO_QUEUE);
        let (jobs, work) = sync_channel(ASR_QUEUE);
        let voice_events = send_events.clone();
        scope.spawn(move || {
            if has_voice {
                let result = catch_unwind(AssertUnwindSafe(|| capture_voice(voice, &voice_events)));
                let error = result.err().map(|_| "voice capture worker panicked".into());
                let _ = voice_events.send(Event::Done(Lane::Voice, error));
            }
        });
        let data_events = send_events.clone();
        scope.spawn(move || {
            if let Some(decoder) = decoder {
                let result = catch_unwind(AssertUnwindSafe(|| decode_data(decoder, data, &data_events, output)))
                    .unwrap_or_else(|_| Err(anyhow::anyhow!("BSS decoder worker panicked")));
                let _ = data_events.send(Event::Done(
                    Lane::Data,
                    result.err().map(|err| format!("{err:#}")),
                ));
            }
        });
        scope.spawn(move || {
            if let Some(mut transcribe) = transcribe {
                let mut failed = false;
                for Job { id, clip } in work {
                    let result = if failed {
                        Err(anyhow::anyhow!("recognition worker is unavailable after a panic"))
                    } else {
                        catch_unwind(AssertUnwindSafe(|| transcribe(&clip))).unwrap_or_else(|_| {
                            failed = true;
                            Err(anyhow::anyhow!("recognition worker panicked"))
                        })
                    };
                    if send_events.send(Event::Inferred { id, result }).is_err() {
                        break;
                    }
                }
            }
        });
        let control = scope.spawn(move || {
            let mut coordinator = Coordinator::new(output, source, window, has_voice, has_data);
            for event in events {
                coordinator.event(event);
                coordinator.dispatch(&jobs);
                coordinator.settle();
                if coordinator.drained() {
                    break;
                }
            }
        });
        let mut ingress = Ingress {
            next: 0,
            live: matches!(source, Source::Live),
            voice: has_voice.then_some(send_voice),
            data: has_data.then_some(send_data),
            dropped_voice: 0,
            dropped_data: 0,
        };
        let result = input(&mut ingress);
        if ingress.dropped_voice != 0 || ingress.dropped_data != 0 {
            log(format_args!(
                "Dropped audio blocks, voice {}, BSS {}",
                ingress.dropped_voice, ingress.dropped_data
            ));
        }
        drop(ingress);
        control
            .join()
            .map_err(|_| anyhow::anyhow!("gateway event worker panicked"))?;
        result
    })
}

fn capture_voice(frames: Receiver<VoiceFrame>, events: &SyncSender<Event>) {
    let mut capture = Capture::new();
    let mut next = 0;
    for Frame {
        index,
        at,
        audio: (open, audio),
    } in frames
    {
        if index != next {
            if events
                .send(Event::Gap {
                    lane: Lane::Voice,
                    frames: next..index,
                })
                .is_err()
            {
                return;
            }
            capture = Capture::at_frame(index);
        }
        next = index + 1;
        let result = match capture.push(open, &audio, at) {
            Some(CaptureEvent::Started(at)) => events.send(Event::VoiceStart {
                frame: index - 1,
                at,
            }),
            Some(CaptureEvent::Completed(clip)) => events.send(Event::VoiceClip {
                clip,
                through: next,
            }),
            None => events.send(Event::VoiceProgress {
                through: next,
                active: capture.active(),
            }),
        };
        if result.is_err() {
            return;
        }
    }
    if let Some(clip) = capture.finish() {
        let _ = events.send(Event::VoiceClip {
            clip,
            through: next,
        });
    }
}

fn decode_data(
    mut decoder: bss::Decoder,
    frames: Receiver<Frame<Vec<f32>>>,
    events: &SyncSender<Event>,
    output: &Output,
) -> Result<()> {
    let mut next = 0;
    for Frame { index, at, audio } in frames {
        if index != next {
            events.send(Event::Gap {
                lane: Lane::Data,
                frames: next..index,
            })?;
            decoder.skip_to_frame(index)?;
        }
        next = index + 1;
        let packets = decoder.feed(&audio, at, output)?;
        let masks = decoder.take_voice_masks();
        events.send(Event::DataProgress {
            through: next,
            packets,
            masks,
        })?;
    }
    decoder.finish()
}

struct Coordinator<'a> {
    output: &'a Output,
    source: Source,
    window: u64,
    history: AudioHistory,
    voice_through: u64,
    data_through: u64,
    voice_done: bool,
    data_done: bool,
    has_data: bool,
    active_voice: Option<u64>,
    masks: VecDeque<Range<u64>>,
    data_gaps: VecDeque<Range<u64>>,
    waiting: VecDeque<(Clip, u64)>,
    pending: VecDeque<Pending>,
    recent_voice: VecDeque<(String, Range<u64>)>,
}

impl<'a> Coordinator<'a> {
    fn new(output: &'a Output, source: Source, window: u64, voice: bool, data: bool) -> Self {
        Self {
            output,
            source,
            window,
            history: AudioHistory::new(window),
            voice_through: 0,
            data_through: 0,
            voice_done: !voice,
            data_done: !data,
            has_data: data,
            active_voice: None,
            masks: VecDeque::new(),
            data_gaps: VecDeque::new(),
            waiting: VecDeque::new(),
            pending: VecDeque::new(),
            recent_voice: VecDeque::new(),
        }
    }

    fn event(&mut self, event: Event) {
        match event {
            Event::VoiceStart { frame, at } => {
                self.voice_through = frame + 2;
                self.active_voice = Some(frame);
                self.output.detected(at);
            }
            Event::VoiceClip { clip, through } => {
                self.active_voice = None;
                self.voice_through = through;
                self.output.completed(&clip);
                if self.waiting.len() < ASR_QUEUE {
                    self.waiting.push_back((clip, through));
                } else {
                    self.failure(
                        "voice_capture",
                        "audio waiting for BSS reached its queue limit".into(),
                    );
                }
            }
            Event::VoiceProgress { through, active } => {
                self.voice_through = through;
                if !active {
                    self.active_voice = None;
                }
            }
            Event::DataProgress {
                through,
                packets,
                mut masks,
            } => {
                self.data_through = through;
                self.masks.append(&mut masks);
                for packet in packets {
                    if !packet.duplicate
                        && let Some(sender) = &packet.sender
                        && let Some(radio_id) = radio_id(sender)
                    {
                        let display_name = sender.trim().to_owned();
                        let kind = match packet.position {
                            Some(position) => Kind::Position {
                                radio_id,
                                display_name,
                                position,
                            },
                            None => Kind::RadioSeen {
                                radio_id,
                                display_name,
                            },
                        };
                        self.persist(packet.id.clone(), packet.at, kind);
                    }
                    self.history.packet(packet);
                }
            }
            Event::Gap { lane, frames } => {
                self.failure(
                    lane.name(),
                    format!("dropped input blocks {}..{}", frames.start, frames.end),
                );
                self.history.gap(frames.clone());
                match lane {
                    Lane::Data => self.data_gaps.push_back(frames),
                    Lane::Voice => self.active_voice = None,
                }
            }
            Event::Done(lane, error) => {
                match lane {
                    Lane::Voice => {
                        self.voice_done = true;
                        self.active_voice = None;
                    }
                    Lane::Data => self.data_done = true,
                }
                if let Some(error) = error {
                    self.failure(lane.name(), error);
                }
            }
            Event::Inferred { id, result } => {
                if let Some(pending) = self.pending.iter_mut().find(|pending| pending.id == id) {
                    pending.text = Some(result);
                }
            }
        }
    }

    fn dispatch(&mut self, jobs: &SyncSender<Job>) {
        while self.waiting.front().is_some_and(|(_, through)| {
            !self.has_data || self.data_done || self.data_through >= *through
        }) {
            let Some((mut clip, through)) = self.waiting.pop_front() else {
                break;
            };
            if self.has_data
                && (self.data_through < through
                    || self
                        .data_gaps
                        .iter()
                        .any(|gap| gap.start < through && gap.end > clip.first_frame))
            {
                self.failure(
                    "voice_capture",
                    "BSS stream was incomplete while masking voice audio".into(),
                );
                continue;
            }
            let Some(span) = mask_audio(&mut clip, &self.masks) else {
                log("BSS burst excluded from speech recognition");
                continue;
            };
            let id = format!(
                "voice-{}-{}",
                clip.started_at.format("%Y%m%dT%H%M%S%.9fZ"),
                clip.first_frame
            );
            self.recent_voice.push_back((id.clone(), span.clone()));
            let started_at = clip.started_at;
            let ended_at = clip.ended_at;
            let rf_end = clip.signal_last_frame + 1;
            match jobs.try_send(Job {
                id: id.clone(),
                clip,
            }) {
                Ok(()) => self.pending.push_back(Pending {
                    id,
                    span,
                    rf_end,
                    started_at,
                    ended_at,
                    text: None,
                    attribution: None,
                }),
                Err(TrySendError::Full(_)) => {
                    self.failure("asr", "recognition queue is full".into());
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.failure("asr", "recognition worker is unavailable".into());
                }
            }
        }
    }

    fn settle(&mut self) {
        let through = self.voice_through.min(self.data_through);
        let mut retained = VecDeque::new();
        while let Some(mut pending) = self.pending.pop_front() {
            if pending.attribution.is_none() {
                pending.attribution = if !self.has_data {
                    Some(Err(Unresolved::MissingIdentifier))
                } else if through >= pending.span.end + self.window {
                    let others = self
                        .recent_voice
                        .iter()
                        .filter(|(id, _)| id != &pending.id)
                        .map(|(_, span)| span.clone());
                    Some(self.history.resolve(
                        &pending.span,
                        pending.rf_end,
                        &others,
                        self.active_voice,
                    ))
                } else if self.data_done && self.voice_done {
                    Some(Err(Unresolved::IncompleteAudio))
                } else {
                    None
                };
            }
            if pending.text.is_none() || pending.attribution.is_none() {
                retained.push_back(pending);
                continue;
            }
            self.complete(pending);
        }
        self.pending = retained;
        let watermark = if self.voice_done {
            self.data_through
        } else {
            through
        };
        let retain_from = watermark.saturating_sub(MAX_CLIP_FRAMES + self.window * 2);
        self.history.expire_before(retain_from);
        self.masks.retain(|mask| mask.end >= retain_from);
        self.data_gaps.retain(|gap| gap.end >= retain_from);
        self.recent_voice
            .retain(|(_, span)| span.end >= retain_from);
    }

    fn complete(&self, pending: Pending) {
        match pending.text {
            Some(Ok(text)) if !text.trim().is_empty() => {
                let (radio_id, display_name, bss_event_id, unresolved_reason) =
                    match pending.attribution {
                        Some(Ok(found)) => (
                            Some(found.radio_id),
                            Some(found.display_name),
                            Some(found.packet_id),
                            None,
                        ),
                        Some(Err(reason)) => (None, None, None, Some(reason)),
                        None => return,
                    };
                self.persist(
                    pending.id,
                    pending.ended_at,
                    Kind::Transcript {
                        radio_id,
                        display_name,
                        bss_event_id,
                        unresolved_reason,
                        started_at: pending.started_at,
                        text,
                    },
                );
            }
            Some(Err(error)) => self.failure("asr", format!("{}: {error:#}", pending.id)),
            _ => {}
        }
    }

    fn persist(&self, id: String, at: DateTime<Utc>, kind: Kind) {
        let event = GatewayEvent {
            schema: "sdr.gateway.v1".into(),
            id,
            timestamp: at,
            source: self.source,
            frequency_hz: self.output.frequency_hz(),
            kind,
        };
        if let Err(error) = self.output.save_event(at, "gateway", &event) {
            log(format_args!("Gateway event could not be saved: {error:#}"));
        }
    }

    fn failure(&self, stage: &str, detail: String) {
        let now = Utc::now();
        log(format_args!("{stage}: {detail}"));
        self.persist(
            format!("{stage}-{}", now.format("%Y%m%dT%H%M%S%.9fZ")),
            now,
            Kind::ProcessingFailure {
                stage: stage.into(),
                detail,
            },
        );
    }

    fn drained(&self) -> bool {
        self.voice_done && self.data_done && self.pending.is_empty() && self.waiting.is_empty()
    }
}

fn mask_audio(clip: &mut Clip, masks: &VecDeque<Range<u64>>) -> Option<Range<u64>> {
    let mut span = clip.signal_first_frame..clip.signal_last_frame + 1;
    let Some(masked_until) = masks
        .iter()
        .filter(|mask| mask.start < span.end && mask.end > clip.first_frame)
        .map(|mask| mask.end)
        .max()
    else {
        return Some(span);
    };
    let mut audible = clip
        .audio
        .chunks_mut(AUDIO_FRAME_LEN)
        .enumerate()
        .filter_map(|(offset, audio)| {
            let index = clip.first_frame + offset as u64;
            if masks.iter().any(|mask| mask.contains(&index)) {
                audio.fill(0.0);
                return None;
            }
            let samples = f32::from(u16::try_from(audio.len()).expect("voice block fits u16"));
            let mean_square = audio.iter().map(|sample| sample * sample).sum::<f32>() / samples;
            // The captured radios keep the carrier open after the AFSK frame.
            // Reject that low-level suffix while retaining speech that resumes.
            let minimum_rms: f32 = if index >= masked_until { 0.009 } else { 0.002 };
            if index >= masked_until && mean_square < minimum_rms.powi(2) {
                audio.fill(0.0);
            }
            (span.contains(&index) && mean_square >= minimum_rms.powi(2)).then_some(index)
        });
    let start = audible.next()?;
    let mut last = start;
    let count = 1 + audible.map(|index| last = index).count();
    span = start..last + 1;
    (count >= 5).then_some(span)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{Duration, Instant};

    use chrono::TimeDelta;
    use yodel::ax25::hdlc::frame_bits;
    use yodel::{Bit, Modulator, ModulatorConfig, NrziEncoder, SampleRate};

    use super::*;
    use crate::radio::{IF_FRAME_LEN, IF_RATE};

    #[test]
    fn a_quiet_carrier_tail_does_not_turn_a_trailing_packet_into_a_leading_packet() {
        let now = Utc::now();
        let masks = VecDeque::from_iter(std::iter::once(20..30));
        let mut clip = Clip {
            started_at: now,
            ended_at: now,
            first_frame: 0,
            signal_first_frame: 0,
            signal_last_frame: 49,
            audio: vec![0.006; AUDIO_FRAME_LEN * 50],
        };
        clip.audio[..20 * AUDIO_FRAME_LEN].fill(0.08);
        assert_eq!(mask_audio(&mut clip, &masks), Some(0..20));
        assert!(
            clip.audio[20 * AUDIO_FRAME_LEN..]
                .iter()
                .all(|sample| *sample == 0.0)
        );
        clip.audio[31 * AUDIO_FRAME_LEN..40 * AUDIO_FRAME_LEN].fill(0.03);
        assert_eq!(mask_audio(&mut clip, &masks), Some(0..40));
    }

    fn packet(name: [u8; 2]) -> Result<Vec<f32>> {
        let mut payload = vec![1, 3, 0x20];
        payload.extend_from_slice(&name);
        payload.extend(hex::decode("0725124F80DDAAF0")?);
        let config = ModulatorConfig::bell_202(SampleRate::new(IF_RATE).unwrap()).unwrap();
        Ok(Modulator::new(config)
            .f32_samples(NrziEncoder::new(Bit::One).encode_iter(frame_bits(&payload, 24, 4)))
            .map(|sample| sample * 0.25)
            .collect())
    }

    fn recorded(output: &Output) -> Result<Vec<GatewayEvent>> {
        let mut events = Vec::new();
        for entry in output.directory().read_dir()? {
            let entry = entry?;
            if entry.file_name().to_string_lossy().starts_with("gateway_")
                && entry.path().extension().is_some_and(|ext| ext == "json")
            {
                events.push(serde_json::from_slice(&fs::read(entry.path())?)?);
            }
        }
        Ok(events)
    }

    #[test]
    fn bss_mask_and_position_complete_while_recognizer_is_waiting() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let voice = [0.15; AUDIO_FRAME_LEN];
        let mut signal = vec![0.0; IF_FRAME_LEN * 195];
        let r4 = packet(*b"R4")?;
        let r5 = packet(*b"R5")?;
        signal[43 * IF_FRAME_LEN..43 * IF_FRAME_LEN + r4.len()].copy_from_slice(&r4);
        signal[132 * IF_FRAME_LEN..132 * IF_FRAME_LEN + r5.len()].copy_from_slice(&r5);
        let r4_end = 43 + r4.len().div_ceil(IF_FRAME_LEN);
        let r5_end = 132 + r5.len().div_ceil(IF_FRAME_LEN);
        let start = Utc::now();
        let recognize = |clip: &Clip| -> Result<String> {
            assert!(clip.audio.iter().any(|value| value.abs() > 0.1));
            let deadline = Instant::now() + Duration::from_secs(5);
            loop {
                if recorded(&output)?.iter().any(|event| matches!(&event.kind, Kind::Position { radio_id, .. } if radio_id == "R5")) { break; }
                anyhow::ensure!(
                    Instant::now() < deadline,
                    "BSS telemetry stalled behind ASR"
                );
                thread::sleep(Duration::from_millis(10));
            }
            Ok("Test voice".into())
        };
        run(
            &output,
            Source::Wav,
            1_000,
            Some(recognize),
            Some(bss::Decoder::new(IF_RATE, Source::Wav, false, None)?),
            |ingress| {
                for (index, frame) in signal.chunks_exact(IF_FRAME_LEN).enumerate() {
                    let open = (5..r4_end).contains(&index) || (132..r5_end).contains(&index);
                    let voice_frame = if open {
                        &voice
                    } else {
                        &[0.0; AUDIO_FRAME_LEN]
                    };
                    ingress.push(
                        open,
                        voice_frame,
                        frame,
                        start + TimeDelta::milliseconds(i64::try_from(index * FRAME_MS)?),
                    );
                }
                Ok(())
            },
        )?;
        let records = recorded(&output)?;
        assert_eq!(
            records
                .iter()
                .filter(|e| matches!(&e.kind, Kind::Position { .. }))
                .count(),
            2
        );
        let transcripts: Vec<_> = records
            .iter()
            .filter(|e| matches!(&e.kind, Kind::Transcript { .. }))
            .collect();
        assert_eq!(transcripts.len(), 1);
        assert!(
            matches!(&transcripts[0].kind, Kind::Transcript { radio_id: Some(id), text, .. } if id == "R4" && text == "Test voice")
        );
        Ok(())
    }
}
