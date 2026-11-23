use std::any::Any;
use std::collections::VecDeque;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use kodiak_core::KodiakResult;

use crate::bss::{self, Decoded};
use crate::capture::{AUDIO_FRAME_LEN, Capture, CaptureEvent, Clip, FRAME_MS};
use crate::correlation::{
    Attribution, AudioHistory, GatewayEvent, Kind, Timing, Unresolved, radio_id,
};
use crate::output::{Output, Source, log};

const AUDIO_QUEUE: usize = 256;
const ASR_QUEUE: usize = 16;
const EVENT_QUEUE: usize = 256;
const MAX_CLIP_FRAMES: u64 = 30_000 / FRAME_MS as u64 + 16;
/// Consecutive recognizer panics after which the gateway stops. Kodiak
/// reloads the model after each panic; repeated panics mean it cannot.
const MAX_CONSECUTIVE_ASR_PANICS: u32 = 3;
/// Shortest interval between running drop-count reports.
const DROP_REPORT_INTERVAL: Duration = Duration::from_mins(1);

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
    /// Blocks lost on USB before the pipeline, and in each lane's queue.
    dropped: Drops,
    reported: Drops,
    reported_at: Option<Instant>,
}

#[derive(Clone, Copy, Default, PartialEq)]
struct Drops {
    usb: u64,
    voice: u64,
    data: u64,
}

impl Ingress {
    fn new(
        live: bool,
        voice: Option<SyncSender<VoiceFrame>>,
        data: Option<SyncSender<Frame<Vec<f32>>>>,
    ) -> Self {
        Self {
            next: 0,
            live,
            voice,
            data,
            dropped: Drops::default(),
            reported: Drops::default(),
            reported_at: None,
        }
    }

    /// Skips `frames` block indexes lost before the pipeline. Both lanes
    /// see the jump and report it as a gap.
    pub fn skip(&mut self, frames: u64) {
        self.next = self.next.saturating_add(frames);
        self.dropped.usb = self.dropped.usb.saturating_add(frames);
        self.report_drops();
    }

    fn report_drops(&mut self) {
        if self.dropped == self.reported
            || self
                .reported_at
                .is_some_and(|at| at.elapsed() < DROP_REPORT_INTERVAL)
        {
            return;
        }
        self.log_drops();
        self.reported = self.dropped;
        self.reported_at = Some(Instant::now());
    }

    fn log_drops(&self) {
        let Drops { usb, voice, data } = self.dropped;
        log(format_args!(
            "Dropped audio blocks: USB {usb}, voice {voice}, BSS {data}"
        ));
    }

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
            self.dropped.voice += u64::from(!offer(
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
            self.dropped.data += u64::from(!offer(
                data,
                Frame {
                    index,
                    at,
                    audio: raw.to_vec(),
                },
                self.live,
            ));
        }
        self.report_drops();
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
        /// Errors the decoder recovered from, such as a failed save.
        failures: Vec<String>,
    },
    Gap {
        lane: Lane,
        frames: Range<u64>,
    },
    /// A worker ended; an error is fatal to the gateway.
    Done(Lane, Option<String>),
    Inferred {
        id: String,
        // Boxed: a Kodiak result is far larger than the other events.
        result: Box<Result<KodiakResult>>,
    },
}

#[derive(Clone, Copy)]
enum Lane {
    Voice,
    Data,
    Asr,
}

