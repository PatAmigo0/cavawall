//! Fullscreen windows from the standard foreign-toplevel protocol, for
//! compositors other than Hyprland: Sway, river, niri, labwc, Wayfire
//!
//! Every window is announced with its state and the outputs it is on; a
//! monitor is covered while a fullscreen, unminimised window is on it. The
//! protocol does not say whether the window's workspace is the visible one,
//! so a fullscreen window left on a hidden workspace still counts - Hyprland's
//! IPC, used there instead, can tell the difference

use std::collections::{BTreeSet, HashMap};

use smithay_client_toolkit::reexports::client::{
    backend::ObjectId, event_created_child, protocol::wl_output::WlOutput, Connection, Dispatch, Proxy,
    QueueHandle,
};
use wayland_protocols_wlr::foreign_toplevel::v1::client::{
    zwlr_foreign_toplevel_handle_v1::{self, State, ZwlrForeignToplevelHandleV1},
    zwlr_foreign_toplevel_manager_v1::{self, ZwlrForeignToplevelManagerV1},
};

use crate::AppState;

/// What one window is doing, as far as coverage is concerned
#[derive(Default)]
pub struct Window {
    outputs: Vec<WlOutput>,
    fullscreen: bool,
    minimized: bool,
    /// State and outputs arrive in pieces; `done` says a batch is complete
    pending_state: Option<(bool, bool)>,
}

#[derive(Default)]
pub struct Toplevels {
    windows: HashMap<ObjectId, Window>,
}

impl Toplevels {
    /// Names of the outputs a fullscreen, unminimised window is on
    pub fn covered(&self, name_of: impl Fn(&WlOutput) -> Option<String>) -> BTreeSet<String> {
        self.windows
            .values()
            .filter(|w| w.fullscreen && !w.minimized)
            .flat_map(|w| w.outputs.iter().filter_map(&name_of))
            .collect()
    }
}

impl Dispatch<ZwlrForeignToplevelManagerV1, ()> for AppState {
    fn event(
        state: &mut Self,
        _: &ZwlrForeignToplevelManagerV1,
        event: zwlr_foreign_toplevel_manager_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let zwlr_foreign_toplevel_manager_v1::Event::Toplevel { toplevel } = event {
            state.toplevels.windows.insert(toplevel.id(), Window::default());
        }
    }

    event_created_child!(AppState, ZwlrForeignToplevelManagerV1, [
        zwlr_foreign_toplevel_manager_v1::EVT_TOPLEVEL_OPCODE => (ZwlrForeignToplevelHandleV1, ()),
    ]);
}

impl Dispatch<ZwlrForeignToplevelHandleV1, ()> for AppState {
    fn event(
        state: &mut Self,
        handle: &ZwlrForeignToplevelHandleV1,
        event: zwlr_foreign_toplevel_handle_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use zwlr_foreign_toplevel_handle_v1::Event;
        let id = handle.id();
        match event {
            Event::State { state: raw } => {
                // An array of u32 states in native byte order
                let has = |s: State| raw.as_chunks::<4>().0.iter().any(|c| u32::from_ne_bytes(*c) == s as u32);
                if let Some(w) = state.toplevels.windows.get_mut(&id) {
                    w.pending_state = Some((has(State::Fullscreen), has(State::Minimized)));
                }
            }
            Event::OutputEnter { output } => {
                if let Some(w) = state.toplevels.windows.get_mut(&id) {
                    w.outputs.push(output);
                }
            }
            Event::OutputLeave { output } => {
                if let Some(w) = state.toplevels.windows.get_mut(&id) {
                    w.outputs.retain(|o| o != &output);
                }
            }
            Event::Done => {
                if let Some(w) = state.toplevels.windows.get_mut(&id) {
                    if let Some((f, m)) = w.pending_state.take() {
                        w.fullscreen = f;
                        w.minimized = m;
                    }
                }
                state.recheck_toplevels(qh);
            }
            Event::Closed => {
                state.toplevels.windows.remove(&id);
                handle.destroy();
                state.recheck_toplevels(qh);
            }
            _ => {}
        }
    }
}
