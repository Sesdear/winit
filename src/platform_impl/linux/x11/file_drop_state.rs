//! Pure lifecycle state for a single XDND URI transfer.

use crate::dpi::PhysicalPosition;

/// The identities and frozen release point needed to deliver a completed transfer.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct PendingDrop {
    pub source: u32,
    pub target: u32,
    pub position: Option<PhysicalPosition<f64>>,
}

/// Correlates one URI reply with one release, allowing either event to arrive first.
#[derive(Debug)]
pub(super) struct FileDropTransfer {
    source: u32,
    target: u32,
    requestor: u32,
    request_time: Option<u32>,
    ready: bool,
    dropped: bool,
    position: Option<PhysicalPosition<f64>>,
    finished: bool,
}

impl FileDropTransfer {
    /// Starts a gesture whose private requestor window isolates it from canceled replies.
    pub fn new(source: u32, target: u32, requestor: u32) -> Self {
        Self {
            source,
            target,
            requestor,
            request_time: None,
            ready: false,
            dropped: false,
            position: None,
            finished: false,
        }
    }

    /// Accepts messages only from this unfinished gesture's source and destination.
    pub fn matches(&self, source: u32, target: u32) -> bool {
        !self.finished && self.source == source && self.target == target
    }

    /// Returns the source window that must retain ownership of the XDND selection.
    pub fn source(&self) -> u32 {
        self.source
    }

    /// Returns the destination window to receive hover and drop events.
    pub fn target(&self) -> u32 {
        self.target
    }

    /// Returns the private window whose property stores this conversion's URI reply.
    pub fn requestor(&self) -> u32 {
        self.requestor
    }

    /// Reports whether release has frozen the coordinates and disabled further requests.
    pub fn dropped(&self) -> bool {
        self.dropped
    }

    /// Do not send another conversion on each motion while its reply is pending.
    pub fn request(&mut self, time: u32) -> bool {
        if self.finished || self.dropped || self.ready || self.request_time.is_some() {
            return false;
        }
        self.request_time = Some(time);
        true
    }

    /// Validates requestor identity and timestamp before consuming selection data.
    /// A CurrentTime request has no known server timestamp, so only its time check is waived.
    pub fn expects_reply(&self, requestor: u32, time: u32) -> bool {
        !self.finished
            && !self.ready
            && self.requestor == requestor
            // XDND 0 supplies CurrentTime, whose actual server timestamp is not known here.
            && self.request_time.is_some_and(|requested| requested == 0 || requested == time)
    }

    /// Marks a matching conversion complete; rejects stale and duplicate replies.
    pub fn receive_reply(&mut self, requestor: u32, time: u32) -> bool {
        if !self.expects_reply(requestor, time) {
            return false;
        }
        self.request_time = None;
        self.ready = true;
        true
    }

    /// Freezes the first matching release point while allowing a pending URI reply to finish.
    pub fn drop_at(
        &mut self,
        source: u32,
        target: u32,
        position: Option<PhysicalPosition<f64>>,
    ) -> bool {
        if !self.matches(source, target) || self.dropped {
            return false;
        }
        self.dropped = true;
        self.position = position;
        true
    }

