//! Cross-platform regression guards for FigCraft's native file-drag coordinate bridge.
//! As with pointer_patches.rs, source guards cover platforms unavailable on the test host;
//! the shared coordinate conversions are also exercised directly, without a display server.

use std::path::Path;

mod dpi {
    pub use winit::dpi::*;
}

#[path = "../src/platform_impl/file_drop_position.rs"]
mod file_drop_position;

#[path = "../src/platform_impl/linux/x11/file_drop_state.rs"]
mod file_drop_state;

fn source(path: &str) -> String {
    std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("src/platform_impl").join(path),
    )
    .unwrap()
}

fn section<'a>(text: &'a str, start: &str, end: &str) -> &'a str {
    text.split_once(start).unwrap().1.split_once(end).unwrap().0
}

fn before(text: &str, first: &str, second: &str) {
    assert!(text.find(first).unwrap() < text.find(second).unwrap());
}

#[test]
fn wayland_replays_saved_drop_point_after_async_uri_read() {
    let text = source("linux/wayland/seat/dnd.rs");
    let advance = section(&text, "fn advance_file_drag", "fn read_file_drag");
    before(
        advance,
        "self.file_drag_motion(drag.window_id, drag.position)",
        "WindowEvent::DroppedFile(path)",
    );
    let motion = section(&text, "    fn motion(", "    fn selection(");
    assert!(motion.contains("drag.position = LogicalPosition::new(x, y)"));
    assert!(motion.contains("self.file_drag_motion(window_id, LogicalPosition::new(x, y))"));
    let leave = section(&text, "    fn leave(", "    fn motion(");
    assert!(leave.contains("if !dropped"));
    assert!(leave.contains("self.cancel_file_drag(&seat)"));
}

#[test]
fn x11_translates_root_pixels_and_replays_the_frozen_point_before_files() {
    let text = source("linux/x11/event_processor.rs");
    let motion = section(
        &text,
        "if xev.message_type == atoms[XdndPosition]",
        "if xev.message_type == atoms[XdndDrop]",
    );
    before(motion, "if !accepted", "translate_coordinates(");
    assert!(motion.contains("self.dnd.position = Some(position)"));
    assert!(motion.contains("transfer.dropped()"));
    before(motion, "transfer.request(time)", "self.dnd.convert_selection(requestor, time)");
    let drop = section(
        &text,
        "if xev.message_type == atoms[XdndDrop]",
        "if xev.message_type == atoms[XdndLeave]",
    );
    before(drop, "transfer.drop_at(source_window, window, position)", "self.finish_file_drop(");
    assert!(!drop.contains("self.dnd.reset()"));
    let finish = section(&text, "fn finish_file_drop", "fn selection_notify");
    before(finish, "transfer.finish()", "self.dnd.reset()");
    before(finish, "self.dnd.reset()", "WindowEvent::CursorMoved");
    before(finish, "WindowEvent::CursorMoved", "WindowEvent::DroppedFile");
    assert!(finish.contains("dropped.position"));
    assert!(source("linux/x11/dnd.rs").contains("self.position = None"));
}

#[test]
fn x11_correlates_delayed_replies_and_cancels_only_the_matching_hover() {
    let text = source("linux/x11/event_processor.rs");
    let enter = section(
        &text,
        "if xev.message_type == atoms[XdndEnter]",
        "if xev.message_type == atoms[XdndPosition]",
    );
    assert!(enter.contains("self.dnd.begin_transfer(source_window, window)"));
    let leave = section(&text, "if xev.message_type == atoms[XdndLeave]", "fn finish_file_drop");
    assert!(leave.contains("transfer.matches(source_window, window) && !transfer.dropped()"));
    let reply = section(&text, "fn selection_notify", "fn configure_notify");
    assert!(reply.contains("xev.target != atoms[TextUriList]"));
    before(reply, "transfer.expects_reply(requestor, time)", "get_selection_owner(");
    before(reply, "owner != transfer.source()", "self.dnd.read_data(requestor)");
    before(reply, "transfer.receive_reply(requestor, time)", "self.dnd.read_data(requestor)");
    assert!(reply
        .contains("self.dnd.result = Some(result);\n        self.finish_file_drop(&mut callback)"));
}

