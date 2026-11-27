//! The real-time looper core. Owns all audio state; driven block by block by the output
//! thread. No locks on the hot path except a `try_lock` to publish status.

use super::denoise::Gate;
use super::dsp::rms;
use crossbeam_channel::{Receiver, Sender};
use std::sync::{Arc, Mutex};

pub const SR: u32 = 48000;
const RING_LEN: usize = SR as usize; // 1 s input history (pre-roll + noise sample)
const DETECT_WIN: usize = 256;
const SEAM_XFADE: usize = 240; // 5 ms

pub type Buffer = Arc<Vec<f32>>;

pub enum Command {
    SetPlaying(bool),
    /// Arm the first take (defines the loop). `buffer` is an empty Vec with large capacity,
    /// `noise` an empty Vec with capacity ≥ RING_LEN.
    ArmFirst {
        track_id: u32,
        buffer: Vec<f32>,
        noise: Vec<f32>,
    },
    /// Arm an overdub of `mult` loop lengths; `buffer` is pre-filled with zeros of that length.
    ArmOverdub {
        track_id: u32,
        buffer: Vec<f32>,
        noise: Vec<f32>,
        replace: bool,
    },
    /// Finish the current take (first take: closes the loop; overdub: stops early).
    StopRecording,
    CancelRecording,
    ReplaceBuffer {
        id: u32,
        buf: Buffer,
    },
    SetTrack {
        id: u32,
        gain: f32,
        mute: bool,
        solo: bool,
    },
    RemoveTrack(u32),
    ClearAll,
    SetMetronome(MetronomeSettings),
    SetThreshold(f32),
    SetMonitoring {
        enabled: bool,
        gate: bool,
    },
    SetLatencyOffset(i32),
    SetTiming {
        preroll: usize,
        tail: usize,
    },
    CaptureNoise(Vec<f32>),
    /// Add a ready-made track (demo / import). Defines the loop if there is none yet.
    Inject {
        id: u32,
        buf: Buffer,
    },
    /// Arrangement for one track: per loop repetition the repetition its clip starts at
    /// (or −1 = silent). `None` = loop everywhere.
    SetTrackMap {
        id: u32,
        map: Option<RepMap>,
        /// Volume per repetition (fade in / out); `None` = full volume.
        env: Option<Envelope>,
        /// Filter curve over the song (`FILTER_RES` points per repetition); `None` = open.
        filter: Option<Arc<Vec<f32>>>,
    },
    /// Song length in loop repetitions; 0 = automatic (just loop the tracks).
    SetSongReps(u32),
    /// Jump to a song position (samples).
    Seek(u64),
    /// Play drum `n` of the kit now (live drum pads).
    /// Play a drum now. With `snap` (note value 1/N, 0 = off) and a playing loop, an early hit
    /// is held back to the next grid point so it already sounds in time.
    DrumHit {
        drum: u8,
        snap: u8,
    },
    /// Global speed change: new loop length and beat, and every track's audio rendered for it
    /// (all at once, so lengths never mismatch). The play position keeps its place in the song.
    Retime {
        loop_len: usize,
        beat_len: f64,
        bufs: Vec<(u32, Buffer)>,
    },
    /// Replace everything with a loaded project (starts paused).
    Restore {
        loop_len: usize,
        beat_len: f64,
        tracks: Vec<RestoreTrack>,
    },
}

pub type RepMap = Arc<Vec<i32>>;
/// Per loop repetition: volume at its start and at its end (linear in between).
pub type Envelope = Arc<Vec<(f32, f32)>>;

/// Volume of an envelope at song position `pos`.
#[inline]
pub fn env_gain(pos: u64, loop_len: usize, env: &[(f32, f32)]) -> f32 {
    let l = loop_len.max(1) as u64;
    match env.get((pos / l) as usize) {
        Some(&(a, b)) => a + (b - a) * ((pos % l) as f32 / l as f32),
        None => 1.0,
    }
}

