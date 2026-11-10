//! Move the real cursor to a known spot with wlr-virtual-pointer, so the
//! evdev-based estimate in `pointer` starts out exact.

use wayland_client::protocol::{wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_wlr::virtual_pointer::v1::client::{
    zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1,
    zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1,
};

#[derive(Default)]
struct State {
    mgr: Option<ZwlrVirtualPointerManagerV1>,
    seat: Option<wl_seat::WlSeat>,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(s: &mut Self, reg: &wl_registry::WlRegistry, ev: wl_registry::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_registry::Event::Global { name, interface, version } = ev {
            match interface.as_str() {
                "zwlr_virtual_pointer_manager_v1" => s.mgr = Some(reg.bind(name, version.min(2), qh, ())),
                "wl_seat" if s.seat.is_none() => s.seat = Some(reg.bind(name, 1, qh, ())),
                _ => {}
            }
        }
    }
}

delegate_noop!(State: ignore ZwlrVirtualPointerManagerV1);
delegate_noop!(State: ignore ZwlrVirtualPointerV1);
delegate_noop!(State: ignore wl_seat::WlSeat);

/// Keeps a virtual pointer open to move the cursor repeatedly.
pub struct Warper {
    queue: wayland_client::EventQueue<State>,
    state: State,
    ptr: ZwlrVirtualPointerV1,
    conn: Connection,
}

impl Warper {
    pub fn new() -> Option<Self> {
        let conn = Connection::connect_to_env().ok()?;
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();
        conn.display().get_registry(&qh, ());
        let mut state = State::default();
        queue.roundtrip(&mut state).ok()?;
        let ptr = state.mgr.as_ref()?.create_virtual_pointer(state.seat.as_ref(), &qh, ());
        Some(Self { queue, state, ptr, conn })
    }

    /// Put the cursor at (x, y) inside an area of size (w, h) (the output
    /// in logical pixels).
    pub fn warp(&mut self, x: f32, y: f32, w: f32, h: f32) -> bool {
        let (w, h) = (w.max(1.0) as u32, h.max(1.0) as u32);
        self.ptr.motion_absolute(0, (x.max(0.0) as u32).min(w - 1), (y.max(0.0) as u32).min(h - 1), w, h);
        self.ptr.frame();
        let _ = self.queue.dispatch_pending(&mut self.state);
        self.conn.flush().is_ok()
    }
}

/// One-off move, see [`Warper::warp`].
pub fn warp(x: f32, y: f32, w: f32, h: f32) -> bool {
    match Warper::new() {
        Some(mut wp) => {
            let ok = wp.warp(x, y, w, h);
            let _ = wp.queue.roundtrip(&mut wp.state);
            ok
        }
        None => false,
    }
}
