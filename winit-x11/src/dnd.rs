use std::collections::VecDeque;
use std::io;
use std::os::raw::*;
use std::str::Utf8Error;
use std::sync::Arc;
use std::sync::atomic::{AtomicI64, Ordering};

use winit_core::data_transfer::{DataTransfer, DataTransferId, TransferType, TypeHint, TypedData};
use winit_core::event_loop::AsyncRequestSerial;
use x11rb::protocol::xproto::{self, ConnectionExt};

use crate::atoms::AtomName::None as DndNone;
use crate::atoms::*;
use crate::event_loop::{CookieResultExt, X11Error};
use crate::xdisplay::XConnection;
use crate::{XWindow, util};

#[derive(Debug, Clone, Copy)]
pub enum DndState {
    Accepted,
    Rejected,
}

#[derive(Debug)]
#[non_exhaustive]
pub enum UriListParseError {
    EmptyData,
    InvalidUtf8(#[allow(dead_code)] Utf8Error),
    HostnameSpecified(#[allow(dead_code)] String),
    UnexpectedProtocol(#[allow(dead_code)] String),
    UnresolvablePath(#[allow(dead_code)] io::Error),
    Io(#[allow(dead_code)] io::Error),
}

impl From<Utf8Error> for UriListParseError {
    fn from(e: Utf8Error) -> Self {
        UriListParseError::InvalidUtf8(e)
    }
}

impl From<io::Error> for UriListParseError {
    fn from(e: io::Error) -> Self {
        UriListParseError::UnresolvablePath(e)
    }
}

#[derive(Debug)]
pub struct SelectionReader {
    type_: SelectionType,
    data: io::Result<Vec<u8>>,
}

impl SelectionReader {
    /// Completes a failed conversion with readable error state instead of losing its serial.
    pub fn failed(type_: SelectionType, error: io::Error) -> Self {
        Self { type_, data: Err(error) }
    }

    /// Borrows successful data while preserving a failed conversion's error on every access.
    fn bytes(&self) -> io::Result<&[u8]> {
        self.data.as_deref().map_err(|err| io::Error::new(err.kind(), err.to_string()))
    }
}

impl TypedData for SelectionReader {
    fn try_read(&self) -> Option<Box<dyn io::BufRead>> {
        Some(Box::new(io::Cursor::new(self.data.as_ref().ok()?.clone())))
    }

    fn type_(&self) -> &dyn TransferType {
        &self.type_
    }

    fn try_as_bytes(&self) -> io::Result<Vec<u8>> {
        self.bytes().map(ToOwned::to_owned)
    }

    fn try_as_string(&self) -> io::Result<String> {
        fn invalid_data<E>(err: E) -> io::Error
        where
            E: Into<Box<dyn std::error::Error + Send + Sync>>,
        {
            io::Error::new(io::ErrorKind::InvalidData, err)
        }

        fn decode_utf16_bytes(bytes: &[u8]) -> io::Result<String> {
            let utf16 = bytes
                .chunks_exact(2)
                .map(|chunk| {
                    let bytes: &[u8; 2] = chunk.try_into().unwrap();
                    u16::from_ne_bytes(*bytes)
                })
                .collect::<Vec<_>>();
            String::from_utf16(&utf16).map_err(invalid_data)
        }

        let data = self.bytes()?;
        match self.type_.hint() {
            Some(TypeHint::Plaintext) | Some(TypeHint::Html) => std::str::from_utf8(data)
                .map(|str| str.to_owned())
                .map_err(invalid_data)
                .or_else(|_| decode_utf16_bytes(data)),
            Some(TypeHint::UriList) => String::from_utf8(data.to_vec()).map_err(invalid_data),
            _ => Err(io::ErrorKind::InvalidData.into()),
        }
    }

    fn try_as_uris(&self) -> io::Result<Vec<String>> {
        if self.type_().hint() != Some(TypeHint::UriList) {
            return Err(io::ErrorKind::InvalidData.into());
        }

        Ok(self
            .try_as_string()?
            .split(['\n', '\r'])
            .filter(|s| !s.is_empty())
            .map(ToOwned::to_owned)
            .collect())
    }
}

#[derive(Debug)]
pub struct DragState {
    // Populated by XdndEnter event handler
    pub version: c_long,
    pub transfer_id: DataTransferId,
    pub types: Arc<[SelectionType]>,
    // Populated by Xdnd* event handlers
    pub source_window: xproto::Window,
    // Populated by Xdnd* event handlers
    pub target_window: xproto::Window,
    // Populated by `fetch_data_transfer`
    pub pending_fetch_types: VecDeque<(AsyncRequestSerial, SelectionType)>,
    /// Private property storage, unique to this drag, so canceled sources cannot overwrite it.
    pub requestor_window: xproto::Window,
    /// Timestamp of the selection conversion currently awaiting a reply.
    pub pending_request_time: Option<xproto::Timestamp>,
    /// Released target/source pair, retained until pending conversions have completed.
    pub finished: Option<(XWindow, XWindow)>,
    /// Whether the application accepted the drag operation.
    // Populated by `Window::accept_drag`/`Window::reject_drag`.
    pub accepted: bool,
}

impl Default for DragState {
    fn default() -> Self {
        static DATA_TRANSFER_ID: AtomicI64 = AtomicI64::new(0);

        Self {
            version: Default::default(),
            transfer_id: DataTransferId::from_raw(DATA_TRANSFER_ID.fetch_add(1, Ordering::Relaxed)),
            types: Default::default(),
            source_window: Default::default(),
            target_window: Default::default(),
            pending_fetch_types: Default::default(),
            requestor_window: Default::default(),
            pending_request_time: None,
            finished: None,
            accepted: Default::default(),
        }
    }
}

impl DragState {
    /// Matches only the current, not-yet-dropped source/target pair.
    pub fn matches_drag(&self, source: xproto::Window, target: xproto::Window) -> bool {
        self.source_window == source && self.target_window == target && self.finished.is_none()
    }

    /// Consumes one reply only after its requestor, requested type, and timestamp match.
    /// `CURRENT_TIME` requests have no known server timestamp to compare against.
    pub fn take_selection_reply(
        &mut self,
        requestor: xproto::Window,
        target_type: xproto::Atom,
        time: xproto::Timestamp,
    ) -> Option<(AsyncRequestSerial, SelectionType)> {
        if self.requestor_window != requestor
            || !self
                .pending_request_time
                .is_some_and(|requested| requested == x11rb::CURRENT_TIME || requested == time)
            || self.pending_fetch_types.front()?.1.atom() != target_type
        {
            return None;
        }

        self.pending_request_time = None;
        self.pending_fetch_types.pop_front()
    }
}

#[derive(Debug)]
pub struct Dnd {
    xconn: Arc<XConnection>,
    // If `None`, no drag operation is in progress.
    state: Option<DragState>,
    requestor: Option<SelectionRequestor>,
}

#[derive(Debug)]
pub struct Selection {
    types: Arc<[SelectionType]>,
}

impl Selection {
    pub(crate) fn new(types: Arc<[SelectionType]>) -> Selection {
        Selection { types }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct SelectionType {
    hint: Option<TypeHint>,
    atom: xproto::Atom,
}

impl SelectionType {
    pub(crate) fn new(atoms: &Atoms, atom: xproto::Atom) -> Self {
        let atom_to_hint = [
            // Files
            (atoms[TextUriList], TypeHint::UriList),
            // Plaintext
            (atoms[STRING], TypeHint::Plaintext),
            (atoms[UTF8_STRING], TypeHint::Plaintext),
            (atoms[TextPlain], TypeHint::Plaintext),
            (atoms[TextPlainCharsetUtf8], TypeHint::Plaintext),
            // HTML
            (atoms[TextHtml], TypeHint::Html),
            (atoms[TextHtmlCharsetUtf8], TypeHint::Html),
            // RTF
            (atoms[ApplicationRtf], TypeHint::Rtf),
            // Audio
            (atoms[AudioAac], TypeHint::Audio { extension_hint: Some("aac") }),
            (atoms[AudioAiff], TypeHint::Audio { extension_hint: Some("aif") }),
            (atoms[AudioFlac], TypeHint::Audio { extension_hint: Some("flac") }),
            (atoms[AudioVndWav], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioVndWave], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioWav], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioWave], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioXWav], TypeHint::Audio { extension_hint: Some("wav") }),
            (atoms[AudioOgg], TypeHint::Audio { extension_hint: Some("ogg") }),
            (atoms[AudioMpeg], TypeHint::Audio { extension_hint: Some("mp3") }),
            // Image
            (atoms[ImageBmp], TypeHint::Image { extension_hint: Some("bmp") }),
            (atoms[ImageGif], TypeHint::Image { extension_hint: Some("gif") }),
            (atoms[ImageJpeg], TypeHint::Image { extension_hint: Some("jpg") }),
            (atoms[ImagePjpeg], TypeHint::Image { extension_hint: Some("jpg") }),
            (atoms[ImagePng], TypeHint::Image { extension_hint: Some("png") }),
            (atoms[ImageRaw], TypeHint::Image { extension_hint: Some("raw") }),
            (atoms[ImageSvg], TypeHint::Image { extension_hint: Some("svg") }),
            (atoms[ImageTiff], TypeHint::Image { extension_hint: Some("tiff") }),
            (atoms[ImageWebp], TypeHint::Image { extension_hint: Some("webp") }),
            (atoms[ImageXIcon], TypeHint::Image { extension_hint: Some("ico") }),
        ];
        let hint =
            atom_to_hint.iter().find_map(|(haystack, hint)| (*haystack == atom).then_some(*hint));

        Self { hint, atom }
    }

    pub fn atom(&self) -> xproto::Atom {
        self.atom
    }
}

impl TransferType for SelectionType {
    fn hint(&self) -> Option<TypeHint> {
        self.hint
    }

    fn matches(&self, other: &dyn TransferType) -> bool {
        if let Some(other_mime) = other.cast_ref::<Self>() {
            *self == *other_mime
        } else {
            // If either hint is `None`, return false
            self.hint().is_some_and(|hint| other.hint() == Some(hint))
        }
    }
}

impl DataTransfer for Selection {
    fn for_each_available_type<'this>(
        &'this self,
        func: &'_ mut dyn FnMut(&'this dyn TransferType) -> std::ops::ControlFlow<()>,
    ) {
        let _ = self.types.iter().map(|mime| mime as &dyn TransferType).try_for_each(func);
    }
}

impl Dnd {
    pub fn new(xconn: Arc<XConnection>) -> Self {
        Dnd { xconn, state: None, requestor: None }
    }

