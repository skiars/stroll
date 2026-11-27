use calloop::timer::{TimeoutAction, Timer};
use gpui::{ExternalDragPayload, ExternalPaths};
use x11rb::{CURRENT_TIME, NONE};

use super::*;

const XDND_VERSION: u32 = 5;
const DROP_TIMEOUT: Duration = Duration::from_secs(3);

#[derive(Clone, Copy)]
pub(super) struct DragPointer {
    pub(super) window: xproto::Window,
    device: xinput::DeviceId,
    time: xproto::Timestamp,
    root: xproto::Window,
    x: i16,
    y: i16,
}

impl From<&xinput::ButtonPressEvent> for DragPointer {
    fn from(event: &xinput::ButtonPressEvent) -> Self {
        Self {
            window: event.event,
            device: event.deviceid,
            time: event.time,
            root: event.root,
            x: (event.root_x >> 16) as i16,
            y: (event.root_y >> 16) as i16,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Target {
    window: xproto::Window,
    proxy: xproto::Window,
    version: u32,
}

#[derive(PartialEq, Eq)]
enum Phase {
    Dragging,
    Released(xproto::Timestamp),
    Dropped,
}

pub(super) struct XdndSource {
    start: DragPointer,
    paths: ExternalPaths,
    uri_list: String,
    target: Option<Target>,
    accepted: Option<bool>,
    inside_source: bool,
    phase: Phase,
}

impl X11ClientStatePtr {
    pub(crate) fn start_external_drag(
        &self,
        origin: xproto::Window,
        payload: &ExternalDragPayload,
    ) -> bool {
        let Some(client) = self.get_client() else {
            return false;
        };
        let mut state = client.0.borrow_mut();
        if state.xdnd_source.is_some() {
            return false;
        }
        let Some(start) = state
            .drag_pointer
            .filter(|pointer| pointer.window == origin)
        else {
            return false;
        };
        let ExternalDragPayload::Files(paths) = payload;
        let uri_list = crate::linux::file_uri_list(paths);
        if uri_list.is_empty() {
            return false;
        }
        let xcb = &state.xcb_connection;
        let atoms = &state.atoms;
        let grab = (|| {
            let mask = xinput::XIEventMask::MOTION | xinput::XIEventMask::BUTTON_RELEASE;
            let result = xcb
                .xinput_xi_grab_device(
                    origin,
                    start.time,
                    NONE,
                    start.device,
                    xproto::GrabMode::ASYNC,
                    xproto::GrabMode::ASYNC,
                    xinput::GrabOwner::NO_OWNER,
                    &[mask.into()],
                )?
                .reply()?;
            anyhow::ensure!(
                result.status == xproto::GrabStatus::SUCCESS,
                "XDND pointer grab failed"
            );
            Ok::<_, anyhow::Error>(())
        })();
        if grab.log_err().is_none() {
            return false;
        }
        let mut source = XdndSource {
            start,
            paths: ExternalPaths(
                paths
                    .entries()
                    .iter()
                    .map(|(path, _)| path.clone())
                    .collect(),
            ),
            uri_list,
            target: None,
            accepted: None,
            inside_source: false,
            phase: Phase::Dragging,
        };
        if check_reply(
            || "Failed to own XdndSelection",
            xcb.set_selection_owner(origin, atoms.XdndSelection, start.time),
        )
        .log_err()
        .is_none()
        {
            source.finish(xcb, atoms, false);
            return false;
        }
        let target = target_at(xcb, atoms, start).filter(|target| target.window != origin);
        source.move_to(xcb, atoms, start, target);
        state.xdnd_source = Some(source);
        true
    }
}

impl X11Client {
    pub(super) fn handle_xdnd_source(&self, event: &Event) -> bool {
        let mut state = self.0.borrow_mut();
        let Some(mut source) = state.xdnd_source.take() else {
            return false;
        };
        let xcb = &state.xcb_connection;
        let atoms = &state.atoms;
        let origin = source.start.window;
        let dragging = source.phase == Phase::Dragging;
        let mut local = SmallVec::<[FileDropEvent; 3]>::new();
        let mut finish = false;
        let mut timeout = false;
        let consumed = match event {
            Event::XinputMotion(event) if dragging && event.deviceid == source.start.device => {
                let pointer = DragPointer::from(event);
                let target = target_at(xcb, atoms, pointer);
                if target.is_some_and(|target| target.window == origin) {
                    source.move_to(xcb, atoms, pointer, None);
                    let position = self_drop_position(event, state.scale_factor);
                    local.push(if source.inside_source {
                        FileDropEvent::Pending { position }
                    } else {
                        FileDropEvent::Entered {
                            position,
                            paths: source.paths.clone(),
                        }
                    });
                    source.inside_source = true;
                } else {
                    if std::mem::take(&mut source.inside_source) {
                        local.push(FileDropEvent::Exited);
                    }
                    source.move_to(xcb, atoms, pointer, target);
                }
                true
            }
            Event::XinputButtonRelease(event)
                if dragging && event.deviceid == source.start.device && event.detail == 1 =>
            {
                let pointer = DragPointer::from(event);
                let target = target_at(xcb, atoms, pointer);
                if target.is_some_and(|target| target.window == origin) {
                    let position = self_drop_position(event, state.scale_factor);
                    if !source.inside_source {
                        local.push(FileDropEvent::Entered {
                            position,
                            paths: source.paths.clone(),
                        });
                    }
                    local.push(FileDropEvent::Submit { position });
                    finish = true;
                } else {
                    if std::mem::take(&mut source.inside_source) {
                        local.push(FileDropEvent::Exited);
                    }
                    if source.target != target {
                        source.move_to(xcb, atoms, pointer, target);
                    }
                    if target.is_some() && source.accepted != Some(false) {
                        source.ungrab(xcb);
                        source.phase = Phase::Released(event.time);
                        source.drop_if_accepted(xcb, atoms);
                        timeout = true;
                    } else {
                        finish = true;
                    }
                }
                true
            }
            Event::KeyPress(event)
                if dragging
                    && state.xkb.key_get_one_sym(event.detail.into()) == xkbc::Keysym::Escape =>
            {
                finish = true;
                true
            }
            Event::ClientMessage(message) if message.window == origin && message.format == 32 => {
                let [window, flags, _, _, action] = message.data.as_data32();
                if message.type_ == atoms.XdndStatus {
                    if source.phase != Phase::Dropped
                        && source.target.is_some_and(|target| target.window == window)
                    {
                        let accepted = flags & 1 != 0 && action == atoms.XdndActionCopy;
                        source.accepted = Some(accepted);
                        if matches!(source.phase, Phase::Released(_)) {
                            source.drop_if_accepted(xcb, atoms);
                            finish = !accepted;
                        }
                    }
                    true
                } else if message.type_ == atoms.XdndFinished {
                    finish = source.phase == Phase::Dropped
                        && source.target.is_some_and(|target| target.window == window);
                    true
                } else {
                    false
                }
            }
            Event::SelectionRequest(event)
                if event.owner == origin && event.selection == atoms.XdndSelection =>
            {
                source.serve_selection(xcb, atoms, event);
                true
            }
            Event::SelectionClear(event)
                if event.owner == origin && event.selection == atoms.XdndSelection =>
            {
                finish = true;
                true
            }
            _ => false,
        };
        if timeout {
            let client = state.this.clone();
            let started = source.start.time;
            if state
                .loop_handle
                .insert_source(Timer::from_duration(DROP_TIMEOUT), move |_, _, _| {
                    if let Some(client) = client.get_client() {
                        client.end_xdnd_source(origin, started);
                    }
                    TimeoutAction::Drop
                })
                .log_err()
                .is_none()
            {
                finish = true;
            }
        }
        if finish {
            source.finish(xcb, atoms, true);
            local.push(FileDropEvent::Ended);
        } else {
            state.xdnd_source = Some(source);
        }
        let window = state
            .windows
            .get(&origin)
            .map(|window| window.window.clone());
        drop(state);
        if let Some(window) = window {
            for input in local {
                window.handle_input(PlatformInput::FileDrop(input));
            }
        }
        consumed
    }

    fn end_xdnd_source(&self, origin: xproto::Window, started: xproto::Timestamp) {
        let mut state = self.0.borrow_mut();
        if !state
            .xdnd_source
            .as_ref()
            .is_some_and(|source| source.start.window == origin && source.start.time == started)
        {
            return;
        }
        let source = state.xdnd_source.take().unwrap();
        source.finish(&state.xcb_connection, &state.atoms, true);
        let window = state
            .windows
            .get(&origin)
            .map(|window| window.window.clone());
        drop(state);
        if let Some(window) = window {
            window.handle_input(PlatformInput::FileDrop(FileDropEvent::Ended));
        }
    }
}

impl X11ClientState {
    pub(super) fn close_drag_source(&mut self, origin: xproto::Window) {
        if self
            .xdnd_source
            .as_ref()
            .is_some_and(|source| source.start.window == origin)
        {
            let source = self.xdnd_source.take().unwrap();
            source.finish(&self.xcb_connection, &self.atoms, true);
        }
    }
}

impl XdndSource {
    fn move_to(
        &mut self,
        xcb: &XCBConnection,
        atoms: &XcbAtoms,
        pointer: DragPointer,
        target: Option<Target>,
    ) {
        let origin = self.start.window;
        if target != self.target {
            if let Some(previous) = self.target {
                send(xcb, previous, atoms.XdndLeave, [origin, 0, 0, 0, 0]);
            }
            self.target = target;
            self.accepted = None;
            if let Some(target) = target {
                send(
                    xcb,
                    target,
                    atoms.XdndEnter,
                    [origin, target.version << 24, atoms.TextUriList, 0, 0],
                );
            }
        }
        if let Some(target) = target {
            let coordinates = (u32::from(pointer.x as u16) << 16) | u32::from(pointer.y as u16);
            send(
                xcb,
                target,
                atoms.XdndPosition,
                [origin, 0, coordinates, pointer.time, atoms.XdndActionCopy],
            );
        }
        xcb.flush().log_err();
    }

    fn drop_if_accepted(&mut self, xcb: &XCBConnection, atoms: &XcbAtoms) {
        let (Phase::Released(time), Some(target), Some(true)) =
            (&self.phase, self.target, self.accepted)
        else {
            return;
        };
        send(
            xcb,
            target,
            atoms.XdndDrop,
            [self.start.window, 0, *time, 0, 0],
        );
        self.phase = Phase::Dropped;
        xcb.flush().log_err();
    }

    fn serve_selection(
        &self,
        xcb: &XCBConnection,
        atoms: &XcbAtoms,
        event: &xproto::SelectionRequestEvent,
    ) {
        let property = if event.property == NONE {
            event.target
        } else {
            event.property
        };
        let data = match event.target {
            mime if mime == atoms.TextUriList => xcb
                .change_property8(
                    xproto::PropMode::REPLACE,
                    event.requestor,
                    property,
                    atoms.TextUriList,
                    self.uri_list.as_bytes(),
                )
                .ok(),
            targets if targets == atoms.TARGETS => xcb
                .change_property32(
                    xproto::PropMode::REPLACE,
                    event.requestor,
                    property,
                    AtomEnum::ATOM,
                    &[atoms.TARGETS, atoms.TextUriList],
                )
                .ok(),
            _ => None,
        };
        let notify = xproto::SelectionNotifyEvent {
            response_type: xproto::SELECTION_NOTIFY_EVENT,
            sequence: 0,
            time: event.time,
            requestor: event.requestor,
            selection: event.selection,
            target: event.target,
            property: if data.is_some_and(|cookie| cookie.check().is_ok()) {
                property
            } else {
                NONE
            },
        };
        xcb.send_event(false, event.requestor, EventMask::NO_EVENT, notify)
            .log_err();
        xcb.flush().log_err();
    }

    fn ungrab(&self, xcb: &XCBConnection) {
        xcb.xinput_xi_ungrab_device(CURRENT_TIME, self.start.device)
            .log_err();
    }

    fn finish(self, xcb: &XCBConnection, atoms: &XcbAtoms, clear_selection: bool) {
        if self.phase == Phase::Dragging {
            self.ungrab(xcb);
        }
        if self.phase != Phase::Dropped
            && let Some(target) = self.target
        {
            send(
                xcb,
                target,
                atoms.XdndLeave,
                [self.start.window, 0, 0, 0, 0],
            );
        }
        if clear_selection
            && let Ok(cookie) = xcb.get_selection_owner(atoms.XdndSelection)
            && cookie
                .reply()
                .is_ok_and(|owner| owner.owner == self.start.window)
        {
            xcb.set_selection_owner(NONE, atoms.XdndSelection, CURRENT_TIME)
                .log_err();
        }
        xcb.flush().log_err();
    }
}

fn self_drop_position(event: &xinput::ButtonPressEvent, scale: f32) -> Point<Pixels> {
    point(
        px(event.event_x as f32 / 65536.0 / scale),
        px(event.event_y as f32 / 65536.0 / scale),
    )
}

fn send(xcb: &XCBConnection, target: Target, message: xproto::Atom, data: [u32; 5]) {
    let event = ClientMessageEvent::new(32, target.window, message, data);
    xcb.send_event(false, target.proxy, EventMask::NO_EVENT, event)
        .log_err();
}

fn property(
    xcb: &XCBConnection,
    window: xproto::Window,
    atom: xproto::Atom,
    format: AtomEnum,
) -> Option<u32> {
    xcb.get_property(false, window, atom, format, 0, 1)
        .ok()?
        .reply()
        .ok()?
        .value32()?
        .next()
}

fn target_at(xcb: &XCBConnection, atoms: &XcbAtoms, pointer: DragPointer) -> Option<Target> {
    let mut window = pointer.root;
    let mut target = None;
    // Follow the window at the motion coordinates through the window manager's frame.
    loop {
        let proxy = property(xcb, window, atoms.XdndProxy, AtomEnum::WINDOW)
            .filter(|&proxy| {
                proxy != NONE
                    && property(xcb, proxy, atoms.XdndProxy, AtomEnum::WINDOW) == Some(proxy)
            })
            .unwrap_or(window);
        if let Some(version) =
            property(xcb, proxy, atoms.XdndAware, AtomEnum::ATOM).filter(|&version| version >= 3)
        {
            target = Some(Target {
                window,
                proxy,
                version: version.min(XDND_VERSION),
            });
        }
        let child = xcb
            .translate_coordinates(pointer.root, window, pointer.x, pointer.y)
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .map_or(NONE, |reply| reply.child);
        if child == NONE {
            return target;
        }
        window = child;
    }
}
