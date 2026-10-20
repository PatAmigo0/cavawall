//! The native backend: PipeWire capture feeding cavawall's own analysis, in
//! place of a cava child. Produces the same raw u16 frame cava would, so
//! everything downstream of the frame is shared.

use crate::capture::{self, Capture};
use crate::spectrum::{Analyzer, Layout, Params};
use cavawall::app_config::NativeConfig;
use std::time::Instant;

pub struct NativeAudio {
    pub capture: Capture,
    analyzer: Analyzer,
    last: Instant,
}

/// `[native]` over the defaults, each value kept where the analysis still
/// makes sense: a typo must not take the visualiser down
pub fn params(c: Option<&NativeConfig>) -> Params {
    let d = Params::default();
    let Some(c) = c else { return d };
    let fmin = c.fmin.unwrap_or(d.fmin).clamp(10.0, 2_000.0);
    Params {
        fmin,
        fmax: c.fmax.unwrap_or(d.fmax).clamp(fmin + 100.0, 24_000.0),
        gate_db: c.gate_db.unwrap_or(d.gate_db).clamp(-120.0, -10.0),
        range_db: c.range_db.unwrap_or(d.range_db).clamp(10.0, 100.0),
        stiffness: c.stiffness.unwrap_or(d.stiffness).clamp(10.0, 1_000.0),
        damping: c.damping.unwrap_or(d.damping).clamp(0.1, 3.0),
        flow: c.flow.unwrap_or(d.flow).clamp(0.0, 1.0),
        a_weighting: c.a_weighting.unwrap_or(d.a_weighting),
        curve: c.curve.unwrap_or(d.curve).clamp(0.5, 4.0),
    }
}

/// `general.channels` and `general.mono_option`, read as cava reads them
pub fn layout(channels: Option<&str>, mono_option: Option<&str>) -> Layout {
    match (channels, mono_option) {
        (Some("mono"), Some("left")) => Layout::Left,
        (Some("mono"), Some("right")) => Layout::Right,
        (Some("mono"), _) => Layout::Average,
        _ => Layout::Stereo,
    }
}

impl NativeAudio {
    pub fn new(config: Option<&NativeConfig>, source: Option<&str>, bars: usize, layout: Layout) -> Result<Self, String> {
        // Half a second of audio at most waits between frames; more means
        // nothing is draining it, and old audio is the part not wanted
        let capture = Capture::new(source, 512, capture::RATE as usize * capture::CHANNELS / 2)?;
        let analyzer = Analyzer::new(bars, layout, capture::RATE, params(config));
        Ok(Self { capture, analyzer, last: Instant::now() })
    }

    /// The next frame into `out`, from whatever audio arrived since the last
    pub fn tick(&mut self, out: &mut [u8]) -> Result<(), String> {
        let now = Instant::now();
        let dt = now.duration_since(self.last).as_secs_f32();
        self.last = now;
        {
            let mut q = self.capture.queue.borrow_mut();
            if let Some(e) = q.error.take() {
                return Err(format!("PipeWire stream failed: {e}"));
            }
            self.analyzer.push(&q.samples);
            q.samples.clear();
        }
        self.analyzer.frame(dt, out);
        Ok(())
    }

    /// Stop or resume capture, for a hidden or shown visualiser. Time spent
    /// paused is not a frame's worth of motion
    pub fn set_active(&mut self, active: bool) {
        self.capture.set_active(active);
        self.last = Instant::now();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_reads_channels_as_cava_does() {
        assert_eq!(layout(None, None), Layout::Stereo, "cava's default");
        assert_eq!(layout(Some("stereo"), Some("left")), Layout::Stereo);
        assert_eq!(layout(Some("mono"), None), Layout::Average);
        assert_eq!(layout(Some("mono"), Some("right")), Layout::Right);
    }

    #[test]
    fn wild_settings_are_kept_in_range() {
        let p = params(Some(&NativeConfig { fmin: Some(5.0), fmax: Some(1.0), flow: Some(9.0), ..Default::default() }));
        assert_eq!(p.fmin, 10.0);
        assert!(p.fmax > p.fmin);
        assert_eq!(p.flow, 1.0);
    }
}
