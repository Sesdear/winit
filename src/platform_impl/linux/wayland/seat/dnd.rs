//! File drag and drop through the core `wl_data_device` protocol.
//!
//! A drag that offers `text/uri-list` is accepted with the copy action. Its list is read as soon
//! as it enters a window, so the window hears `HoveredFile` for each local file while the drag is
//! over it, `HoveredFileCancelled` when it leaves, and `DroppedFile` for each file once dropped.

use std::ffi::OsStr;
use std::fs::File;
use std::io::{ErrorKind, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use percent_encoding::percent_decode;
use tracing::warn;

use sctk::data_device_manager::data_device::{DataDeviceData, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::DataSourceHandler;
use sctk::reexports::calloop::{PostAction, RegistrationToken};
use sctk::reexports::client::backend::ObjectId;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Proxy, QueueHandle};

use crate::dpi::LogicalPosition;
use crate::event::WindowEvent;
use crate::platform_impl::wayland::state::WinitState;
use crate::platform_impl::wayland::{make_wid, WindowId};

const URI_LIST: &str = "text/uri-list";

/// Allow thousands of local file URIs, but bound memory held by an untrusted drag source.
const MAX_URI_LIST_BYTES: usize = 1024 * 1024;

/// Withdraw acceptance when no reader can service this offer. Before a drop, finish is not
/// permitted: rejecting the MIME type and flushing matches unsupported-MIME handling.
fn reject_file_offer(offer: &DragOffer, conn: &Connection) {
    offer.accept_mime_type(offer.serial, None);
    let _ = conn.flush();
}

/// A file drag over one of our windows.
#[derive(Debug)]
pub struct FileDrag {
    offer: DragOffer,
    window_id: WindowId,
    /// Latest surface-local logical position, retained while URI data is read asynchronously.
    position: LogicalPosition<f64>,
    /// The files in the drag, once the list has been read.
    paths: Option<Vec<PathBuf>>,
    /// Bytes of the list read so far.
    buffer: Vec<u8>,
    /// The source reading the list.
    reader: Option<RegistrationToken>,
    /// Whether `HoveredFile` was sent for this drag.
    hovered: bool,
    /// Whether the drag was dropped on the window.
    dropped: bool,
}

impl WinitState {
    /// Emits a physical cursor point using the receiving surface's current scale factor.
    fn file_drag_motion(&mut self, window_id: WindowId, position: LogicalPosition<f64>) {
        let Some(window) = self.windows.get_mut().get(&window_id) else { return };
        let Ok(window) = window.lock() else { return };
        let Some(position) = crate::platform_impl::file_drop_position::logical_position(
            position.x,
            position.y,
            window.scale_factor(),
        ) else {
            return;
        };
        let device_id = crate::event::DeviceId(crate::platform_impl::DeviceId::Wayland(
            crate::platform_impl::wayland::DeviceId,
        ));
        self.events_sink
            .push_window_event(WindowEvent::CursorMoved { device_id, position }, window_id);
        self.dispatched_events = true;
    }

    /// Ends the file drag of a seat without dropping it.
    pub(super) fn cancel_file_drag(&mut self, seat: &ObjectId) {
        let Some(drag) = self.seats.get_mut(seat).and_then(|seat| seat.file_drag.take()) else {
            return;
        };
        if let Some(reader) = drag.reader {
            self.loop_handle.remove(reader);
        }
        if drag.hovered {
            self.events_sink.push_window_event(WindowEvent::HoveredFileCancelled, drag.window_id);
        }
        self.dispatched_events = true;
    }

    /// Reports what is known about a seat's file drag: hovered files once they are read, and
    /// dropped files once the drag is dropped and read, which also finishes it.
    fn advance_file_drag(&mut self, seat: &ObjectId) {
        let Some(seat_state) = self.seats.get_mut(seat) else { return };
        let Some(drag) = seat_state.file_drag.as_mut() else { return };
        let Some(paths) = drag.paths.as_ref() else { return };

        if !drag.dropped {
            if !drag.hovered {
                drag.hovered = true;
                for path in paths {
                    self.events_sink
                        .push_window_event(WindowEvent::HoveredFile(path.clone()), drag.window_id);
                }
                self.dispatched_events = true;
            }
            return;
        }

        let drag = seat_state.file_drag.take().unwrap();
        // The offer carries the action the compositor settled on; finishing without one is a
        // protocol error.
        let selected = seat_state
            .data_device
            .as_ref()
            .and_then(|device| device.data().drag_offer())
            .filter(|offer| offer.inner() == drag.offer.inner())
            .map_or(drag.offer.selected_action, |offer| offer.selected_action);
        if selected == DndAction::Copy {
            drag.offer.finish();
        }
        drag.offer.destroy();

        // Reading the URI list may finish after a leave or another pointer event. Restore the
        // actual drop position immediately before delivering the corresponding files.
        self.file_drag_motion(drag.window_id, drag.position);
        for path in drag.paths.unwrap_or_default() {
            self.events_sink.push_window_event(WindowEvent::DroppedFile(path), drag.window_id);
        }
        self.dispatched_events = true;
    }

    /// Reads one ready URI chunk, keeping the source registered only while more data is needed.
    /// Permanent errors, including the size limit, release bytes and complete with an empty list;
    /// PostAction::Remove deregisters and closes the reader even when a drop is already pending.
    fn read_file_drag(&mut self, seat: &ObjectId, pipe: &File) -> PostAction {
        let Some(drag) = self.seats.get_mut(seat).and_then(|seat| seat.file_drag.as_mut()) else {
            return PostAction::Remove;
        };

        match read_uri_list_chunk(pipe, &mut drag.buffer) {
            Ok(true) => {
                drag.reader = None;
                drag.paths = Some(parse_uri_list(&drag.buffer));
                drag.buffer = Vec::new();
                self.advance_file_drag(seat);
                PostAction::Remove
            },
            Ok(false) => PostAction::Continue,
            Err(error)
                if matches!(error.kind(), ErrorKind::Interrupted | ErrorKind::WouldBlock) =>
            {
                PostAction::Continue
            },
            Err(error) => {
                warn!("Failed to read a dragged file list: {error}");
                drag.reader = None;
                drag.paths = Some(Vec::new());
                drag.buffer = Vec::new();
                self.advance_file_drag(seat);
                // Removing the event source also closes its pipe; do not keep watching a
                // source whose URI list failed validation or exceeded the size limit.
                PostAction::Remove
            },
        }
    }
}

/// Reads one ready chunk, returning true at EOF. Rejects oversized lists before growing the
/// buffer; the caller handles this like a read error and removes the event-loop source.
fn read_uri_list_chunk(mut pipe: impl Read, buffer: &mut Vec<u8>) -> std::io::Result<bool> {
    let mut chunk = [0u8; 4096];
    let read = pipe.read(&mut chunk)?;
    if read == 0 {
        return Ok(true);
    }
    if buffer.len().checked_add(read).map_or(true, |size| size > MAX_URI_LIST_BYTES) {
        return Err(std::io::Error::new(ErrorKind::InvalidData, "dragged URI list exceeds 1 MiB"));
    }
    buffer.extend_from_slice(&chunk[..read]);
    Ok(false)
}

impl DataDeviceHandler for WinitState {
    /// Accept file offers only with a registered reader; setup failures withdraw MIME
    /// acceptance and flush without finishing an offer that has not been dropped.
    fn enter(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        let data = data_device.data::<DataDeviceData>().unwrap();
        let seat = data.seat().id();
        self.cancel_file_drag(&seat);

        let Some(offer) = data.drag_offer() else { return };
        let window_id = make_wid(surface);
        let offers_files = offer.with_mime_types(|types| types.iter().any(|mime| mime == URI_LIST));
        if !offers_files || !self.windows.get_mut().contains_key(&window_id) {
            reject_file_offer(&offer, conn);
            return;
        }

        offer.accept_mime_type(offer.serial, Some(URI_LIST.to_owned()));
        offer.set_actions(DndAction::Copy, DndAction::Copy);
        let pipe = match offer.receive(URI_LIST.to_owned()) {
            Ok(pipe) => pipe,
            Err(error) => {
                warn!("Failed to receive a dragged file list: {error}");
                reject_file_offer(&offer, conn);
                return;
            },
        };
        // The source only writes once the compositor has our request.
        let _ = conn.flush();

        let reader_seat = seat.clone();
        let reader = self
            .loop_handle
            .insert_source(pipe, move |_, file, state| state.read_file_drag(&reader_seat, file));
        let reader = match reader {
            Ok(reader) => reader,
            Err(error) => {
                warn!("Failed to watch a dragged file list: {error}");
                reject_file_offer(&offer, conn);
                return;
            },
        };

        let Some(seat_state) = self.seats.get_mut(&seat) else {
            self.loop_handle.remove(reader);
            reject_file_offer(&offer, conn);
            return;
        };
        seat_state.file_drag = Some(FileDrag {
            offer,
            window_id,
            position: LogicalPosition::new(x, y),
            paths: None,
            buffer: Vec::new(),
            reader: Some(reader),
            hovered: false,
            dropped: false,
        });
        self.file_drag_motion(window_id, LogicalPosition::new(x, y));
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, data_device: &WlDataDevice) {
        let seat = data_device.data::<DataDeviceData>().unwrap().seat().id();
        // A drop is followed by a leave; that drag stays until its files are delivered.
        let dropped = self
            .seats
            .get(&seat)
            .and_then(|seat| seat.file_drag.as_ref())
            .is_some_and(|drag| drag.dropped);
        if !dropped {
            self.cancel_file_drag(&seat);
        }
    }

    /// Retains the surface-local point during asynchronous reading and reaffirms MIME acceptance
    /// without restarting action negotiation, which can interrupt a quick drop.
    fn motion(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
        x: f64,
        y: f64,
    ) {
        let seat = data_device.data::<DataDeviceData>().unwrap().seat().id();
        // Some compositors want the type accepted again as the drag moves. The actions are not
        // set again: that restarts the negotiation and can turn a quick drop into a leave.
        if let Some(drag) = self.seats.get_mut(&seat).and_then(|seat| seat.file_drag.as_mut()) {
            drag.position = LogicalPosition::new(x, y);
            let window_id = drag.window_id;
            drag.offer.accept_mime_type(drag.offer.serial, Some(URI_LIST.to_owned()));
            let _ = conn.flush();
            self.file_drag_motion(window_id, LogicalPosition::new(x, y));
        }
    }

    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, data_device: &WlDataDevice) {
        let data = data_device.data::<DataDeviceData>().unwrap();
        let seat = data.seat().id();
        let offer = data.selection_offer();
        self.clipboard.regular_selection_changed(&seat, offer);
    }

    fn drop_performed(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        data_device: &WlDataDevice,
    ) {
        let data = data_device.data::<DataDeviceData>().unwrap();
        let seat = data.seat().id();
        let current = data.drag_offer();
        let Some(drag) = self.seats.get_mut(&seat).and_then(|seat| seat.file_drag.as_mut()) else {
            return;
        };
        if let Some(current) = current.filter(|current| current.inner() == drag.offer.inner()) {
            drag.offer = current;
        }
        drag.dropped = true;
        self.advance_file_drag(&seat);
    }
}