impl Lane {
    fn name(self) -> &'static str {
        match self {
            Self::Voice => "voice_capture",
            Self::Data => "bss",
            Self::Asr => "asr",
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
    text: Option<Result<KodiakResult>>,
    attribution: Option<Result<Attribution, Unresolved>>,
}

/// Runs the voice, BSS, recognition and event workers over the frames
/// `input` pushes.
///
/// A worker panic, a BSS lane error or repeated recognizer panics are
/// fatal: they are recorded as a `processing_failure`, `stop` is set so a
/// live receiver ends, and after draining the error is returned.
pub fn run<F>(
    output: &Output,
    source: Source,
    timing: Timing,
    transcribe: Option<F>,
    decoder: Option<bss::Decoder>,
    stop: &AtomicBool,
    input: impl FnOnce(&mut Ingress) -> Result<()>,
) -> Result<()>
where
    F: FnMut(&Clip) -> Result<KodiakResult> + Send,
{
    let has_voice = transcribe.is_some();
    let has_data = decoder.is_some();
    let window = timing.window_frames();
    let gap = timing.gap_frames();
    thread::scope(|scope| {
        let (send_events, events) = sync_channel(EVENT_QUEUE);
        let (send_voice, voice) = sync_channel(AUDIO_QUEUE);
        let (send_data, data) = sync_channel(AUDIO_QUEUE);
        let (jobs, work) = sync_channel(ASR_QUEUE);
        let voice_events = send_events.clone();
        scope.spawn(move || {
            if has_voice {
                let result = catch_unwind(AssertUnwindSafe(|| capture_voice(voice, &voice_events)));
                let error = result.err().map(|payload| {
                    format!(
                        "voice capture worker panicked: {}",
                        panic_message(&*payload)
                    )
                });
                let _ = voice_events.send(Event::Done(Lane::Voice, error));
            }
        });
        let data_events = send_events.clone();
        scope.spawn(move || {
            if let Some(decoder) = decoder {
                let result = catch_unwind(AssertUnwindSafe(|| {
                    decode_data(decoder, data, &data_events, output)
                }))
                .unwrap_or_else(|payload| {
                    Err(anyhow!(
                        "BSS decoder worker panicked: {}",
                        panic_message(&*payload)
                    ))
                });
                let _ = data_events.send(Event::Done(
                    Lane::Data,
                    result.err().map(|err| format!("{err:#}")),
                ));
            }
        });
        scope.spawn(move || {
            if let Some(transcribe) = transcribe {
                recognize(transcribe, work, &send_events);
            }
        });
        let control = scope.spawn(move || {
            supervised(stop, || {
                let mut coordinator =
                    Coordinator::new(output, source, window, gap, has_voice, has_data, stop);
                for event in events {
                    coordinator.event(event);
                    coordinator.dispatch(&jobs);
                    coordinator.settle();
                    if coordinator.drained() {
                        break;
                    }
                }
                coordinator.fatal
            })
        });
        let mut ingress = Ingress::new(
            matches!(source, Source::Live),
            has_voice.then_some(send_voice),
            has_data.then_some(send_data),
        );
        let result = input(&mut ingress);
        if ingress.dropped != Drops::default() {
            ingress.log_drops();
        }
        drop(ingress);
        let fatal = control
            .join()
            .map_err(|_| anyhow!("gateway event worker panicked"))?;
        if let Some(fatal) = fatal {
            if let Err(error) = &result {
                log(format_args!("Input also failed: {error:#}"));
            }
            anyhow::bail!("processing stopped: {fatal}");
        }
        result
    })
}

/// Runs the event worker, setting `stop` as soon as it panics so a live
/// receiver does not run on without it. Returns the fatal error, if any.
fn supervised(stop: &AtomicBool, work: impl FnOnce() -> Option<String>) -> Option<String> {
    catch_unwind(AssertUnwindSafe(work)).unwrap_or_else(|payload| {
        let error = format!(
            "gateway event worker panicked: {}",
            panic_message(&*payload)
        );
        log(&error);
        stop.store(true, Ordering::Relaxed);
        Some(error)
    })
}

fn panic_message(payload: &(dyn Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("unknown panic")
}

/// Transcribes clips until the queue closes. Kodiak reloads its model after
/// a panic, so a panic fails only its own clip; consecutive panics are fatal.
fn recognize<F>(mut transcribe: F, work: Receiver<Job>, events: &SyncSender<Event>)
where
    F: FnMut(&Clip) -> Result<KodiakResult>,
{
    let mut panics = 0;
    while let Ok(Job { id, clip }) = work.recv() {
        let result = match catch_unwind(AssertUnwindSafe(|| transcribe(&clip))) {
            Ok(result) => {
                panics = 0;
                result
            }
            Err(payload) => {
                panics += 1;
                Err(anyhow!(
                    "recognition worker panicked: {}",
                    panic_message(&*payload)
                ))
            }
        };
        let result = Box::new(result);
        if events.send(Event::Inferred { id, result }).is_err() {
            return;
        }
        if panics >= MAX_CONSECUTIVE_ASR_PANICS {
            let error = format!("recognition panicked on {panics} consecutive clips");
            // Close the queue first, so every clip dispatched after the
            // coordinator sees this event fails at once.
            drop(work);
            let _ = events.send(Event::Done(Lane::Asr, Some(error)));
            return;
        }
    }
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
        events.send(Event::DataProgress {
            through: next,
            packets,
            masks: decoder.take_voice_masks(),
            failures: decoder.take_failures(),
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
    stop: &'a AtomicBool,
    /// The first fatal failure, returned once the workers have drained.
    fatal: Option<String>,
    /// Whether the recognition worker closed its queue and said why.
    asr: AsrState,
}

/// The recognition worker closes its queue just before reporting why it
/// stopped, so a clip can find the queue closed while the report is still
/// on its way. Draining waits for the report.
#[derive(Clone, Copy, PartialEq, Eq)]
enum AsrState {
    Running,
    /// A clip found the queue closed; the worker's report has not arrived.
    Closed,
    /// The worker reported that it stopped.
    Done,
}

impl<'a> Coordinator<'a> {
    fn new(
        output: &'a Output,
        source: Source,
        window: u64,
        gap: u64,
        voice: bool,
        data: bool,
        stop: &'a AtomicBool,
    ) -> Self {
        Self {
            output,
            source,
            window,
            history: AudioHistory::new(window, gap),
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
            stop,
            fatal: None,
            asr: AsrState::Running,
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
                failures,
            } => {
                self.data_through = through;
                self.masks.append(&mut masks);
                for detail in failures {
                    self.failure(Lane::Data.name(), detail);
                }
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
                    Lane::Voice | Lane::Asr => self.active_voice = None,
                }
            }
            Event::Done(lane, error) => {
                match lane {
                    Lane::Voice => {
                        self.voice_done = true;
                        self.active_voice = None;
                    }
                    Lane::Data => self.data_done = true,
                    Lane::Asr => {
                        self.asr = AsrState::Done;
                        // The worker closed its queue: queued clips will not
                        // be transcribed, and later ones are refused.
                        for pending in &mut self.pending {
                            pending
                                .text
                                .get_or_insert_with(|| Err(anyhow!("recognition worker stopped")));
                        }
                    }
                }
                if let Some(error) = error {
                    self.fatal(lane.name(), error);
                }
            }
            Event::Inferred { id, result } => {
                if let Some(pending) = self.pending.iter_mut().find(|pending| pending.id == id) {
                    pending.text = Some(*result);
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
                    if self.asr == AsrState::Running {
                        self.asr = AsrState::Closed;
                    }
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
            Some(Ok(result)) if !result.transcript().trim().is_empty() => {
                let (radio_id, display_name, bss_event_id, position, unresolved_reason) =
                    match pending.attribution {
                        Some(Ok(found)) => (
                            Some(found.radio_id),
                            Some(found.display_name),
                            Some(found.packet_id),
                            found.position,
                            None,
                        ),
                        Some(Err(reason)) => (None, None, None, None, Some(reason)),
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
                        text: result.transcript().to_owned(),
                        position,
                        kodiak: Some(Box::new(result)),
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

    /// Records a failure the gateway cannot run on with and stops the input.
    fn fatal(&mut self, stage: &str, detail: String) {
        if self.fatal.is_none() {
            self.fatal = Some(format!("{stage}: {detail}"));
            self.stop.store(true, Ordering::Relaxed);
        }
        self.failure(stage, detail);
    }

    fn drained(&self) -> bool {
        self.voice_done
            && self.data_done
            && self.pending.is_empty()
            && self.waiting.is_empty()
            && self.asr != AsrState::Closed
    }
}

/// Blocks of continuous audio after a BSS burst before speech counts as
/// resumed. A single noise block after BLACKY's packet on 21 November 2026
/// otherwise extended the speech past the packet and lost the attribution.
const RESUMED_SPEECH_FRAMES: usize = 5;

fn mask_audio(clip: &mut Clip, masks: &VecDeque<Range<u64>>) -> Option<Range<u64>> {
    let span = clip.signal_first_frame..clip.signal_last_frame + 1;
    let Some(masked_until) = masks
        .iter()
        .filter(|mask| mask.start < span.end && mask.end > clip.first_frame)
        .map(|mask| mask.end)
        .max()
    else {
        return Some(span);
    };
    let first = clip.first_frame;
    let mut audible: Vec<bool> = clip
        .audio
        .chunks_mut(AUDIO_FRAME_LEN)
        .enumerate()
        .map(|(offset, audio)| {
            let index = first + offset as u64;
            if masks.iter().any(|mask| mask.contains(&index)) {
                audio.fill(0.0);
                return false;
            }
            let samples = f32::from(u16::try_from(audio.len()).expect("voice block fits u16"));
            let mean_square = audio.iter().map(|sample| sample * sample).sum::<f32>() / samples;
            // The captured radios keep the carrier open after the AFSK frame.
            // Reject that low-level suffix while retaining speech that resumes.
            let minimum_rms: f32 = if index >= masked_until { 0.009 } else { 0.002 };
            mean_square >= minimum_rms.powi(2)
        })
        .collect();
    // After the last burst, keep only sustained runs: isolated noise blocks
    // must not extend the speech past a trailing packet.
    let tail = usize::try_from(masked_until.saturating_sub(first))
        .unwrap_or(usize::MAX)
        .min(audible.len());
    let mut run = None;
    for offset in tail..=audible.len() {
        if audible.get(offset).copied().unwrap_or(false) {
            run.get_or_insert(offset);
        } else if let Some(begin) = run.take()
            && offset - begin < RESUMED_SPEECH_FRAMES
        {
            audible[begin..offset].fill(false);
        }
    }
    for (offset, audio) in clip.audio.chunks_mut(AUDIO_FRAME_LEN).enumerate() {
        if offset >= tail && !audible[offset] {
            audio.fill(0.0);
        }
    }
    let heard: Vec<u64> = audible
        .iter()
        .enumerate()
        .map(|(offset, audible)| (first + offset as u64, *audible))
        .filter(|(index, audible)| *audible && span.contains(index))
        .map(|(index, _)| index)
        .collect();
    let (&start, &last) = (heard.first()?, heard.last()?);
    (heard.len() >= 5).then_some(start..last + 1)
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
        let masks = std::iter::once(20..30).collect::<VecDeque<_>>();
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

    #[test]
    fn a_single_noise_block_after_the_packet_does_not_extend_the_speech() {
        let now = Utc::now();
        let masks = std::iter::once(20..30).collect::<VecDeque<_>>();
        let mut clip = Clip {
            started_at: now,
            ended_at: now,
            first_frame: 0,
            signal_first_frame: 0,
            signal_last_frame: 39,
            audio: vec![0.0; AUDIO_FRAME_LEN * 40],
        };
        clip.audio[..20 * AUDIO_FRAME_LEN].fill(0.08);
        // One 20 ms block at RMS 0.010 after the packet, as BLACKY's tail had.
        clip.audio[37 * AUDIO_FRAME_LEN..38 * AUDIO_FRAME_LEN].fill(0.010);
        assert_eq!(mask_audio(&mut clip, &masks), Some(0..20));
        assert!(clip.audio[20 * AUDIO_FRAME_LEN..].iter().all(|s| *s == 0.0));
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
        let recognize = |clip: &Clip| -> Result<KodiakResult> {
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
            Ok(KodiakResult::Chat {
                content: "Test voice".into(),
            })
        };
        run(
            &output,
            Source::Wav,
            Timing {
                window_ms: 1_000,
                trailing_gap_ms: 300,
            },
            Some(recognize),
            Some(bss::Decoder::new(IF_RATE, Source::Wav, false, None)?),
            &AtomicBool::new(false),
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

    const TIMING: Timing = Timing {
        window_ms: 1_000,
        trailing_gap_ms: 300,
    };

    fn failures(records: &[GatewayEvent]) -> Vec<(&str, &str)> {
        records
            .iter()
            .filter_map(|event| match &event.kind {
                Kind::ProcessingFailure { stage, detail } => {
                    Some((stage.as_str(), detail.as_str()))
                }
                _ => None,
            })
            .collect()
    }

    fn chat(content: &str) -> KodiakResult {
        KodiakResult::Chat {
            content: content.into(),
        }
    }

    fn at(start: DateTime<Utc>, index: usize) -> Result<DateTime<Utc>> {
        Ok(start + TimeDelta::milliseconds(i64::try_from(index * FRAME_MS)?))
    }

    /// Pushes voice-band frames that are open during `bursts`.
    fn push_voice(ingress: &mut Ingress, frames: usize, bursts: &[Range<usize>]) -> Result<()> {
        let start = Utc::now();
        for index in 0..frames {
            let open = bursts.iter().any(|burst| burst.contains(&index));
            let audio = [if open { 0.15 } else { 0.0 }; AUDIO_FRAME_LEN];
            ingress.push(open, &audio, &[0.0; IF_FRAME_LEN], at(start, index)?);
        }
        Ok(())
    }

    fn wait_for(stop: &AtomicBool) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while !stop.load(Ordering::Relaxed) {
            anyhow::ensure!(Instant::now() < deadline, "the stop flag was not set");
            thread::sleep(Duration::from_millis(10));
        }
        Ok(())
    }

    #[test]
    fn a_skip_reaches_both_lanes_and_speech_is_not_attributed_across_it() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let mut signal = vec![0.0; IF_FRAME_LEN * 195];
        let r4 = packet(*b"R4")?;
        signal[43 * IF_FRAME_LEN..43 * IF_FRAME_LEN + r4.len()].copy_from_slice(&r4);
        let r4_end = 43 + r4.len().div_ceil(IF_FRAME_LEN);
        let start = Utc::now();
        let stop = AtomicBool::new(false);
        run(
            &output,
            Source::Wav,
            TIMING,
            Some(|clip: &Clip| {
                assert!(clip.first_frame >= 35, "a clip spans the gap");
                Ok(chat("Test voice"))
            }),
            Some(bss::Decoder::new(IF_RATE, Source::Wav, false, None)?),
            &stop,
            |ingress| {
                for (index, frame) in signal.chunks_exact(IF_FRAME_LEN).enumerate() {
                    // Ten blocks are lost upstream in the middle of the speech.
                    if index == 25 {
                        ingress.skip(10);
                    }
                    let open = (5..r4_end).contains(&index);
                    let voice = [if open { 0.15 } else { 0.0 }; AUDIO_FRAME_LEN];
                    let shifted = if index < 25 { index } else { index + 10 };
                    ingress.push(open, &voice, frame, at(start, shifted)?);
                }
                Ok(())
            },
        )?;
        let records = recorded(&output)?;
        let failures = failures(&records);
        for stage in ["voice_capture", "bss"] {
            assert!(
                failures.contains(&(stage, "dropped input blocks 25..35")),
                "{failures:?}"
            );
        }
        // The clip open at the gap is dropped with it. Speech after the gap
        // is followed by the packet with no gap in between.
        let transcripts: Vec<_> = records
            .iter()
            .filter_map(|event| match &event.kind {
                Kind::Transcript {
                    radio_id,
                    started_at,
                    ..
                } => Some((*started_at, radio_id.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(transcripts, [(at(start, 35)?, Some("R4"))]);
        assert!(!stop.load(Ordering::Relaxed));
        Ok(())
    }

    #[test]
    fn a_fatal_bss_failure_stops_a_live_run() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let stop = AtomicBool::new(false);
        let result = run(
            &output,
            Source::Live,
            TIMING,
            None::<fn(&Clip) -> Result<KodiakResult>>,
            Some(bss::Decoder::new(IF_RATE, Source::Live, false, None)?),
            &stop,
            |ingress| {
                let quiet = [0.0; AUDIO_FRAME_LEN];
                ingress.push(false, &quiet, &[0.0; IF_FRAME_LEN], Utc::now());
                // No sample offset exists for this block: the decoder fails.
                ingress.skip(u64::MAX / 2);
                ingress.push(false, &quiet, &[0.0; IF_FRAME_LEN], Utc::now());
                // As the radio loop does, run until told to stop.
                wait_for(&stop)
            },
        );
        let error = format!("{:#}", result.expect_err("a fatal BSS failure"));
        assert!(
            error.contains("bss: BSS block index is out of range"),
            "{error}"
        );
        assert!(stop.load(Ordering::Relaxed));
        let records = recorded(&output)?;
        assert!(
            failures(&records)
                .iter()
                .any(|(stage, detail)| *stage == "bss" && detail.contains("out of range"))
        );
        Ok(())
    }

    #[test]
    fn a_failed_bss_save_does_not_stop_decoding() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let directory = temporary.path().join("events");
        let output = Output::new(directory.clone(), 435_995_000, false)?;
        let (r4, r5) = (packet(*b"R4")?, packet(*b"R5")?);
        let r4_end = 43 + r4.len().div_ceil(IF_FRAME_LEN);
        // The decoder has finished a block once a full queue behind it is pushed.
        let restored = r4_end + AUDIO_QUEUE + 10;
        let r5_start = restored + 10;
        let mut signal = vec![0.0; IF_FRAME_LEN * (r5_start + 100)];
        signal[43 * IF_FRAME_LEN..43 * IF_FRAME_LEN + r4.len()].copy_from_slice(&r4);
        let r5_at = r5_start * IF_FRAME_LEN;
        signal[r5_at..r5_at + r5.len()].copy_from_slice(&r5);
        fs::remove_dir(&directory)?;
        let start = Utc::now();
        let stop = AtomicBool::new(false);
        run(
            &output,
            Source::Wav,
            TIMING,
            None::<fn(&Clip) -> Result<KodiakResult>>,
            Some(bss::Decoder::new(IF_RATE, Source::Wav, false, None)?),
            &stop,
            |ingress| {
                for (index, frame) in signal.chunks_exact(IF_FRAME_LEN).enumerate() {
                    if index == restored {
                        fs::create_dir(&directory)?;
                    }
                    let quiet = [0.0; AUDIO_FRAME_LEN];
                    ingress.push(false, &quiet, frame, at(start, index)?);
                }
                Ok(())
            },
        )?;
        assert!(!stop.load(Ordering::Relaxed));
        let records = recorded(&output)?;
        let positions: Vec<_> = records
            .iter()
            .filter_map(|event| match &event.kind {
                Kind::Position { radio_id, .. } => Some(radio_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(positions, ["R5"]);
        Ok(())
    }

    #[test]
    fn recoverable_bss_failures_are_recorded_and_lane_errors_are_fatal() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let stop = AtomicBool::new(false);
        let mut coordinator = Coordinator::new(&output, Source::Live, 50, 15, false, true, &stop);
        coordinator.event(Event::DataProgress {
            through: 1,
            packets: Vec::new(),
            masks: VecDeque::new(),
            failures: vec!["BSS event could not be saved: disk full".into()],
        });
        assert!(!stop.load(Ordering::Relaxed));
        assert!(coordinator.fatal.is_none());
        coordinator.event(Event::Done(Lane::Data, Some("decoder failed".into())));
        assert!(stop.load(Ordering::Relaxed));
        assert_eq!(coordinator.fatal.as_deref(), Some("bss: decoder failed"));
        let records = recorded(&output)?;
        let mut failures = failures(&records);
        failures.sort_unstable();
        assert_eq!(
            failures,
            [
                ("bss", "BSS event could not be saved: disk full"),
                ("bss", "decoder failed")
            ]
        );
        Ok(())
    }

    #[test]
    fn a_recognizer_panic_fails_only_its_own_clip() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let stop = AtomicBool::new(false);
        let mut calls = 0;
        run(
            &output,
            Source::Wav,
            TIMING,
            Some(|_: &Clip| {
                calls += 1;
                assert!(calls > 1, "test transcriber panic");
                Ok(chat("Second clip"))
            }),
            None,
            &stop,
            |ingress| push_voice(ingress, 150, &[5..30, 60..85]),
        )?;
        assert!(!stop.load(Ordering::Relaxed));
        let records = recorded(&output)?;
        let failures = failures(&records);
        assert_eq!(failures.len(), 1, "{failures:?}");
        assert!(failures[0].0 == "asr" && failures[0].1.contains("test transcriber panic"));
        let transcripts: Vec<_> = records
            .iter()
            .filter_map(|event| match &event.kind {
                Kind::Transcript { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(transcripts, ["Second clip"]);
        Ok(())
    }

    #[test]
    fn a_closed_recognition_queue_waits_for_the_workers_reason() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let stop = AtomicBool::new(false);
        let mut coordinator = Coordinator::new(&output, Source::Wav, 50, 15, true, false, &stop);
        let (jobs, work) = sync_channel(ASR_QUEUE);
        // The worker has closed its queue but not yet reported why.
        drop(work);
        let now = Utc::now();
        coordinator.waiting.push_back((
            Clip {
                started_at: now,
                ended_at: now,
                first_frame: 0,
                signal_first_frame: 0,
                signal_last_frame: 19,
                audio: vec![0.1; AUDIO_FRAME_LEN * 20],
            },
            20,
        ));
        coordinator.event(Event::Done(Lane::Voice, None));
        coordinator.dispatch(&jobs);
        assert!(coordinator.pending.is_empty());
        assert!(!coordinator.drained(), "drained before the worker's error");
        coordinator.event(Event::Done(
            Lane::Asr,
            Some("recognition panicked on 3 consecutive clips".into()),
        ));
        assert!(coordinator.drained());
        assert!(coordinator.fatal.is_some());
        Ok(())
    }

    #[test]
    fn consecutive_recognizer_panics_are_fatal() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let output = Output::new(temporary.path().to_owned(), 435_995_000, false)?;
        let stop = AtomicBool::new(false);
        let result = run(
            &output,
            Source::Wav,
            TIMING,
            Some(|_: &Clip| -> Result<KodiakResult> { panic!("test transcriber panic") }),
            None,
            &stop,
            |ingress| push_voice(ingress, 230, &[5..30, 60..85, 115..140, 170..195]),
        );
        let error = format!("{:#}", result.expect_err("repeated panics are fatal"));
        assert!(
            error.contains("asr: recognition panicked on 3 consecutive clips"),
            "{error}"
        );
        assert!(stop.load(Ordering::Relaxed));
        // Every clip is accounted for: three panics and one refused clip.
        let records = recorded(&output)?;
        let failures = failures(&records);
        assert_eq!(
            failures
                .iter()
                .filter(|(_, detail)| detail.contains("test transcriber panic"))
                .count(),
            3
        );
        assert_eq!(
            failures.iter().filter(|(stage, _)| *stage == "asr").count(),
            5,
            "{failures:?}"
        );
        Ok(())
    }

    #[test]
    fn a_coordinator_panic_sets_the_stop_flag_without_waiting_for_input() -> Result<()> {
        let stop = AtomicBool::new(false);
        thread::scope(|scope| {
            let worker = scope.spawn(|| supervised(&stop, || panic!("coordinator test panic")));
            // The input side only learns of the panic through the flag.
            wait_for(&stop)?;
            let fatal = worker.join().map_err(|_| anyhow!("the panic escaped"))?;
            assert!(fatal.is_some_and(|error| error.contains("coordinator test panic")));
            Ok(())
        })
    }
}