pub struct RestoreTrack {
    pub id: u32,
    pub buf: Buffer,
    pub gain: f32,
    pub mute: bool,
    pub solo: bool,
    pub map: Option<RepMap>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct MetronomeSettings {
    pub enabled: bool,
    pub bpm: f32,
    pub beats_per_bar: u32,
    pub volume: f32,
}

pub enum Event {
    /// Threshold crossed – the take is now actually recording.
    RecordingStarted,
    /// First take finished: loop defined.
    LoopCreated {
        track_id: u32,
        raw: Buffer,
        noise: Vec<f32>,
        info: LoopInfo,
    },
    /// `onset` is the song position (samples) where the take's playing began.
    OverdubDone {
        track_id: u32,
        raw: Buffer,
        noise: Vec<f32>,
        replaced: bool,
        onset: u64,
    },
    /// Take was empty (no signal) and got discarded.
    TakeDiscarded,
    NoiseSample(Vec<f32>),
    /// A drum pad was hit; `pos` is the song position the player heard at that moment.
    DrumHit {
        drum: u8,
        pos: Option<u64>,
    },
    /// Old buffers handed back so they are freed off the audio thread.
    Garbage(Vec<Buffer>),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LoopInfo {
    pub len: usize,
    /// Set when the loop was quantised to the metronome.
    pub bars: Option<u32>,
    /// Tempo that fits the loop (beats are an integer division of the loop).
    pub bpm: f32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum RecState {
    #[default]
    Idle,
    ArmedFirst,
    RecordingFirst,
    ArmedOverdub,
    RecordingOverdub,
}

#[derive(Clone, Debug, Default)]
pub struct Status {
    pub rec: RecState,
    pub rec_track: Option<u32>,
    pub playing: bool,
    pub loop_len: usize,
    /// Length of one pass through the song (multiple of `loop_len`).
    pub song_len: usize,
    pub pos: u64,
    /// Samples recorded in the current take.
    pub rec_samples: usize,
    /// Overdub target length (samples).
    pub rec_target: usize,
    pub input_peak: f32,
    pub input_rms: f32,
    pub beat: Option<(u32, u32)>,
    pub latency: usize,
    pub threshold: f32,
}

struct Track {
    id: u32,
    buf: Buffer,
    gain: f32,
    mute: bool,
    solo: bool,
    map: Option<RepMap>,
    env: Option<Envelope>,
    filter: Option<Arc<Vec<f32>>>,
    filter_state: super::dsp::TrackFilter,
    /// One block of this track, rendered ahead when it has a filter.
    scratch: Vec<f32>,
}

impl Track {
    fn new(id: u32, buf: Buffer) -> Self {
        Self {
            id,
            buf,
            gain: 1.0,
            mute: false,
            solo: false,
            map: None,
            env: None,
            filter: None,
            filter_state: Default::default(),
            scratch: Vec::with_capacity(4096),
        }
    }
}

/// Index into a track buffer for song position `pos`, or `None` when the track is silent there.
#[inline]
pub fn track_index(pos: u64, loop_len: usize, buf_len: usize, map: Option<&[i32]>) -> Option<usize> {
    track_slot(pos, loop_len, buf_len, map).map(|(i, _)| i)
}

/// Like [`track_index`], plus whether this is the wrapped-around end of a clip that started near
/// the end of the song and continues at its beginning (the song loops, so clips may wrap).
#[inline]
pub fn track_slot(pos: u64, loop_len: usize, buf_len: usize, map: Option<&[i32]>) -> Option<(usize, bool)> {
    if buf_len == 0 || loop_len == 0 {
        return None;
    }
    match map {
        None => Some(((pos % buf_len as u64) as usize, false)),
        Some(m) => {
            let rep = (pos / loop_len as u64) as usize;
            let start = *m.get(rep)?;
            if start < 0 {
                return None;
            }
            let start = start as usize;
            let wrapped = start > rep;
            let reps_in = if wrapped { rep + m.len() - start } else { rep - start };
            let idx = reps_in * loop_len + (pos % loop_len as u64) as usize;
            (idx < buf_len).then_some((idx, wrapped))
        }
    }
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

enum Rec {
    Idle,
    ArmedFirst { id: u32, buf: Vec<f32>, noise: Vec<f32>, armed: usize },
    First { id: u32, buf: Vec<f32>, noise: Vec<f32>, onset: usize, origin_clock: u64, last_loud: usize },
    ArmedOverdub { id: u32, buf: Vec<f32>, noise: Vec<f32>, armed: usize, replace: bool },
    Overdub { id: u32, buf: Vec<f32>, noise: Vec<f32>, start: usize, written: usize, replace: bool, onset: u64 },
}

pub struct Looper {
    cmd_rx: Receiver<Command>,
    ev_tx: Sender<Event>,
    status: Arc<Mutex<Status>>,

    tracks: Vec<Track>,
    loop_len: usize,
    song_reps: u32,
    song_len: usize,
    limiter_gain: f32,
    /// Position since loop start (samples), keeps counting across cycles.
    abs: u64,
    playing: bool,
    clock: u64,
    rec: Rec,

    ring: Vec<f32>,
    ring_pos: usize,

    metro: MetronomeSettings,
    metro_origin: u64,
    /// Beat length while a loop exists (fitted to the loop).
    loop_beat_len: f64,
    click_phase: usize,
    click_accent: bool,

    threshold: f32,
    monitor: bool,
    gate_on: bool,
    gate: Gate,
    latency_offset: i32,
    preroll: usize,
    tail: usize,

    kit: Arc<super::drums::Kit>,
    /// Active drum voices: (drum, sample index; negative = samples until it starts).
    voices: Vec<(u8, i64)>,
    pending_hits: Vec<(u8, u8)>,
    peak: f32,
    rms_acc: f32,
    rms_n: usize,
    garbage: Vec<Buffer>,
}

impl Looper {
    pub fn new(cmd_rx: Receiver<Command>, ev_tx: Sender<Event>, status: Arc<Mutex<Status>>) -> Self {
        Self {
            cmd_rx,
            ev_tx,
            status,
            tracks: Vec::with_capacity(128),
            loop_len: 0,
            song_reps: 0,
            song_len: 0,
            limiter_gain: 1.0,
            abs: 0,
            playing: false,
            clock: 0,
            rec: Rec::Idle,
            ring: vec![0.0; RING_LEN],
            ring_pos: 0,
            metro: MetronomeSettings { enabled: false, bpm: 100.0, beats_per_bar: 4, volume: 0.6 },
            metro_origin: 0,
            loop_beat_len: 0.0,
            click_phase: usize::MAX,
            click_accent: false,
            threshold: 0.02,
            monitor: true,
            gate_on: true,
            gate: Gate::new(SR as f32),
            latency_offset: 0,
            preroll: (0.03 * SR as f32) as usize,
            tail: (0.15 * SR as f32) as usize,
            kit: super::drums::kit(),
            voices: Vec::with_capacity(32),
            pending_hits: Vec::with_capacity(16),
            peak: 0.0,
            rms_acc: 0.0,
            rms_n: 0,
            garbage: Vec::with_capacity(64),
        }
    }

    fn beat_len(&self) -> f64 {
        if self.loop_len > 0 && self.loop_beat_len > 0.0 {
            self.loop_beat_len
        } else {
            60.0 / self.metro.bpm.max(20.0) as f64 * SR as f64
        }
    }

    fn handle_commands(&mut self) {
        while let Ok(cmd) = self.cmd_rx.try_recv() {
            match cmd {
                Command::SetPlaying(p) => {
                    if self.loop_len > 0 {
                        self.playing = p;
                    }
                }
                Command::ArmFirst { track_id, buffer, noise } => {
                    self.cancel();
                    if self.loop_len == 0 {
                        self.rec = Rec::ArmedFirst { id: track_id, buf: buffer, noise, armed: 0 };
                    }
                }
                Command::ArmOverdub { track_id, buffer, noise, replace } => {
                    self.cancel();
                    if self.loop_len > 0 && !buffer.is_empty() {
                        self.playing = true;
                        self.rec = Rec::ArmedOverdub { id: track_id, buf: buffer, noise, armed: 0, replace };
                    }
                }
                Command::StopRecording => self.stop_recording(),
                Command::CancelRecording => self.cancel(),
                Command::ReplaceBuffer { id, buf } => {
                    if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                        let old = std::mem::replace(&mut t.buf, buf);
                        self.garbage.push(old);
                    } else {
                        self.garbage.push(buf);
                    }
                }
                Command::SetTrack { id, gain, mute, solo } => {
                    if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                        t.gain = gain;
                        t.mute = mute;
                        t.solo = solo;
                    }
                }
                Command::RemoveTrack(id) => {
                    if let Some(i) = self.tracks.iter().position(|t| t.id == id) {
                        let t = self.tracks.remove(i);
                        self.garbage.push(t.buf);
                    }
                    if self.tracks.is_empty() && !matches!(self.rec, Rec::ArmedOverdub { .. } | Rec::Overdub { .. }) {
                        self.reset_loop();
                    }
                }
                Command::ClearAll => {
                    self.cancel();
                    for t in self.tracks.drain(..) {
                        self.garbage.push(t.buf);
                    }
                    self.reset_loop();
                }
                Command::SetMetronome(m) => {
                    if m.enabled && !self.metro.enabled {
                        self.metro_origin = self.clock;
                    }
                    if self.loop_len > 0 && (m.bpm - self.metro.bpm).abs() > 0.01 {
                        // tempo was changed by the user: refit beats to the loop
                        self.loop_beat_len = fit_beat_len(self.loop_len, m.bpm);
                    }
                    self.metro = m;
                }
                Command::SetThreshold(t) => {
                    self.threshold = t;
                    self.gate.set_threshold(t * 0.7);
                }
                Command::SetMonitoring { enabled, gate } => {
                    self.monitor = enabled;
                    self.gate_on = gate;
                }
                Command::SetLatencyOffset(o) => self.latency_offset = o,
                Command::SetTiming { preroll, tail } => {
                    self.preroll = preroll.min(RING_LEN / 2);
                    self.tail = tail;
                }
                Command::Inject { id, buf } => {
                    if self.loop_len == 0 {
                        self.loop_len = buf.len();
                        self.loop_beat_len = fit_beat_len(buf.len(), self.metro.bpm);
                        self.abs = 0;
                        self.playing = true;
                        let info = LoopInfo {
                            len: buf.len(),
                            bars: None,
                            bpm: (60.0 * SR as f64 / self.loop_beat_len) as f32,
                        };
                        let _ = self.ev_tx.try_send(Event::LoopCreated {
                            track_id: id,
                            raw: buf.clone(),
                            noise: Vec::new(),
                            info,
                        });
                    } else {
                        let _ = self.ev_tx.try_send(Event::OverdubDone {
                            track_id: id,
                            raw: buf.clone(),
                            noise: Vec::new(),
                            replaced: false,
                            onset: 0,
                        });
                    }
                    self.tracks.push(Track::new(id, buf));
                }
                Command::SetTrackMap { id, map, env, filter } => {
                    if let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
                        t.map = map;
                        t.env = env;
                        t.filter = filter;
                    }
                }
                Command::SetSongReps(r) => self.song_reps = r,
                Command::DrumHit { drum, snap } => {
                    if (drum as usize) < self.kit.len() && self.pending_hits.len() < self.pending_hits.capacity() {
                        self.pending_hits.push((drum, snap));
                    }
                }
                Command::Seek(p) => {
                    if self.loop_len > 0 && !matches!(self.rec, Rec::Overdub { .. }) {
                        self.update_song_len();
                        self.abs = p % self.song_len.max(1) as u64;
                    }
                }
                Command::Retime { loop_len, beat_len, bufs } => {
                    if self.loop_len == 0 || loop_len == 0 {
                        self.garbage.extend(bufs.into_iter().map(|(_, b)| b));
                        continue;
                    }
                    self.cancel();
                    let old = self.loop_len as f64;
                    self.abs = (self.abs as f64 * loop_len as f64 / old) as u64;
                    self.loop_len = loop_len;
                    self.loop_beat_len = beat_len;
                    self.metro.bpm = (60.0 * SR as f64 / beat_len) as f32;
                    for (id, buf) in bufs {
                        match self.tracks.iter_mut().find(|t| t.id == id) {
                            Some(t) => {
                                let old = std::mem::replace(&mut t.buf, buf);
                                self.garbage.push(old);
                            }
                            None => self.garbage.push(buf),
                        }
                    }
                    self.update_song_len();
                    self.abs %= self.song_len.max(1) as u64;
                }
                Command::Restore { loop_len, beat_len, tracks } => {
                    self.cancel();
                    for t in self.tracks.drain(..) {
                        self.garbage.push(t.buf);
                    }
                    for t in tracks {
                        self.tracks.push(Track {
                            id: t.id,
                            buf: t.buf,
                            gain: t.gain,
                            mute: t.mute,
                            solo: t.solo,
                            map: t.map,
                            env: None,
                            filter: None,
                            filter_state: Default::default(),
                            scratch: Vec::with_capacity(4096),
                        });
                    }
                    self.loop_len = loop_len;
                    self.loop_beat_len = beat_len;
                    self.abs = 0;
                    self.playing = false;
                }
                Command::CaptureNoise(mut v) => {
                    v.clear();
                    self.copy_ring_tail(&mut v, RING_LEN, 0);
                    let _ = self.ev_tx.try_send(Event::NoiseSample(v));
                }
            }
        }
    }

