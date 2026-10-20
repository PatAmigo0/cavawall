//! Native audio capture: a PipeWire stream driven from cavawall's own event
//! loop, with no thread of its own.
//!
//! The stream is connected without RT_PROCESS, so PipeWire calls `process` on
//! the thread that iterates its main loop - this one, from a calloop source
//! on the loop's fd. Samples land in a shared queue that the analysis drains
//! once per frame.

use pipewire as pw;
use pw::spa;
use std::cell::RefCell;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::rc::Rc;

/// The rate the stream asks for. The graph's own rate on a stock PipeWire, so
/// PipeWire has nothing to resample
pub const RATE: u32 = 48_000;
/// Interleaved left, right
pub const CHANNELS: usize = 2;

/// Samples the analysis has not taken yet, interleaved stereo i16
#[derive(Default)]
pub struct Queue {
    pub samples: Vec<i16>,
    /// Set from the stream's state callback; read and reported by the owner
    pub error: Option<String>,
    /// Samples dropped because nothing drained the queue
    pub dropped: u64,
}

impl Queue {
    /// Oldest samples go first when the queue is full: a stalled reader must
    /// not grow it without bound, and stale audio is the part not wanted
    fn push(&mut self, new: &[i16], cap: usize) {
        self.samples.extend_from_slice(new);
        if self.samples.len() > cap {
            let over = self.samples.len() - cap;
            self.samples.drain(..over);
            self.dropped += over as u64;
        }
    }
}

pub struct Capture {
    main_loop: pw::main_loop::MainLoopRc,
    _context: pw::context::ContextRc,
    _core: pw::core::CoreRc,
    stream: pw::stream::StreamRc,
    _listener: pw::stream::StreamListener<()>,
    pub queue: Rc<RefCell<Queue>>,
}

/// What to capture, as cava reads its `source` for the PipeWire input: unset
/// is the default output's monitor, a name ending in `.monitor` is that sink's
/// monitor, anything else is a source by name
fn target(source: Option<&str>) -> (bool, Option<&str>) {
    match source {
        None => (true, None),
        Some(s) => match s.strip_suffix(".monitor") {
            Some(sink) => (true, Some(sink)),
            None => (false, Some(s)),
        },
    }
}

impl Capture {
    /// Connect a capture stream. `queue_cap` bounds the samples kept between
    /// drains, interleaved
    pub fn new(source: Option<&str>, latency_frames: u32, queue_cap: usize) -> Result<Self, String> {
        pw::init();
        let err = |what: &str, e: pw::Error| format!("PipeWire: {what}: {e}");
        let main_loop = pw::main_loop::MainLoopRc::new(None).map_err(|e| err("cannot create a loop", e))?;
        let context = pw::context::ContextRc::new(&main_loop, None).map_err(|e| err("cannot create a context", e))?;
        let core = context.connect_rc(None).map_err(|e| err("cannot connect; is PipeWire running?", e))?;

        let mut props = pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Audio",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Music",
            *pw::keys::APP_NAME => "cavawall",
            *pw::keys::NODE_NAME => "cavawall",
            *pw::keys::NODE_LATENCY => format!("{latency_frames}/{RATE}"),
            // As cava does: keep processing while nothing else listens, so a
            // monitor with no other reader still delivers
            *pw::keys::NODE_ALWAYS_PROCESS => "true",
        };
        let (sink, name) = target(source);
        if sink {
            props.insert(*pw::keys::STREAM_CAPTURE_SINK, "true");
        }
        if let Some(name) = name {
            props.insert(*pw::keys::TARGET_OBJECT, name);
        }

        let stream = pw::stream::StreamRc::new(core.clone(), "cavawall", props)
            .map_err(|e| err("cannot create the stream", e))?;
        let queue = Rc::new(RefCell::new(Queue::default()));