    /// Consumes a transfer exactly once, after both release and its URI reply have arrived.
    pub fn finish(&mut self) -> Option<PendingDrop> {
        if self.finished || !self.dropped || !self.ready {
            return None;
        }
        self.finished = true;
        Some(PendingDrop { source: self.source, target: self.target, position: self.position })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_reply_before_drop_waits_for_release() {
        let mut drag = FileDropTransfer::new(10, 20, 30);
        assert!(drag.request(100));
        assert!(!drag.request(101));
        assert!(drag.receive_reply(30, 100));
        assert!(drag.finish().is_none());
        assert!(drag.drop_at(10, 20, Some(PhysicalPosition::new(40.0, 50.0))));
        assert_eq!(drag.finish().unwrap().position, Some(PhysicalPosition::new(40.0, 50.0)));
    }

    #[test]
    fn fast_drop_waits_for_uri_reply_and_freezes_the_release_point() {
        let mut drag = FileDropTransfer::new(10, 20, 30);
        assert!(drag.request(100));
        assert!(drag.drop_at(10, 20, Some(PhysicalPosition::new(40.0, 50.0))));
        assert!(drag.finish().is_none());
        assert!(!drag.drop_at(10, 20, Some(PhysicalPosition::new(400.0, 500.0))));
        assert!(!drag.request(101));
        assert!(drag.receive_reply(30, 100));
        let dropped = drag.finish().unwrap();
        assert_eq!(
            dropped,
            PendingDrop {
                source: 10,
                target: 20,
                position: Some(PhysicalPosition::new(40.0, 50.0)),
            }
        );
    }

    #[test]
    fn duplicate_reply_and_drop_cannot_deliver_again() {
        let mut drag = FileDropTransfer::new(10, 20, 30);
        assert!(drag.request(100));
        assert!(drag.drop_at(10, 20, None));
        assert!(drag.receive_reply(30, 100));
        assert!(!drag.receive_reply(30, 100));
        assert!(drag.finish().is_some());
        assert!(drag.finish().is_none());
        assert!(!drag.drop_at(10, 20, None));
        assert!(!drag.request(200));
    }

    #[test]
    fn unrelated_sources_windows_and_reply_times_do_not_change_the_drag() {
        let mut drag = FileDropTransfer::new(10, 20, 30);
        assert!(drag.request(100));
        assert!(!drag.drop_at(11, 20, None));
        assert!(!drag.drop_at(10, 21, None));
        assert!(!drag.receive_reply(31, 100));
        assert!(!drag.receive_reply(30, 99));
        assert!(!drag.dropped());
        assert!(drag.matches(10, 20));
        assert_eq!(drag.source(), 10);
        assert_eq!(drag.target(), 20);
        assert_eq!(drag.requestor(), 30);
        assert!(drag.receive_reply(30, 100));
    }

    #[test]
    fn legacy_current_time_reply_is_consumed_only_once() {
        let mut drag = FileDropTransfer::new(10, 20, 30);
        assert!(drag.request(0));
        assert!(drag.drop_at(10, 20, None));
        assert!(drag.receive_reply(30, 100));
        assert!(!drag.receive_reply(30, 100));
        assert!(drag.finish().is_some());
        assert!(drag.finish().is_none());
    }

    #[test]
    fn canceled_transfer_cannot_supply_a_later_gesture() {
        let mut drag = FileDropTransfer::new(10, 20, 30);
        assert!(drag.request(100));
        assert!(drag.drop_at(10, 20, None));
        // Reset/leave or a new Enter discards the entire old transfer, not only its files.
        drag = FileDropTransfer::new(10, 20, 31);
        assert!(drag.request(200));
        assert!(drag.matches(10, 20));
        assert!(!drag.receive_reply(30, 100));
        assert!(!drag.receive_reply(30, 200));
        assert!(drag.receive_reply(31, 200));
        assert!(drag.finish().is_none());
    }
    #[test]
    fn canceled_requestor_cannot_overwrite_or_notify_the_next_gesture() {
        for time in [0, 100] {
            let mut old = FileDropTransfer::new(10, 20, 30);
            assert!(old.request(time));
            // Its property belongs to window 30, which is destroyed by Dnd::reset.
            let mut current = FileDropTransfer::new(10, 20, 31);
            assert!(current.request(time));
            assert!(current.drop_at(10, 20, Some(PhysicalPosition::new(4.0, 5.0))));
            assert!(!current.receive_reply(old.requestor(), time));
            assert!(current.finish().is_none());
            assert!(current.receive_reply(31, time));
            let dropped = current.finish().unwrap();
            assert_eq!(dropped.target, 20);
            assert_eq!(dropped.position, Some(PhysicalPosition::new(4.0, 5.0)));
        }
    }
}
