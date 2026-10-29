use std::io;
use std::os::raw::*;
use std::path::{Path, PathBuf};
use std::str::Utf8Error;
use std::sync::Arc;

use percent_encoding::percent_decode;
use x11rb::protocol::xproto::{self, ConnectionExt};

use super::atoms::AtomName::None as DndNone;
use super::atoms::*;
use super::file_drop_state::FileDropTransfer;
use super::{util, CookieResultExt, X11Error, XConnection};
use crate::dpi::PhysicalPosition;

#[derive(Debug, Clone, Copy)]
pub enum DndState {
    Accepted,
    Rejected,
}

#[derive(Debug)]
pub enum DndDataParseError {
    EmptyData,
    InvalidUtf8(#[allow(dead_code)] Utf8Error),
    HostnameSpecified(#[allow(dead_code)] String),
    UnexpectedProtocol(#[allow(dead_code)] String),
    UnresolvablePath(#[allow(dead_code)] io::Error),
}

impl From<Utf8Error> for DndDataParseError {
    fn from(e: Utf8Error) -> Self {
        DndDataParseError::InvalidUtf8(e)
    }
}

impl From<io::Error> for DndDataParseError {
    fn from(e: io::Error) -> Self {
        DndDataParseError::UnresolvablePath(e)
    }
}

pub struct Dnd {
    xconn: Arc<XConnection>,
    // Populated by XdndEnter event handler
    pub version: Option<c_long>,
    pub type_list: Option<Vec<xproto::Atom>>,
    // Populated by XdndPosition event handler
    pub source_window: Option<xproto::Window>,
    pub position: Option<PhysicalPosition<f64>>,
    pub(super) transfer: Option<FileDropTransfer>,
    requestor: Option<FileDropRequestor>,
    // Populated by SelectionNotify event handler (triggered by XdndPosition event handler)
    pub result: Option<Result<Vec<PathBuf>, DndDataParseError>>,
}

impl Dnd {
    /// Creates idle XDND state; no requestor window is allocated until a gesture begins.
    pub fn new(xconn: Arc<XConnection>) -> Result<Self, X11Error> {
        Ok(Dnd {
            xconn,
            version: None,
            type_list: None,
            source_window: None,
            position: None,
            transfer: None,
            requestor: None,
            result: None,
        })
    }

    /// Cancels pending data and coordinates, destroying the gesture's private requestor window.
    pub fn reset(&mut self) {
        self.version = None;
        self.type_list = None;
        self.source_window = None;
        self.position = None;
        self.transfer = None;
        self.requestor = None;
        self.result = None;
    }

    /// Replaces any old gesture with isolated selection-property storage for this source/target.
    /// If requestor creation fails, the old transfer has already been released and state is idle.
    pub(super) fn begin_transfer(
        &mut self,
        source: xproto::Window,
        target: xproto::Window,
    ) -> Result<(), X11Error> {
        self.reset();
        // Each conversion owns separate property storage. A delayed, canceled source
        // cannot overwrite the next gesture's data before its SelectionNotify is read.
        let requestor = FileDropRequestor::new(Arc::clone(&self.xconn))?;
        self.transfer = Some(FileDropTransfer::new(source, target, requestor.window));
        self.requestor = Some(requestor);
        Ok(())
    }

    pub unsafe fn send_status(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
        state: DndState,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let (accepted, action) = match state {
            DndState::Accepted => (1, atoms[XdndActionPrivate]),
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

    pub unsafe fn send_finished(
        &self,
        this_window: xproto::Window,
        target_window: xproto::Window,
        state: DndState,
    ) -> Result<(), X11Error> {
        let atoms = self.xconn.atoms();
        let (accepted, action) = match state {
            DndState::Accepted => (1, atoms[XdndActionPrivate]),
            DndState::Rejected => (0, atoms[DndNone]),
        };
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

    pub unsafe fn convert_selection(&self, window: xproto::Window, time: xproto::Timestamp) {
        let atoms = self.xconn.atoms();
        self.xconn
            .xcb_connection()
            .convert_selection(
                window,
                atoms[XdndSelection],
                atoms[TextUriList],
                atoms[XdndSelection],
                time,
            )
            .expect_then_ignore_error("Failed to send XdndSelection event")
    }

    pub unsafe fn read_data(
        &self,
        window: xproto::Window,
    ) -> Result<Vec<c_uchar>, util::GetPropertyError> {
        let atoms = self.xconn.atoms();
        self.xconn.get_property(window, atoms[XdndSelection], atoms[TextUriList])
    }

    pub fn parse_data(&self, data: &mut [c_uchar]) -> Result<Vec<PathBuf>, DndDataParseError> {
        if !data.is_empty() {
            let mut path_list = Vec::new();
            let decoded = percent_decode(data).decode_utf8()?.into_owned();
            for uri in decoded.split("\r\n").filter(|u| !u.is_empty()) {
                // The format is specified as protocol://host/path
                // However, it's typically simply protocol:///path
                let path_str = if uri.starts_with("file://") {
                    let path_str = uri.replace("file://", "");
                    if !path_str.starts_with('/') {
                        // A hostname is specified
                        // Supporting this case is beyond the scope of my mental health
                        return Err(DndDataParseError::HostnameSpecified(path_str));
                    }
                    path_str
                } else {
                    // Only the file protocol is supported
                    return Err(DndDataParseError::UnexpectedProtocol(uri.to_owned()));
                };

                let path = Path::new(&path_str).canonicalize()?;
                path_list.push(path);
            }
            Ok(path_list)
        } else {
            Err(DndDataParseError::EmptyData)
        }
    }
}

/// An unmapped selection requestor, destroyed on cancellation/completion or Dnd drop.
struct FileDropRequestor {
    xconn: Arc<XConnection>,
    window: xproto::Window,
}

impl FileDropRequestor {
    /// Creates an unmapped input-only window; the temporary wrapper cleans up creation failure.
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

impl Drop for FileDropRequestor {
    /// Releases server-side property storage on cancellation, completion or backend teardown.
    fn drop(&mut self) {
        if let Ok(cookie) = self.xconn.xcb_connection().destroy_window(self.window) {
            cookie.ignore_error();
        }
    }
}