#[test]
fn x11_each_transfer_owns_separate_property_storage_and_cleans_it_up() {
    let text = source("linux/x11/dnd.rs");
    let begin = section(&text, "fn begin_transfer(", "pub unsafe fn send_status(");
    before(begin, "self.reset()", "FileDropRequestor::new(");
    assert!(begin.contains("FileDropTransfer::new(source, target, requestor.window)"));
    let reset = section(&text, "pub fn reset(", "fn begin_transfer(");
    assert!(reset.contains("self.requestor = None"));
    let guard = section(&text, "impl FileDropRequestor", "impl Drop for FileDropRequestor");
    assert!(guard.contains("WindowClass::INPUT_ONLY"));
    assert!(guard.contains("cookie.check()?"));
    assert!(text.contains("destroy_window(self.window)"));
    let events = source("linux/x11/event_processor.rs");
    let reply = section(&events, "fn selection_notify", "fn configure_notify");
    assert!(reply.contains("let window = transfer.target()"));
    assert!(reply.contains("self.dnd.read_data(requestor)"));
}

#[test]
fn windows_com_uses_value_point_and_updates_only_file_gestures() {
    let definitions = source("windows/definitions.rs");
    let vtable = section(&definitions, "pub struct IDropTargetVtbl", "pub struct IDropTarget {");
    assert_eq!(vtable.matches("pt: POINTL").count(), 3);
    assert!(!vtable.contains("*const POINTL"));
    let text = source("windows/drop_handler.rs");
    assert_eq!(text.matches("pt: POINTL").count(), 3);
    let over = section(&text, "fn DragOver(", "fn DragLeave(");
    before(over, "if drop_handler.hovered_is_valid", "drop_handler.file_drag_motion(pt)");
    let drop = section(&text, "fn Drop(", "unsafe fn from_interface");
    before(drop, "drop_handler.file_drag_motion(pt)", "event: DroppedFile(filename)");
    assert!(drop.contains("drop_handler.hovered_is_valid = false"));
}

#[test]
fn cocoa_updates_motion_and_emits_actual_drop_point_before_files() {
    let text = source("macos/window_delegate.rs");
    let enter = section(&text, "fn dragging_entered(", "#[method(draggingUpdated:)]");
    assert!(enter.contains("-> usize"));
    before(enter, "self.file_drag_motion(sender)", "WindowEvent::HoveredFile(path)");
    let over = section(&text, "fn dragging_updated(", "#[method(prepareForDragOperation:)]");
    before(over, "propertyListForType", "self.file_drag_motion(sender)");
    let drop = section(&text, "fn perform_drag_operation(", "#[method(concludeDragOperation:)]");
    before(drop, "self.file_drag_motion(sender)", "WindowEvent::DroppedFile(path)");
    let helper = section(&text, "fn file_drag_motion(", "fn handle_scale_factor_changed(");
    assert!(helper.contains("draggingLocation"));
    assert!(helper.contains("convertPoint_fromView(window_point, None)"));
    assert!(helper.contains("file_drop_position::logical_position("));
}

#[test]
fn macos_webkit_cursor_uses_separate_hotspot_axes() {
    let text = source("macos/cursor.rs");
    let x = section(&text, "let mut x = 0.0", "let mut y = 0.0");
    let y = section(&text, "let mut y = 0.0", "let hotspot");
    assert!(x.contains("ns_string!(\"hotx\")"));
    assert!(y.contains("ns_string!(\"hoty\")"));
    assert!(!y.contains("ns_string!(\"hotx\")"));
    assert!(text.contains("CursorIcon::Move | CursorIcon::AllScroll => webkit_move()"));
    assert!(text.contains("CursorIcon::Cell => webkit_cell()"));
}