    pub fn state(&self) -> Option<&DragState> {
        self.state.as_ref()
    }

    pub fn state_mut(&mut self) -> Option<&mut DragState> {
        self.state.as_mut()
    }

    pub fn find_type_by_hint(&self, hint: TypeHint) -> Option<&SelectionType> {
        self.state.as_ref()?.types.iter().find(|haystack| haystack.hint() == Some(hint))
    }

    /// Starts a drag with separate server-side storage, dropping any canceled requestor first.
    pub fn init_state(
        &mut self,
        version: c_long,
        source_window: xproto::Window,
        target_window: xproto::Window,
        types: Arc<[SelectionType]>,
    ) -> Result<&DragState, X11Error> {
        self.reset_state();
        let requestor = SelectionRequestor::new(Arc::clone(&self.xconn))?;
        let requestor_window = requestor.window;
        self.requestor = Some(requestor);
        Ok(self.state.insert(DragState {
            version,
            types,
            source_window,
            target_window,
            requestor_window,
            ..Default::default()
        }))
    }

    /// Cancels or completes the drag and releases its private selection properties.
    pub fn reset_state(&mut self) -> Option<DragState> {
        self.requestor = None;
        self.state.take()
    }

    pub unsafe fn send_finished(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let Some(state) = &self.state else {
            return Err(X11Error::UnexpectedNull(
                "Drag-and-drop state was not initialized (called `send_finished` before XdndEnter",
            ));
        };
        let (accepted, action) =
            if state.accepted { (1, atoms[XdndActionCopy]) } else { (0, atoms[DndNone]) };
        self.xconn
            .send_client_msg(target_window, target_window, atoms[XdndFinished] as _, None, [
                this_window,
                accepted,
                action as _,
                0,
                0,
            ])?
            .ignore_error();

        Ok(())
    }

