//! Audio I/O: two ALSA PCMs routed through PipeWire's ALSA plugin (`pipewire:NODE=<node>`),
//! so every stream targets the chosen device directly while still sharing the PipeWire graph.
//!
//! Capture thread → lock-free ring → playback thread (owns the [`Looper`]).

use super::looper::{Command, Event, Looper, SR, Status};
use crate::config::InputChannel;
use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use crossbeam_channel::{Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

const PERIOD: usize = 256;

pub struct EngineHandle {
    pub cmd: Sender<Command>,
    pub events: Receiver<Event>,
    pub status: Arc<Mutex<Status>>,
    pub errors: Arc<Mutex<Option<String>>>,
    channel: Arc<AtomicU8>,
    looper: Option<Looper>,
    running: Option<Running>,
}

struct Running {
    stop: Arc<AtomicBool>,
    capture: Option<JoinHandle<()>>,
    playback: JoinHandle<Looper>,
}

fn pcm_name(node: Option<&str>) -> String {
    match node {
        Some(n) if !n.is_empty() => format!("pipewire:NODE={n}"),
        _ => "pipewire".to_string(),
    }
}

fn open(name: &str, dir: Direction, buffer_periods: usize) -> anyhow::Result<PCM> {
    let pcm = PCM::new(name, dir, true)?; // non-blocking + poll: never hangs on unplug
    {
        let hwp = HwParams::any(&pcm)?;
        hwp.set_channels(2)?;
        hwp.set_rate(SR, ValueOr::Nearest)?;
        hwp.set_format(Format::float())?;
        hwp.set_access(Access::RWInterleaved)?;
        hwp.set_period_size_near(PERIOD as alsa::pcm::Frames, ValueOr::Nearest)?;
        hwp.set_buffer_size_near((PERIOD * buffer_periods) as alsa::pcm::Frames)?;
        pcm.hw_params(&hwp)?;
    }
    if dir == Direction::Playback {
        let swp = pcm.sw_params_current()?;
        swp.set_start_threshold(PERIOD as alsa::pcm::Frames)?;
        pcm.sw_params(&swp)?;
    }
    pcm.prepare()?;
    Ok(pcm)
}

impl EngineHandle {
    pub fn new() -> Self {
        let (cmd_tx, cmd_rx) = crossbeam_channel::unbounded();
        let (ev_tx, ev_rx) = crossbeam_channel::bounded(256);
        let status = Arc::new(Mutex::new(Status::default()));
        let looper = Looper::new(cmd_rx, ev_tx, status.clone());
        Self {
            cmd: cmd_tx,
            events: ev_rx,
            status,
            errors: Arc::new(Mutex::new(None)),
            channel: Arc::new(AtomicU8::new(0)),
            looper: Some(looper),
            running: None,
        }
    }

    pub fn send(&self, c: Command) {
        let _ = self.cmd.send(c);
    }

    pub fn set_channel(&self, ch: InputChannel) {
        self.channel.store(ch as u8, Ordering::Relaxed);
    }

    pub fn stop(&mut self) {
        if let Some(r) = self.running.take() {
            r.stop.store(true, Ordering::Relaxed);
            if let Some(c) = r.capture {
                let _ = c.join();
            }
            match r.playback.join() {
                Ok(l) => self.looper = Some(l),
                Err(_) => {
                    // playback thread panicked: start over with a fresh looper
                    *self = Self::new();
                }
            }
        }
    }

    /// (Re)start audio on the given PipeWire nodes. The looper (and its loops) survives restarts.
    pub fn start(&mut self, input: Option<&str>, output: Option<&str>) {
        self.stop();
        *self.errors.lock().unwrap() = None;
        let Some(mut looper) = self.looper.take() else { return };
        let stop = Arc::new(AtomicBool::new(false));
        let (mut prod, mut cons) = rtrb::RingBuffer::<f32>::new(SR as usize);
        let in_delay = Arc::new(AtomicI64::new(0));

        let in_name = pcm_name(input);
        let out_name = pcm_name(output);
        let errors = self.errors.clone();

        // ---- capture
        let capture = {
            let stop = stop.clone();
            let errors = errors.clone();
            let channel = self.channel.clone();
            let in_delay = in_delay.clone();
            std::thread::Builder::new()
                .name("capture".into())
                .spawn(move || {
                    let pcm = match open(&in_name, Direction::Capture, 4) {
                        Ok(p) => p,
                        Err(e) => {
                            *errors.lock().unwrap() = Some(format!("Input: {e}"));
                            return;
                        }
                    };
                    let io = match pcm.io_f32() {
                        Ok(io) => io,
                        Err(e) => {
                            *errors.lock().unwrap() = Some(format!("Input: {e}"));
                            return;
                        }
                    };
                    let _ = pcm.start();
                    let mut buf = vec![0f32; PERIOD * 2];
                    let mut failures = 0;
                    while !stop.load(Ordering::Relaxed) {
                        match pcm.wait(Some(100)) {
                            Ok(true) => {}
                            Ok(false) => {
                                failures += 1;
                                if failures > 30 {
                                    *errors.lock().unwrap() = Some("Input stopped delivering audio".into());
                                    failures = 0;
                                }
                                continue;
                            }
                            Err(e) => {
                                let _ = pcm.try_recover(e, true);
                                let _ = pcm.start();
                                std::thread::sleep(std::time::Duration::from_millis(10));
                                continue;
                            }
                        }
                        match io.readi(&mut buf) {
                            Ok(frames) => {
                                failures = 0;
                                let ch = channel.load(Ordering::Relaxed);
                                let n = frames.min(prod.slots());
                                if let Ok(mut chunk) = prod.write_chunk_uninit(n) {
                                    let (a, b) = chunk.as_mut_slices();
                                    for (i, slot) in a.iter_mut().chain(b.iter_mut()).enumerate() {
                                        let (l, r) = (buf[2 * i], buf[2 * i + 1]);
                                        slot.write(match ch {
                                            1 => l,
                                            2 => r,
                                            _ => 0.5 * (l + r),
                                        });
                                    }
                                    // SAFETY: all `n` slots were initialised above
                                    unsafe { chunk.commit(n) };
                                }
                                if let Ok(d) = pcm.delay() {
                                    in_delay.store(d, Ordering::Relaxed);
                                }
                            }
                            Err(e) if e.errno() == libc_eagain() => {}
                            Err(e) => {
                                if pcm.try_recover(e, true).is_err() {
                                    std::thread::sleep(std::time::Duration::from_millis(20));
                                    let _ = pcm.prepare();
                                }
                                let _ = pcm.start();
                            }
                        }
                    }
                })
                .ok()
        };

        // ---- playback (owns the looper)
        let playback = {
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("playback".into())
                .spawn(move || {
                    let mut input = vec![0f32; PERIOD];
                    let mut out = vec![0f32; PERIOD * 2];
                    let pcm = match open(&out_name, Direction::Playback, 3) {
                        Ok(p) => Some(p),
                        Err(e) => {
                            *errors.lock().unwrap() = Some(format!("Output: {e}"));
                            None
                        }
                    };
                    let Some(pcm) = pcm else {
                        // keep the looper alive (state, commands) even without an output
                        while !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_millis(PERIOD as u64 * 1000 / SR as u64));
                            let n = cons.slots().min(PERIOD);
                            if let Ok(c) = cons.read_chunk(n) {
                                c.commit_all();
                            }
                            input.fill(0.0);
                            looper.process(&input, &mut out, 0, 0);
                        }
                        return looper;
                    };
                    let io = pcm.io_f32().expect("f32 io");
                    // pre-fill one period of silence to get going
                    let _ = io.writei(&out);
                    while !stop.load(Ordering::Relaxed) {
                        // keep input latency bounded: drop backlog beyond 2 periods (clock drift)
                        let backlog = cons.slots();
                        if backlog > PERIOD * 2
                            && let Ok(c) = cons.read_chunk(backlog - PERIOD)
                        {
                            c.commit_all();
                        }
                        // give the capture side a moment if it is just behind (avoids dropouts)
                        let mut spins = 0;
                        while cons.slots() < PERIOD && spins < 6 && !stop.load(Ordering::Relaxed) {
                            std::thread::sleep(std::time::Duration::from_micros(500));
                            spins += 1;
                        }
                        let avail = cons.slots().min(PERIOD);
                        input.fill(0.0);
                        if let Ok(c) = cons.read_chunk(avail) {
                            let (a, b) = c.as_slices();
                            input[..a.len()].copy_from_slice(a);
                            input[a.len()..a.len() + b.len()].copy_from_slice(b);
                            c.commit_all();
                        }
                        let out_delay = pcm.delay().unwrap_or(0).max(0) as usize;
                        let latency =
                            out_delay + in_delay.load(Ordering::Relaxed).max(0) as usize + cons.slots() + PERIOD;
                        looper.process(&input, &mut out, latency, out_delay + PERIOD);
                        // write the whole period, waiting for room (with timeout)
                        let mut written = 0;
                        let mut stalls = 0;
                        while written < PERIOD && !stop.load(Ordering::Relaxed) {
                            match io.writei(&out[written * 2..]) {
                                Ok(n) => written += n,
                                Err(e) if e.errno() == libc_eagain() => {
                                    if !matches!(pcm.wait(Some(100)), Ok(true)) {
                                        stalls += 1;
                                        if stalls > 20 {
                                            *errors.lock().unwrap() = Some("Output stopped accepting audio".into());
                                            break;
                                        }
                                    }
                                }
                                Err(e) => {
                                    if pcm.try_recover(e, true).is_err() {
                                        std::thread::sleep(std::time::Duration::from_millis(20));
                                        let _ = pcm.prepare();
                                    }
                                    break;
                                }
                            }
                        }
                    }
                    let _ = pcm.drop();
                    looper
                })
                .expect("spawn playback thread")
        };

        self.running = Some(Running { stop, capture, playback });
    }
}

impl Drop for EngineHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

const fn libc_eagain() -> i32 {
    11 // EAGAIN on Linux
}
