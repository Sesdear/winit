//! Incremental reads for incoming Wayland data transfers.

use std::io::{self, Read};
use std::mem;

/// URI lists contain paths rather than file contents. One MiB allows thousands of paths
/// while bounding memory retained by a malicious or broken drag source.
const MAX_URI_LIST_BYTES: usize = 1024 * 1024;

/// Other MIME types can contain arbitrary file contents and retain their existing behavior.
pub(super) fn size_limit(mime_type: &str) -> Option<usize> {
    (mime_type.split(';').next().map(str::trim) == Some("text/uri-list"))
        .then_some(MAX_URI_LIST_BYTES)
}

/// Drain ready bytes without extending the buffer past the limit. `None` means WouldBlock;
/// EOF transfers ownership of the completed buffer. Errors are terminal: the caller must
/// report the failed transfer and remove its event source, releasing the pipe and buffer.
pub(super) fn read_available(
    reader: &mut impl Read,
    buffer: &mut Vec<u8>,
    limit: Option<usize>,
) -> io::Result<Option<Vec<u8>>> {
    let mut chunk = [0; 8192];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => return Ok(Some(mem::take(buffer))),
            Ok(count) => {
                if buffer
                    .len()
                    .checked_add(count)
                    .is_none_or(|size| limit.is_some_and(|limit| size > limit))
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "Wayland URI list exceeds the size limit",
                    ));
                }
                buffer.extend_from_slice(&chunk[..count]);
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uri_list_limit_does_not_limit_file_contents() {
        assert_eq!(size_limit("text/uri-list"), Some(MAX_URI_LIST_BYTES));
        assert_eq!(size_limit("text/uri-list; charset=utf-8"), Some(MAX_URI_LIST_BYTES));
        assert_eq!(size_limit("image/png"), None);
    }

    #[test]
    fn boundary_sizes_and_eof() {
        for size in [0, MAX_URI_LIST_BYTES - 1, MAX_URI_LIST_BYTES] {
            let bytes = vec![b'x'; size];
            let mut buffer = Vec::new();
            let result =
                read_available(&mut bytes.as_slice(), &mut buffer, Some(MAX_URI_LIST_BYTES))
                    .unwrap();
            assert_eq!(result, Some(bytes));
            assert!(buffer.is_empty());
        }
    }

    #[test]
    fn rejects_before_appending_oversized_chunk() {
        let bytes = vec![b'x'; MAX_URI_LIST_BYTES + 1];
        let mut buffer = Vec::new();
        let error = read_available(&mut bytes.as_slice(), &mut buffer, Some(MAX_URI_LIST_BYTES))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(buffer.len(), MAX_URI_LIST_BYTES);
    }

    #[test]
    fn would_block_preserves_partial_data_and_interrupted_retries() {
        let mut buffer = b"file:///".to_vec();
        let mut reader = ScriptedReader(0);
        assert_eq!(read_available(&mut reader, &mut buffer, Some(32)).unwrap(), None);
        assert_eq!(buffer, b"file:///tmp/test\r\n");
        assert_eq!(
            read_available(&mut reader, &mut buffer, Some(32)).unwrap(),
            Some(b"file:///tmp/test\r\n".to_vec()),
        );
    }

    #[test]
    fn terminal_read_errors_propagate() {
        struct FailedReader;
        impl Read for FailedReader {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(io::ErrorKind::ConnectionReset.into())
            }
        }
        let error = read_available(&mut FailedReader, &mut Vec::new(), Some(32)).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    }

    #[test]
    fn terminal_reads_remove_the_source_and_release_captured_resources() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        use calloop::{EventLoop, PostAction};
        use rustix::pipe::{PipeFlags, pipe_with};
        use sctk::data_device_manager::ReadPipe;

        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        for limit in [Some(3), Some(4)] {
            let (readfd, writefd) = pipe_with(PipeFlags::CLOEXEC | PipeFlags::NONBLOCK).unwrap();
            assert_eq!(rustix::io::write(&writefd, b"test").unwrap(), 4);
            drop(writefd);
            let released = Arc::new(AtomicBool::new(false));
            let guard = Guard(released.clone());
            let mut event_loop = EventLoop::<Vec<io::Result<Vec<u8>>>>::try_new().unwrap();
            let mut buffer = Vec::new();
            event_loop
                .handle()
                .insert_source(ReadPipe::from(readfd), move |_, file, results| {
                    let _guard = &guard;
                    // SAFETY: The file is read in place and never replaced.
                    let file = unsafe { file.get_mut() };
                    match read_available(file, &mut buffer, limit) {
                        Ok(None) => PostAction::Continue,
                        result => {
                            results.push(result.map(Option::unwrap));
                            PostAction::Remove
                        },
                    }
                })
                .unwrap();
            let mut results = Vec::new();
            event_loop.dispatch(Duration::from_secs(1), &mut results).unwrap();
            assert_eq!(results.len(), 1);
            assert_eq!(results[0].is_ok(), limit == Some(4));
            assert!(released.load(Ordering::SeqCst));
            event_loop.dispatch(Duration::ZERO, &mut results).unwrap();
            assert_eq!(results.len(), 1);
        }
    }

    struct ScriptedReader(usize);

    impl Read for ScriptedReader {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            match self.0 {
                1 => Err(io::ErrorKind::Interrupted.into()),
                2 => {
                    let bytes = b"tmp/test\r\n";
                    out[..bytes.len()].copy_from_slice(bytes);
                    Ok(bytes.len())
                },
                3 => Err(io::ErrorKind::WouldBlock.into()),
                _ => Ok(0),
            }
        }
    }
}