    /// Song length: explicit repetitions, or the least common multiple of all track lengths.
    fn update_song_len(&mut self) {
        if self.loop_len == 0 {
            self.song_len = 0;
            return;
        }
        let l = self.loop_len as u64;
        let reps = if self.song_reps > 0 {
            self.song_reps as u64
        } else {
            let mut lcm = 1u64;
            for t in &self.tracks {
                let k = (t.buf.len() as u64).div_ceil(l).max(1);
                let next = lcm / gcd(lcm, k) * k;
                lcm = if next > 64 { lcm.max(k) } else { next };
            }
            lcm
        };
        self.song_len = (reps * l) as usize;
    }

    fn reset_loop(&mut self) {
        self.loop_len = 0;
        self.abs = 0;
        self.playing = false;
        self.loop_beat_len = 0.0;
        self.metro_origin = self.clock;
    }

    fn cancel(&mut self) {
        let rec = std::mem::replace(&mut self.rec, Rec::Idle);
        if let Rec::ArmedOverdub { .. } | Rec::Overdub { .. } = rec
            && self.tracks.is_empty()
        {
            self.reset_loop();
        }
        drop_off_thread(rec, &self.ev_tx);
    }

    /// Append the last `n` ring samples (excluding the newest `skip_newest`) to `out`.
    fn copy_ring_tail(&self, out: &mut Vec<f32>, n: usize, skip_newest: usize) {
        let n = n.min(RING_LEN.saturating_sub(skip_newest));
        let end = (self.ring_pos + RING_LEN - skip_newest) % RING_LEN;
        let start = (end + RING_LEN - n) % RING_LEN;
        for i in 0..n {
            out.push(self.ring[(start + i) % RING_LEN]);
        }
    }

