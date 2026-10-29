//! Source guards for Wayland URI reader cleanup, without a compositor.

use std::path::Path;

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
fn wayland_failed_reads_release_the_buffer_and_remove_the_reader() {
    let text = source("linux/wayland/seat/dnd.rs");
    let read = section(&text, "fn read_file_drag", "fn read_uri_list_chunk");
    assert!(read.contains("match read_uri_list_chunk(pipe, &mut drag.buffer)"));
    let failure = read.rsplit_once("Err(error) => {").unwrap().1;
    for cleanup in
        ["drag.reader = None", "drag.paths = Some(Vec::new())", "drag.buffer = Vec::new()"]
    {
        before(failure, cleanup, "self.advance_file_drag(seat)");
    }
    before(failure, "self.advance_file_drag(seat)", "PostAction::Remove");
    let transient = section(read, "if matches!(error.kind()", "Err(error) => {");
    assert!(transient.contains("ErrorKind::Interrupted | ErrorKind::WouldBlock"));
    assert!(transient.contains("PostAction::Continue"));
}

#[test]
fn wayland_reader_setup_failures_withdraw_acceptance() {
    let text = source("linux/wayland/seat/dnd.rs");
    let reject = section(&text, "fn reject_file_offer", "pub struct FileDrag");
    before(reject, "offer.accept_mime_type(offer.serial, None)", "conn.flush()");
    assert!(!reject.contains("offer.finish("));
    assert!(!reject.contains("offer.destroy("));
    let enter = section(&text, "    fn enter(", "    fn leave(");
    for message in ["Failed to receive a dragged file list", "Failed to watch a dragged file list"]
    {
        let failure = section(enter, message, "return;");
        assert!(failure.contains("reject_file_offer(&offer, conn)"));
    }
    let missing_seat = section(enter, "let Some(seat_state)", "seat_state.file_drag = Some");
    before(missing_seat, "self.loop_handle.remove(reader)", "reject_file_offer(&offer, conn)");
}