    pub unsafe fn get_type_list(
        &self,
        source_window: xproto::Window,
    ) -> Result<Vec<xproto::Atom>, util::GetPropertyError> {
        let atoms = self.xconn.atoms();
        self.xconn.get_property(
            source_window,
            atoms[XdndTypeList],
            xproto::Atom::from(xproto::AtomEnum::ATOM),
        )
    }

    /// Requests the next queued type in storage isolated from canceled drag sources.
    pub fn convert_selection(&mut self, time: xproto::Timestamp, new_type: xproto::Atom) {
        let Some(state) = self.state.as_mut() else {
            return;
        };
        state.pending_request_time = Some(time);
        let requestor = state.requestor_window;
        let atoms = self.xconn.atoms();
        self.xconn
            .xcb_connection()
            // The property belongs to a private requestor, not the application's window.
            .convert_selection(
                requestor,
                atoms[XdndSelection],
                new_type,
                atoms[XdndSelection],
                time,
            )
            .expect_then_ignore_error("Failed to send XdndSelection event")
    }

    pub unsafe fn send_status(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
        status: DndState,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let (accepted, action) = match status {
            DndState::Accepted => (1, atoms[XdndActionCopy]),
            DndState::Rejected => (0, atoms[DndNone]),
        };
        self.xconn
            .send_client_msg(target_window, target_window, atoms[XdndStatus] as _, None, [
                this_window,
                accepted,
                0,
                0,
                action as _,
            ])?
            .ignore_error();

        Ok(())
    }