    fn stop_recording(&mut self) {
        match std::mem::replace(&mut self.rec, Rec::Idle) {
            Rec::First { id, mut buf, noise, onset, origin_clock, last_loud } => {
                // `buf[0]` corresponds to `origin_clock`; the current clock is the end.
                let end = (last_loud + self.tail).min(buf.len());
                let metro = self.metro.enabled.then(|| (self.metro_origin, self.beat_len(), self.metro.beats_per_bar));
                let Some(cut) = plan_first_take(onset, end, buf.len(), origin_clock, self.preroll, metro) else {
                    let _ = self.ev_tx.try_send(Event::TakeDiscarded);
                    drop_off_thread(Rec::ArmedFirst { id, buf, noise, armed: 0 }, &self.ev_tx);
                    return;
                };
                // build the loop in-place: shift, pad, seam crossfade with audio before the start
                let mut pre = [0f32; SEAM_XFADE];
                for (i, p) in pre.iter_mut().enumerate() {
                    let idx = cut.start as isize - SEAM_XFADE as isize + i as isize;
                    if idx >= 0 && (idx as usize) < buf.len() {
                        *p = buf[idx as usize];
                    }
                }
                buf.drain(..cut.start.min(buf.len()));
                buf.resize(cut.len, 0.0); // within capacity → no allocation in practice
                crossfade_seam(&mut buf, &pre);
                self.loop_len = cut.len;
                self.loop_beat_len = cut.beat_len;
                // free mode: start from the top now; metronome mode: stay on the grid
                self.abs = match cut.grid_start_clock {
                    Some(c) => self.clock.saturating_sub(c) % cut.len as u64,
                    None => 0,
                };
                self.playing = true;
                let raw: Buffer = Arc::new(buf);
                self.tracks.push(Track::new(id, raw.clone()));
                let info = LoopInfo { len: cut.len, bars: cut.bars, bpm: (60.0 * SR as f64 / cut.beat_len) as f32 };
                let _ = self.ev_tx.try_send(Event::LoopCreated { track_id: id, raw, noise, info });
            }
            Rec::Overdub { id, buf, noise, written, replace, onset, .. } => {
                if written < DETECT_WIN {
                    let _ = self.ev_tx.try_send(Event::TakeDiscarded);
                    drop_off_thread(Rec::Overdub { id, buf, noise, start: 0, written, replace, onset }, &self.ev_tx);
                } else {
                    self.finish_overdub(id, buf, noise, replace, onset);
                }
            }
            Rec::ArmedFirst { .. } | Rec::ArmedOverdub { .. } | Rec::Idle => {
                // nothing played yet – just disarm
                let _ = self.ev_tx.try_send(Event::TakeDiscarded);
            }
        }
    }

    fn finish_overdub(&mut self, id: u32, buf: Vec<f32>, noise: Vec<f32>, replace: bool, onset: u64) {
        let raw: Buffer = Arc::new(buf);
        let mut replaced = false;
        if replace && let Some(t) = self.tracks.iter_mut().find(|t| t.id == id) {
            let old = std::mem::replace(&mut t.buf, raw.clone());
            self.garbage.push(old);
            replaced = true;
        }
        if !replaced {
            self.tracks.push(Track::new(id, raw.clone()));
        }
        let _ = self.ev_tx.try_send(Event::OverdubDone { track_id: id, raw, noise, replaced, onset });
    }