impl DataOfferHandler for WinitState {
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }

    fn selected_action(
        &mut self,
        conn: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        _: DndAction,
    ) {
        let ours = self
            .seats
            .values_mut()
            .filter_map(|seat| seat.file_drag.as_mut())
            .find(|drag| drag.offer.inner() == offer.inner());
        if let Some(drag) = ours {
            drag.offer.selected_action = offer.selected_action;
            offer.accept_mime_type(offer.serial, Some(URI_LIST.to_owned()));
            let _ = conn.flush();
        }
    }
}

// Winit offers data only for the clipboard.
impl DataSourceHandler for WinitState {
    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        source: &WlDataSource,
        mime: String,
        pipe: sctk::data_device_manager::WritePipe,
    ) {
        self.clipboard.send(source, &mime, pipe);
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.clipboard.source_cancelled(source);
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

sctk::delegate_data_device!(WinitState);

/// The local files in a `text/uri-list` (RFC 2483): one URI per line, `#` starting a comment.
/// Only `file` URIs naming this machine (no host, or `localhost`) become paths.
fn parse_uri_list(list: &[u8]) -> Vec<PathBuf> {
    list.split(|&byte| byte == b'\n')
        .map(trim_ascii_whitespace)
        .filter(|line| !line.is_empty() && !line.starts_with(b"#"))
        .filter_map(file_uri_path)
        .collect()
}

fn trim_ascii_whitespace(mut line: &[u8]) -> &[u8] {
    while let [first, rest @ ..] = line {
        if !first.is_ascii_whitespace() {
            break;
        }
        line = rest;
    }
    while let [rest @ .., last] = line {
        if !last.is_ascii_whitespace() {
            break;
        }
        line = rest;
    }
    line
}

fn file_uri_path(uri: &[u8]) -> Option<PathBuf> {
    let (scheme, rest) = uri.split_at(uri.iter().position(|&byte| byte == b':')?);
    if !scheme.eq_ignore_ascii_case(b"file") {
        return None;
    }
    let rest = &rest[1..];
    let path = match rest.strip_prefix(b"//") {
        Some(authority_and_path) => {
            let slash = authority_and_path.iter().position(|&byte| byte == b'/')?;
            let (host, path) = authority_and_path.split_at(slash);
            if !host.is_empty() && !host.eq_ignore_ascii_case(b"localhost") {
                return None;
            }
            path
        },
        // `file:/path`, which some sources write.
        None if rest.starts_with(b"/") => rest,
        None => return None,
    };
    // Drop a query or fragment, which a file path cannot hold unescaped.
    let end = path.iter().position(|&byte| byte == b'?' || byte == b'#').unwrap_or(path.len());
    let decoded: Vec<u8> = percent_decode(&path[..end]).collect();
    if decoded.contains(&0) {
        return None;
    }
    Some(PathBuf::from(OsStr::from_bytes(&decoded)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_accepts_empty_and_boundary_sizes() {
        for size in [0, MAX_URI_LIST_BYTES - 1, MAX_URI_LIST_BYTES] {
            // Padding is a comment, so the exact-limit case still exercises URI parsing.
            let mut input = b"file:///tmp/kept\n#".to_vec();
            input.resize(size, b'x');
            let mut pipe = input.as_slice();
            let mut buffer = Vec::new();
            while !read_uri_list_chunk(&mut pipe, &mut buffer).unwrap() {}
            assert_eq!(buffer, input);
            let expected = if size == 0 { Vec::new() } else { vec![PathBuf::from("/tmp/kept")] };
            assert_eq!(parse_uri_list(&buffer), expected);
        }
    }

    #[test]
    fn uri_list_rejects_oversized_stream_without_appending_the_excess() {
        for size in [MAX_URI_LIST_BYTES + 1, MAX_URI_LIST_BYTES + 4096] {
            let input = vec![b'x'; size];
            let mut pipe = input.as_slice();
            let mut buffer = Vec::new();
            loop {
                let before = buffer.len();
                match read_uri_list_chunk(&mut pipe, &mut buffer) {
                    Ok(false) => assert!(buffer.len() <= MAX_URI_LIST_BYTES),
                    Ok(true) => panic!("oversized input was accepted"),
                    Err(error) => {
                        assert_eq!(error.kind(), ErrorKind::InvalidData);
                        assert_eq!(buffer.len(), before);
                        assert_eq!(buffer.len(), MAX_URI_LIST_BYTES);
                        break;
                    },
                }
            }
        }
    }

    #[test]
    fn uri_list_rejects_a_chunk_crossing_the_limit_atomically() {
        let mut buffer = vec![b'x'; MAX_URI_LIST_BYTES - 1];
        let before = buffer.clone();
        let error = read_uri_list_chunk(&b"ab"[..], &mut buffer).unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidData);
        assert_eq!(buffer, before);
    }

    #[test]
    fn uri_list_preserves_read_errors_without_appending() {
        struct FailedRead(ErrorKind);
        impl Read for FailedRead {
            fn read(&mut self, _: &mut [u8]) -> std::io::Result<usize> {
                Err(std::io::Error::from(self.0))
            }
        }
        for kind in [ErrorKind::Interrupted, ErrorKind::WouldBlock, ErrorKind::BrokenPipe] {
            let mut buffer = b"file:///tmp/kept\n".to_vec();
            let before = buffer.clone();
            assert_eq!(
                read_uri_list_chunk(FailedRead(kind), &mut buffer).unwrap_err().kind(),
                kind
            );
            assert_eq!(buffer, before);
        }
    }

    fn paths(list: &str) -> Vec<PathBuf> {
        parse_uri_list(list.as_bytes())
    }

    #[test]
    fn reads_file_uris() {
        assert_eq!(paths("file:///home/me/a.txt\r\nfile:///tmp/b%20c.png\r\n"), [
            PathBuf::from("/home/me/a.txt"),
            PathBuf::from("/tmp/b c.png")
        ]);
    }

    #[test]
    fn accepts_bare_newlines_and_a_missing_final_one() {
        assert_eq!(paths("file:///a\nfile:///b"), [PathBuf::from("/a"), PathBuf::from("/b")]);
    }

    #[test]
    fn skips_comments_blank_lines_and_other_schemes() {
        assert_eq!(
            paths("# from a file manager\r\n\r\nhttps://example.com/x\r\nfile:///kept\r\n"),
            [PathBuf::from("/kept")]
        );
    }

    #[test]
    fn accepts_localhost_and_skips_other_hosts() {
        assert_eq!(paths("file://localhost/a\r\nfile://LOCALHOST/b\r\nfile://elsewhere/c\r\n"), [
            PathBuf::from("/a"),
            PathBuf::from("/b")
        ]);
    }

    #[test]
    fn decodes_utf8_and_non_utf8_bytes() {
        assert_eq!(paths("file:///na%C3%AFve%2Bx"), [PathBuf::from("/naïve+x")]);
        assert_eq!(paths("file:///raw%FF"), [PathBuf::from(OsStr::from_bytes(b"/raw\xff"))]);
    }

    #[test]
    fn skips_escaped_nul_and_malformed_uris() {
        assert!(paths("file:///a%00b\r\nfile:\r\nfile://host-without-path\r\n").is_empty());
    }

    #[test]
    fn accepts_the_short_file_form_and_drops_fragments() {
        assert_eq!(paths("file:/a/b#frag"), [PathBuf::from("/a/b")]);
        assert_eq!(paths("FILE:///%23literal"), [PathBuf::from("/#literal")]);
    }
}