    pub fn read_data(
        &self,
        window: xproto::Window,
        type_: SelectionType,
    ) -> Result<SelectionReader, util::GetPropertyError> {
        let atoms = self.xconn.atoms();
        let type_atom = type_.atom();
        let bytes = self.xconn.get_property(window, atoms[XdndSelection], type_atom)?;

        Ok(SelectionReader { type_, data: Ok(bytes) })
    }
}

/// Owns a drag's unmapped requestor and destroys its properties on every terminal path.
#[derive(Debug)]
struct SelectionRequestor {
    xconn: Arc<XConnection>,
    window: xproto::Window,
}

impl SelectionRequestor {
    /// Creates an input-only window; the temporary wrapper cleans up if creation is rejected.
    fn new(xconn: Arc<XConnection>) -> Result<Self, X11Error> {
        let (window, cookie) = xproto::WindowWrapper::create_window_and_get_cookie(
            xconn.xcb_connection(),
            0,
            xconn.default_root().root,
            0,
            0,
            1,
            1,
            0,
            xproto::WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new(),
        )?;
        cookie.check()?;
        let window = window.into_window();
        Ok(Self { xconn, window })
    }
}

impl Drop for SelectionRequestor {
    fn drop(&mut self) {
        if let Ok(cookie) = self.xconn.xcb_connection().destroy_window(self.window) {
            cookie.ignore_error();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state(requestor_window: xproto::Window) -> DragState {
        DragState {
            source_window: 1,
            target_window: 2,
            requestor_window,
            pending_request_time: Some(42),
            pending_fetch_types: VecDeque::from([
                (AsyncRequestSerial::get(), SelectionType {
                    hint: Some(TypeHint::UriList),
                    atom: 3,
                }),
                (AsyncRequestSerial::get(), SelectionType {
                    hint: Some(TypeHint::Plaintext),
                    atom: 4,
                }),
            ]),
            ..Default::default()
        }
    }

    #[test]
    fn drag_messages_require_matching_source_and_target() {
        let state = state(10);
        assert!(state.matches_drag(1, 2));
        assert!(!state.matches_drag(9, 2));
        assert!(!state.matches_drag(1, 9));
    }

    #[test]
    fn dropped_transfer_ignores_duplicate_drop_motion_and_leave() {
        let mut state = state(10);
        state.finished = Some((2, 1));
        assert!(!state.matches_drag(1, 2));
        // The pending conversion remains valid after release.
        assert!(state.take_selection_reply(10, 3, 42).is_some());
        assert_eq!(state.finished, Some((2, 1)));
    }

    #[test]
    fn unrelated_reply_does_not_consume_pending_fetch() {
        let mut state = state(10);
        assert!(state.take_selection_reply(11, 3, 42).is_none());
        assert!(state.take_selection_reply(10, 4, 42).is_none());
        assert!(state.take_selection_reply(10, 3, 43).is_none());
        assert_eq!(state.pending_fetch_types.len(), 2);
        assert_eq!(state.pending_request_time, Some(42));
    }

    #[test]
    fn each_conversion_completes_once_before_next_type_is_requested() {
        let mut state = state(10);
        assert!(state.take_selection_reply(10, 3, 42).is_some());
        assert!(state.take_selection_reply(10, 3, 42).is_none());
        assert!(state.take_selection_reply(10, 4, 42).is_none());
        assert_eq!(state.pending_fetch_types.len(), 1);
        // A failed conversion uses the same completion path, so it cannot block the next type.
        state.pending_request_time = Some(43);
        assert!(state.take_selection_reply(10, 4, 43).is_some());
        assert!(state.pending_fetch_types.is_empty());
    }

    #[test]
    fn canceled_requestor_cannot_complete_later_drag() {
        let mut next = state(11);
        assert!(next.take_selection_reply(10, 3, 42).is_none());
        assert!(next.take_selection_reply(11, 3, 42).is_some());
    }

    #[test]
    fn current_time_conversion_accepts_the_server_timestamp() {
        let mut state = state(10);
        state.pending_request_time = Some(x11rb::CURRENT_TIME);
        assert!(state.take_selection_reply(10, 3, 123).is_some());
    }

    #[test]
    fn failed_conversion_reports_error_for_each_data_access() {
        let reader = SelectionReader::failed(
            SelectionType { hint: Some(TypeHint::UriList), atom: 3 },
            io::Error::other("Selection conversion refused"),
        );
        assert!(reader.try_read().is_none());
        assert!(reader.try_as_bytes().is_err());
        assert!(reader.try_as_string().is_err());
        assert!(reader.try_as_uris().is_err());
        assert_eq!(reader.type_().hint(), Some(TypeHint::UriList));
    }

    #[test]
    fn successful_selection_preserves_bytes_and_uris() {
        let data = b"file:///tmp/a%20b.png\r\nfile:///tmp/c.svg\r\n".to_vec();
        let reader = SelectionReader {
            type_: SelectionType { hint: Some(TypeHint::UriList), atom: 3 },
            data: Ok(data.clone()),
        };
        assert_eq!(reader.try_as_bytes().unwrap(), data);
        assert_eq!(reader.try_as_uris().unwrap(), ["file:///tmp/a%20b.png", "file:///tmp/c.svg"]);
    }
}