    /// Process one block. `input` is mono, `out` is interleaved stereo (len = 2 * input.len()).
    /// `out_latency` is the delay until written audio is heard (used to timestamp drum hits).
    pub fn process(&mut self, input: &[f32], out: &mut [f32], latency: usize, out_latency: usize) {
        self.handle_commands();
        let n = input.len();
        debug_assert_eq!(out.len(), n * 2);

        // ---- input analysis + history ring
        let block_rms = rms(input);
        for &x in input {
            self.peak = self.peak.max(x.abs());
            self.ring[self.ring_pos] = x;
            self.ring_pos = (self.ring_pos + 1) % RING_LEN;
        }
        self.rms_acc += block_rms * block_rms * n as f32;
        self.rms_n += n;
        let loud = block_rms > self.threshold;

        self.update_song_len();
        let song_len = self.song_len.max(1) as u64;

        // ---- drum pads: start voices, report the heard position for pattern recording
        let hit_beat = self.beat_len();
        for i in 0..self.pending_hits.len() {
            let (d, snap) = self.pending_hits[i];
            let pos = (self.loop_len > 0 && self.playing)
                .then(|| (self.abs as i64 - out_latency as i64).rem_euclid(song_len as i64) as u64);
            // early hits wait for the grid point they will be snapped to
            let wait = match (pos, super::drums::grid_len(snap, hit_beat)) {
                (Some(p), Some(g)) => drum_wait(p, g, out_latency as u64),
                _ => 0,
            };
            if self.voices.len() < self.voices.capacity() {
                self.voices.push((d, -(wait as i64)));
            }
            let _ = self.ev_tx.try_send(Event::DrumHit { drum: d, pos });
        }
        self.pending_hits.clear();

        // ---- recording state machine
        let block_abs = self.abs;
        let lat = (latency as i64 + self.latency_offset as i64).max(0) as u64;
        let mut started: Option<u32> = None;
        let mut overdub_finished = false;
        match &mut self.rec {
            Rec::Idle => {}
            Rec::ArmedFirst { id, buf, noise, armed } => {
                *armed += n;
                if loud {
                    // onset: keep the whole 1 s history (needed for grid snapping & seam fade)
                    let hist = RING_LEN.min(*armed);
                    let id = *id;
                    let mut b = std::mem::take(buf);
                    let mut nz = std::mem::take(noise);
                    let guard = (0.1 * SR as f32) as usize + n;
                    let nlen = hist.saturating_sub(guard);
                    let (ring, rp) = (&self.ring, self.ring_pos);
                    copy_ring(ring, rp, &mut nz, nlen, guard);
                    copy_ring(ring, rp, &mut b, hist, 0);
                    let onset = b.len() - n;
                    let origin_clock = self.clock + n as u64 - b.len() as u64;
                    self.rec = Rec::First { id, buf: b, noise: nz, onset, origin_clock, last_loud: onset + n };
                    started = Some(id);
                }
            }
            Rec::First { buf, last_loud, .. } => {
                if buf.len() + n <= buf.capacity() {
                    buf.extend_from_slice(input);
                    if loud {
                        *last_loud = buf.len();
                    }
                }
            }
            Rec::ArmedOverdub { id, buf, noise, armed, replace } => {
                *armed += n;
                if loud {
                    let len = buf.len();
                    let id = *id;
                    let replace = *replace;
                    let mut b = std::mem::take(buf);
                    let mut nz = std::mem::take(noise);
                    let guard = (0.1 * SR as f32) as usize + n;
                    copy_ring(&self.ring, self.ring_pos, &mut nz, RING_LEN.min(*armed).saturating_sub(guard), guard);
                    // the first sample of this block was heard at loop position (abs - lat)
                    let pre = self.preroll.min(RING_LEN - n);
                    let onset_pos = (block_abs as i64 - lat as i64 - pre as i64).rem_euclid(song_len as i64) as u64;
                    let start = (onset_pos % len as u64) as usize;
                    let mut written = 0;
                    let rs = (self.ring_pos + RING_LEN - n - pre) % RING_LEN;
                    for i in 0..pre + n {
                        b[(start + written) % len] = self.ring[(rs + i) % RING_LEN];
                        written += 1;
                    }
                    self.rec = Rec::Overdub { id, buf: b, noise: nz, start, written, replace, onset: onset_pos };
                    started = Some(id);
                }
            }
            Rec::Overdub { buf, start, written, .. } => {
                let len = buf.len();
                for &x in input {
                    if *written < len {
                        buf[(*start + *written) % len] = x;
                    } else {
                        // seam crossfade: blend the overshoot into the beginning of the take
                        let j = *written - len;
                        if j >= SEAM_XFADE {
                            overdub_finished = true;
                            break;
                        }
                        let a = j as f32 / SEAM_XFADE as f32;
                        let idx = (*start + j) % len;
                        buf[idx] = buf[idx] * a + x * (1.0 - a);
                    }
                    *written += 1;
                }
                if *written >= len + SEAM_XFADE {
                    overdub_finished = true;
                }
            }
        }
        if let Some(id) = started {
            let _ = (id, self.ev_tx.try_send(Event::RecordingStarted));
        }
        if overdub_finished
            && let Rec::Overdub { id, buf, noise, replace, onset, .. } = std::mem::replace(&mut self.rec, Rec::Idle)
        {
            self.finish_overdub(id, buf, noise, replace, onset);
        }

        // ---- mix
        let any_solo = self.tracks.iter().any(|t| t.solo);
        let recording_first = matches!(self.rec, Rec::ArmedFirst { .. } | Rec::First { .. });
        let metro_on = self.metro.enabled && (self.playing || self.loop_len == 0);
        let beat_len = self.beat_len();
        let bpb = self.metro.beats_per_bar.max(1) as u64;
        // tracks with a filter curve: render this block ahead and filter it as a whole
        let playing_loop = self.playing && self.loop_len > 0;
        if playing_loop {
            let (abs0, l) = (self.abs, self.loop_len);
            for t in self.tracks.iter_mut() {
                let Some(curve) = t.filter.clone() else { continue };
                t.scratch.clear();
                for k in 0..n as u64 {
                    let a = (abs0 + k) % song_len;
                    let v = match track_index(a, l, t.buf.len(), t.map.as_deref().map(|v| v.as_slice())) {
                        Some(i) => t.buf[i] * t.env.as_deref().map_or(1.0, |e| env_gain(a, l, e)),
                        None => 0.0,
                    };
                    t.scratch.push(v);
                }
                let target = super::dsp::filter_at(&curve, abs0, l);
                t.filter_state.process(&mut t.scratch, target, SR as f32);
            }
        }
        for i in 0..n {
            let mut s = 0.0f32;
            if playing_loop {
                let a = self.abs;
                for t in &self.tracks {
                    if t.mute || (any_solo && !t.solo) {
                        continue;
                    }
                    if t.filter.is_some() {
                        s += t.scratch.get(i).copied().unwrap_or(0.0) * t.gain;
                        continue;
                    }
                    if let Some(i) = track_index(a, self.loop_len, t.buf.len(), t.map.as_deref().map(|v| v.as_slice()))
                    {
                        let fade = t.env.as_deref().map_or(1.0, |e| env_gain(a, self.loop_len, e));
                        s += t.buf[i] * t.gain * fade;
                    }
                }
                self.abs = (self.abs + 1) % song_len;
            }
            for v in self.voices.iter_mut() {
                let smp = &self.kit[v.0 as usize];
                if v.1 >= 0 && (v.1 as usize) < smp.len() {
                    s += smp[v.1 as usize];
                }
                v.1 += 1;
            }
            if self.monitor {
                let x = input[i];
                s += if self.gate_on { self.gate.process(x) } else { x };
            }
            if metro_on {
                let t = if self.loop_len > 0 && self.playing {
                    self.abs as f64 - 1.0
                } else {
                    (self.clock - self.metro_origin) as f64
                };
                let beat_idx = (t / beat_len).floor();
                let frac = t - beat_idx * beat_len;
                if frac < 1.0 {
                    self.click_phase = 0;
                    self.click_accent = (beat_idx as u64).is_multiple_of(bpb);
                }
                s += self.click_sample() * if recording_first { 1.0 } else { 0.8 };
            }
            // peak limiter: instant attack, smooth release (no waveshaping distortion)
            let a = s.abs() * self.limiter_gain;
            if a > 0.95 {
                self.limiter_gain = 0.95 / s.abs();
            } else {
                self.limiter_gain += (1.0 - self.limiter_gain) * 0.00015;
            }
            let y = s * self.limiter_gain;
            out[2 * i] = y;
            out[2 * i + 1] = y;
            self.clock += 1;
        }

        let kit = &self.kit;
        self.voices.retain(|v| v.1 < kit[v.0 as usize].len() as i64);
        if !self.garbage.is_empty() && self.ev_tx.try_send(Event::Garbage(std::mem::take(&mut self.garbage))).is_err() {
            // channel full: keep for later (rare)
        }
        self.publish_status(latency, beat_len, bpb);
    }