        let q_process = Rc::clone(&queue);
        let q_state = Rc::clone(&queue);
        let listener = stream
            .add_local_listener_with_user_data(())
            .state_changed(move |_, (), _, new| {
                if let pw::stream::StreamState::Error(e) = new {
                    q_state.borrow_mut().error = Some(e);
                }
            })
            .process(move |stream, ()| {
                let Some(mut buffer) = stream.dequeue_buffer() else { return };
                let Some(data) = buffer.datas_mut().first_mut() else { return };
                let (offset, size) = (data.chunk().offset() as usize, data.chunk().size() as usize);
                let Some(bytes) = data.data() else { return };
                let Some(bytes) = bytes.get(offset..offset + size) else { return };
                let samples: Vec<i16> = bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
                q_process.borrow_mut().push(&samples, queue_cap);
            })
            .register()
            .map_err(|e| err("cannot listen to the stream", e))?;

        let mut info = spa::param::audio::AudioInfoRaw::new();
        info.set_format(spa::param::audio::AudioFormat::S16LE);
        info.set_rate(RATE);
        info.set_channels(CHANNELS as u32);
        let object = spa::pod::Object {
            type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: info.into(),
        };
        let bytes = spa::pod::serialize::PodSerializer::serialize(
            std::io::Cursor::new(Vec::new()),
            &spa::pod::Value::Object(object),
        )
        .map_err(|e| format!("PipeWire: cannot describe the format: {e:?}"))?
        .0
        .into_inner();
        let pod = spa::pod::Pod::from_bytes(&bytes).ok_or("PipeWire: the format did not serialise")?;
        stream
            .connect(
                spa::utils::Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut [pod],
            )
            .map_err(|e| err("cannot connect the stream", e))?;

        Ok(Self { main_loop, _context: context, _core: core, stream, _listener: listener, queue })
    }

    /// A duplicate of the loop's fd, for the event loop to watch: readable
    /// means PipeWire has work, done by `dispatch`
    pub fn fd(&self) -> std::io::Result<OwnedFd> {
        let raw = self.main_loop.loop_().fd().as_raw_fd();
        // SAFETY: dup of a descriptor the PipeWire loop owns and keeps open
        // for its own life; the duplicate is ours alone
        let dup = unsafe { libc::fcntl(raw, libc::F_DUPFD_CLOEXEC, 0) };
        if dup < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: freshly duplicated, owned by nothing else
        Ok(unsafe { OwnedFd::from_raw_fd(dup) })
    }

    /// Run what PipeWire has ready, without blocking: stream events and the
    /// `process` callbacks that fill the queue
    pub fn dispatch(&self) {
        self.main_loop.loop_().iterate(pw::loop_::Timeout::None);
    }

    /// Stop or resume delivery, for a paused visualiser
    pub fn set_active(&self, active: bool) {
        let _ = self.stream.set_active(active);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn targets_follow_cavas_source_rules() {
        assert_eq!(target(None), (true, None), "the default output's monitor");
        assert_eq!(target(Some("easyeffects_sink.monitor")), (true, Some("easyeffects_sink")));
        assert_eq!(target(Some("alsa_input.mic")), (false, Some("alsa_input.mic")));
    }

    #[test]
    fn a_full_queue_drops_the_oldest() {
        let mut q = Queue::default();
        q.push(&[1, 2, 3, 4], 6);
        q.push(&[5, 6, 7, 8], 6);
        assert_eq!(q.samples, [3, 4, 5, 6, 7, 8]);
        assert_eq!(q.dropped, 2);
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Needs a running PipeWire: `cargo test --release capture -- --ignored`
    #[test]
    #[ignore = "needs a PipeWire session"]
    fn captures_a_second_of_the_default_output() {
        let cap = Capture::new(None, 512, RATE as usize * CHANNELS).expect("connect");
        let fd = cap.fd().expect("fd");
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(1) {
            let mut pfd = libc::pollfd { fd: fd.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            // SAFETY: one valid pollfd for the duration of the call
            unsafe { libc::poll(&raw mut pfd, 1, 50) };
            cap.dispatch();
        }
        let q = cap.queue.borrow();
        assert!(q.error.is_none(), "{:?}", q.error);
        eprintln!("captured {} samples, dropped {}", q.samples.len(), q.dropped);
        assert!(q.samples.len() > RATE as usize, "about a second of stereo frames");
    }
}