    fn click_sample(&mut self) -> f32 {
        const CLICK_LEN: usize = 1600;
        if self.click_phase >= CLICK_LEN {
            return 0.0;
        }
        let t = self.click_phase as f32 / SR as f32;
        let f = if self.click_accent { 1760.0 } else { 1175.0 };
        let env = (-t * 90.0).exp();
        self.click_phase += 1;
        (2.0 * std::f32::consts::PI * f * t).sin() * env * self.metro.volume * 0.5
    }

    fn publish_status(&mut self, latency: usize, beat_len: f64, bpb: u64) {
        let Ok(mut st) = self.status.try_lock() else { return };
        st.rec = match &self.rec {
            Rec::Idle => RecState::Idle,
            Rec::ArmedFirst { .. } => RecState::ArmedFirst,
            Rec::First { .. } => RecState::RecordingFirst,
            Rec::ArmedOverdub { .. } => RecState::ArmedOverdub,
            Rec::Overdub { .. } => RecState::RecordingOverdub,
        };
        st.rec_track = match &self.rec {
            Rec::Idle => None,
            Rec::ArmedFirst { id, .. }
            | Rec::First { id, .. }
            | Rec::ArmedOverdub { id, .. }
            | Rec::Overdub { id, .. } => Some(*id),
        };
        (st.rec_samples, st.rec_target) = match &self.rec {
            Rec::First { buf, onset, .. } => (buf.len().saturating_sub(*onset), 0),
            Rec::Overdub { buf, written, .. } => (*written, buf.len()),
            Rec::ArmedOverdub { buf, .. } => (0, buf.len()),
            _ => (0, 0),
        };
        st.playing = self.playing;
        st.loop_len = self.loop_len;
        st.song_len = self.song_len;
        st.pos = self.abs;
        st.input_peak = st.input_peak.max(self.peak);
        let r = if self.rms_n > 0 { (self.rms_acc / self.rms_n as f32).sqrt() } else { 0.0 };
        st.input_rms = st.input_rms.max(r);
        self.peak = 0.0;
        self.rms_acc = 0.0;
        self.rms_n = 0;
        st.latency = latency;
        st.threshold = self.threshold;
        st.beat = if self.metro.enabled {
            let t = if self.loop_len > 0 && self.playing {
                self.abs as f64
            } else {
                (self.clock - self.metro_origin) as f64
            };
            let b = (t / beat_len).floor() as u64;
            Some(((b % bpb) as u32, bpb as u32))
        } else {
            None
        };
    }
}

fn copy_ring(ring: &[f32], ring_pos: usize, out: &mut Vec<f32>, n: usize, skip_newest: usize) {
    let len = ring.len();
    let n = n.min(len.saturating_sub(skip_newest));
    if out.capacity() - out.len() < n {
        return;
    }
    let end = (ring_pos + len - skip_newest) % len;
    let start = (end + len - n) % len;
    for i in 0..n {
        out.push(ring[(start + i) % len]);
    }
}

fn drop_off_thread(rec: Rec, tx: &Sender<Event>) {
    // Hand big allocations back to the UI thread for freeing.
    let bufs = match rec {
        Rec::Idle => return,
        Rec::ArmedFirst { buf, .. }
        | Rec::First { buf, .. }
        | Rec::ArmedOverdub { buf, .. }
        | Rec::Overdub { buf, .. } => buf,
    };
    let _ = tx.try_send(Event::Garbage(vec![Arc::new(bufs)]));
}

/// Make the loop seam continuous: the end of the loop fades into the audio that
/// preceded its start, so wrapping end → start sounds like the original performance.
fn crossfade_seam(buf: &mut [f32], pre: &[f32]) {
    let n = pre.len().min(buf.len() / 4);
    let len = buf.len();
    for i in 0..n {
        let a = (i + 1) as f32 / (n + 1) as f32; // 0 → 1 across the end
        let idx = len - n + i;
        buf[idx] = buf[idx] * (1.0 - a) + pre[pre.len() - n + i] * a;
    }
}

/// Beat length (samples) close to `bpm` such that the loop holds a whole number of beats.
/// Samples to hold back a drum hit, pressed while the loop at `pos` was audible, so that it
/// sounds on the nearest grid point. Audio written now is heard `latency` samples later than
/// `pos`, so only grid points further ahead than that can be hit exactly; otherwise it plays at
/// once (a late hit can't be moved back in time – the pattern snaps it from the next pass on).
pub fn drum_wait(pos: u64, grid: f64, latency: u64) -> u64 {
    let q = (pos as f64 / grid).round() * grid;
    (q - pos as f64 - latency as f64).max(0.0).round() as u64
}

pub fn fit_beat_len(loop_len: usize, bpm: f32) -> f64 {
    let nominal = 60.0 / bpm.max(20.0) as f64 * SR as f64;
    let beats = (loop_len as f64 / nominal).round().max(1.0);
    loop_len as f64 / beats
}

#[derive(Debug, PartialEq)]
pub struct FirstCut {
    pub start: usize,
    pub len: usize,
    pub bars: Option<u32>,
    pub beat_len: f64,
    pub grid_start_clock: Option<u64>,
}

/// Decide where the first loop starts and how long it is.
///
/// * `onset` – index of the block that crossed the threshold
/// * `end` – index after the last loud block plus release tail
/// * `origin_clock` – engine clock of `buf[0]`
/// * `metro` – `(metro_origin_clock, beat_len, beats_per_bar)` when the metronome is on
pub fn plan_first_take(
    onset: usize,
    end: usize,
    buf_len: usize,
    origin_clock: u64,
    preroll: usize,
    metro: Option<(u64, f64, u32)>,
) -> Option<FirstCut> {
    if end <= onset || end - onset < (SR as usize) / 10 {
        return None; // < 100 ms of signal: accidental trigger
    }
    match metro {
        None => {
            let start = onset.saturating_sub(preroll);
            let len = end - start;
            // derive a plausible tempo for an optional click later (4 beats per loop default)
            Some(FirstCut { start, len, bars: None, beat_len: fit_beat_len(len, 100.0), grid_start_clock: None })
        }
        Some((origin, beat_len, bpb)) => {
            let onset_clock = origin_clock + onset as u64;
            let rel = onset_clock as f64 - origin as f64;
            // snap the first note to the nearest beat (players often start slightly early/late)
            let k = (rel / beat_len).round().max(0.0);
            let grid_clock = origin as f64 + k * beat_len;
            let start = (grid_clock - origin_clock as f64).round().max(0.0) as usize;
            let bar_len = beat_len * bpb.max(1) as f64;
            let played = end.saturating_sub(start) as f64;
            let bars = (played / bar_len).round().max(1.0) as u32;
            let len = (bars as f64 * bar_len).round() as usize;
            let _ = buf_len;
            Some(FirstCut { start, len, bars: Some(bars), beat_len, grid_start_clock: Some(grid_clock.round() as u64) })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::dsp::{noise, sine};
    use crossbeam_channel::unbounded;

    fn looper() -> (Looper, Sender<Command>, Receiver<Event>) {
        let (ctx, crx) = unbounded();
        let (etx, erx) = unbounded();
        let st = Arc::new(Mutex::new(Status::default()));
        let mut l = Looper::new(crx, etx, st);
        l.monitor = false;
        (l, ctx, erx)
    }

    fn run(l: &mut Looper, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::new();
        for chunk in input.chunks(256) {
            let mut o = vec![0.0; chunk.len() * 2];
            l.process(chunk, &mut o, 0, 0);
            out.extend(o.iter().step_by(2));
        }
        out
    }

    #[test]
    fn plan_free_trims_both_ends() {
        let c = plan_first_take(48000, 48000 + 96000, 300000, 0, 1440, None).unwrap();
        assert_eq!(c.start, 48000 - 1440);
        assert_eq!(c.len, 96000 + 1440);
        assert!(plan_first_take(1000, 1100, 5000, 0, 10, None).is_none());
    }

    #[test]
    fn plan_metronome_snaps_to_grid_and_bars() {
        // 120 bpm → beat 24000, bar 96000. Player starts 1000 samples late on beat 3
        let beat = 24000.0;
        let c = plan_first_take(50000, 50000 + 190000, 400000, 1000, 100, Some((0, beat, 4))).unwrap();
        assert_eq!(c.bars, Some(2));
        assert_eq!(c.len, 192000);
        assert_eq!(c.grid_start_clock, Some(48000));
        assert_eq!(c.start, 47000); // grid 48000 - origin_clock 1000
    }

    #[test]
    fn first_take_removes_silence_before_and_after() {
        let (mut l, tx, rx) = looper();
        l.threshold = 0.05;
        tx.send(Command::ArmFirst {
            track_id: 1,
            buffer: Vec::with_capacity(48000 * 20),
            noise: Vec::with_capacity(RING_LEN),
        })
        .unwrap();
        // 2 s of hiss, 1 s of playing, 2 s of hiss, then "click stop"
        let mut input = noise(96000, 0.005, 3);
        input.extend(sine(196.0, 48000.0, 48000, 0.4));
        input.extend(noise(96000, 0.005, 4));
        run(&mut l, &input);
        tx.send(Command::StopRecording).unwrap();
        run(&mut l, &[0.0; 256]);
        let mut info = None;
        for ev in rx.try_iter() {
            if let Event::LoopCreated { info: i, noise, .. } = ev {
                info = Some(i);
                assert!(noise.len() > 30000, "noise sample captured");
            }
        }
        let info = info.expect("loop created");
        let expected = 48000 + l.preroll + l.tail;
        assert!((info.len as i64 - expected as i64).abs() < 600, "len {} vs {}", info.len, expected);
        assert!(l.playing);
    }

    #[test]
    fn overdub_records_exactly_one_loop_aligned_with_latency() {
        let (mut l, tx, rx) = looper();
        l.threshold = 0.05;
        l.preroll = 0;
        // fake an existing loop of 1 s
        l.loop_len = 48000;
        l.tracks.push(Track::new(1, Arc::new(vec![0.0; 48000])));
        l.playing = true;
        tx.send(Command::ArmOverdub {
            track_id: 2,
            buffer: vec![0.0; 48000],
            noise: Vec::with_capacity(RING_LEN),
            replace: false,
        })
        .unwrap();
        // silence for 10000 samples, then a tone for 2 s – recording must auto-stop after 1 loop
        let mut input = vec![0.0; 10240];
        input.extend(sine(300.0, 48000.0, 96000, 0.5));
        let latency = 0;
        let mut out = Vec::new();
        for chunk in input.chunks(256) {
            let mut o = vec![0.0; chunk.len() * 2];
            l.process(chunk, &mut o, latency, 0);
            out.push(o);
        }
        let mut done = false;
        for ev in rx.try_iter() {
            if let Event::OverdubDone { raw, track_id, onset, .. } = ev {
                assert!((onset as i64 - 10240).abs() < 300, "onset {onset}");
                assert_eq!(track_id, 2);
                assert_eq!(raw.len(), 48000);
                // tone began at loop position 10240 → before that region should be the seam-faded end
                assert!(raw[10240 + 100].abs() > 0.0 || raw[10240 + 101].abs() > 0.0);
                done = true;
            }
        }
        assert!(done, "overdub auto-finished");
        assert_eq!(l.tracks.len(), 2);
    }

    #[test]
    fn song_mode_take_keeps_playing_order() {
        // a 1-loop take started 10240 samples into a repetition, with a signal that encodes time
        let (mut l, tx, rx) = looper();
        l.threshold = 0.05;
        l.preroll = 0;
        l.loop_len = 48000;
        l.tracks.push(Track::new(1, Arc::new(vec![0.0; 48000])));
        l.playing = true;
        tx.send(Command::ArmOverdub {
            track_id: 2,
            buffer: vec![0.0; 48000],
            noise: Vec::with_capacity(RING_LEN),
            replace: false,
        })
        .unwrap();
        let mut input = vec![0.0; 10240];
        input.extend((0..60000).map(|i| 0.1 + i as f32 * 1e-5));
        for chunk in input.chunks(256) {
            let mut o = vec![0.0; chunk.len() * 2];
            l.process(chunk, &mut o, 0, 0);
        }
        let (raw, onset) = rx
            .try_iter()
            .find_map(|ev| if let Event::OverdubDone { raw, onset, .. } = ev { Some((raw, onset)) } else { None })
            .expect("overdub finished");
        let out = crate::ui::song::linearize_take(&raw, (onset % 48000) as usize, (onset % 48000) as usize, 48000);
        assert_eq!(out.len(), 96000, "spills into the next repetition instead of wrapping");
        let off = onset as usize;
        assert!(out[..off].iter().all(|v| *v == 0.0), "nothing before the first note");
        // in between the edge fades the take only rises: it is in playing order
        let body = &out[off + 500..off + 48000 - 500];
        assert!(body.windows(2).all(|w| w[1] > w[0]), "take is out of order");
    }

    #[test]
    fn retime_scales_loop_and_position() {
        let (mut l, tx, _rx) = looper();
        l.loop_len = 1000;
        l.loop_beat_len = 250.0;
        l.tracks.push(Track::new(1, Arc::new(vec![0.1; 2000])));
        l.playing = true;
        tx.send(Command::SetSongReps(4)).unwrap();
        tx.send(Command::Seek(2500)).unwrap();
        l.handle_commands();
        // 1.25× faster: loop 800, beat 200, buffers rendered for it
        tx.send(Command::Retime { loop_len: 800, beat_len: 200.0, bufs: vec![(1, Arc::new(vec![0.2; 1600]))] })
            .unwrap();
        l.handle_commands();
        assert_eq!(l.loop_len, 800);
        assert_eq!(l.song_len, 3200, "song keeps its 4 repetitions");
        assert_eq!(l.abs, 2000, "same place in the song (repetition 3, half way)");
        assert_eq!(l.tracks[0].buf.len(), 1600);
        assert!((l.metro.bpm - 14400.0).abs() < 1.0, "click follows: 60 s / 200 samples");
    }

    #[test]
    fn early_drum_hits_wait_for_the_grid() {
        // grid 1000, 100 samples of output latency
        assert_eq!(drum_wait(700, 1000.0, 100), 200, "early: waits until the grid point is heard");
        assert_eq!(drum_wait(950, 1000.0, 100), 0, "too close to wait for");
        assert_eq!(drum_wait(1200, 1000.0, 100), 0, "late hits play at once");
        assert_eq!(drum_wait(1000, 1000.0, 0), 0);
    }

    #[test]
    fn arrangement_maps_and_song_wrap() {
        // clip of a 2-rep track starting at rep 3
        let map = vec![-1, -1, -1, 3, 3, -1];
        assert_eq!(track_index(100, 1000, 2000, Some(&map)), None);
        assert_eq!(track_index(3000, 1000, 2000, Some(&map)), Some(0));
        assert_eq!(track_index(4500, 1000, 2000, Some(&map)), Some(1500));
        assert_eq!(track_index(5500, 1000, 2000, Some(&map)), None);
        assert_eq!(track_index(4500, 1000, 2000, None), Some(500));
        // a 3-rep clip starting at rep 4 of a 5-rep song wraps: reps 4, 0, 1
        let map = vec![4, 4, -1, -1, 4];
        assert_eq!(track_slot(4200, 1000, 3000, Some(&map)), Some((200, false)));
        assert_eq!(track_slot(200, 1000, 3000, Some(&map)), Some((1200, true)));
        assert_eq!(track_slot(1200, 1000, 3000, Some(&map)), Some((2200, true)));
        assert_eq!(track_slot(2200, 1000, 3000, Some(&map)), None);

        let (mut l, tx, _rx) = looper();
        l.loop_len = 1000;
        l.tracks.push(Track::new(1, Arc::new(vec![0.1; 1000])));
        l.tracks.push(Track::new(2, Arc::new(vec![0.1; 3000])));
        l.playing = true;
        run(&mut l, &[0.0; 256]);
        assert_eq!(l.song_len, 3000, "auto song = lcm of track lengths");
        tx.send(Command::SetSongReps(12)).unwrap();
        tx.send(Command::Seek(11_900)).unwrap();
        run(&mut l, &[0.0; 256]);
        assert_eq!(l.song_len, 12_000);
        assert!(l.abs < 300, "wrapped to song start, abs = {}", l.abs);
    }
}
