use std::{mem::size_of, ptr};

// Reuse the generated C API; this test observer needs only terminal parsing and formatting.
use ghostty_vt::ffi;

struct Screen {
    terminal: ffi::GhosttyTerminal,
    formatter: ffi::GhosttyFormatter,
}

impl Drop for Screen {
    fn drop(&mut self) {
        unsafe {
            ffi::ghostty_formatter_free(self.formatter);
            ffi::ghostty_terminal_free(self.terminal);
        }
    }
}

pub fn text(output: &[u8], cols: u16, rows: u16) -> String {
    let mut screen = Screen {
        terminal: ptr::null_mut(),
        formatter: ptr::null_mut(),
    };
    let options = ffi::GhosttyFormatterTerminalOptions {
        size: size_of::<ffi::GhosttyFormatterTerminalOptions>(),
        emit: ffi::GhosttyFormatterFormat_GHOSTTY_FORMATTER_FORMAT_PLAIN,
        trim: true,
        extra: ffi::GhosttyFormatterTerminalExtra {
            size: size_of::<ffi::GhosttyFormatterTerminalExtra>(),
            screen: ffi::GhosttyFormatterScreenExtra {
                size: size_of::<ffi::GhosttyFormatterScreenExtra>(),
                ..Default::default()
            },
            ..Default::default()
        },
        ..Default::default()
    };
    unsafe {
        assert_eq!(
            ffi::ghostty_terminal_new(ptr::null(), &mut screen.terminal, cols, rows),
            ffi::GhosttyResult_GHOSTTY_SUCCESS
        );
        ffi::ghostty_terminal_vt_write(screen.terminal, output.as_ptr(), output.len());
        assert_eq!(
            ffi::ghostty_formatter_terminal_new(
                ptr::null(),
                &mut screen.formatter,
                screen.terminal,
                options,
            ),
            ffi::GhosttyResult_GHOSTTY_SUCCESS
        );
        let mut len = 0;
        let result =
            ffi::ghostty_formatter_format_buf(screen.formatter, ptr::null_mut(), 0, &mut len);
        assert!(matches!(
            result,
            ffi::GhosttyResult_GHOSTTY_SUCCESS | ffi::GhosttyResult_GHOSTTY_OUT_OF_SPACE
        ));
        let mut bytes = vec![0; len];
        assert_eq!(
            ffi::ghostty_formatter_format_buf(
                screen.formatter,
                bytes.as_mut_ptr(),
                bytes.len(),
                &mut len
            ),
            ffi::GhosttyResult_GHOSTTY_SUCCESS
        );
        String::from_utf8_lossy(&bytes[..len]).into_owned()
    }
}

pub struct CommittedSnapshot {
    pub text: String,
    pub cursor: Option<(u16, u16, bool)>,
}

pub fn committed_snapshot(output: &[u8], cols: u16, rows: u16) -> Option<CommittedSnapshot> {
    let mut terminal =
        ghostty_vt::Terminal::new(cols, rows, 0).expect("committed observer terminal");
    terminal.write(output);
    if terminal
        .mode_get(ghostty_vt::MODE_SYNCHRONIZED_OUTPUT)
        .expect("observer synchronized mode")
    {
        return None;
    }
    let text = terminal
        .read_text_viewport((0, 0), (cols - 1, u32::from(rows - 1)), true)
        .expect("committed text");
    let mut render = ghostty_vt::RenderState::new().expect("committed render state");
    render.update(&terminal).expect("committed cursor update");
    let cursor = render.cursor().expect("committed cursor");
    Some(CommittedSnapshot {
        text,
        cursor: cursor
            .viewport
            .map(|position| (position.x, position.y, cursor.visible)),
    })
}

#[test]
fn screen_text_reconstructs_partial_redraws() {
    let output = b"REMOTE_SURVIVED\x1b[1;8HSTILL_SELECTED";
    assert!(!output
        .windows(b"REMOTE_STILL_SELECTED".len())
        .any(|part| part == b"REMOTE_STILL_SELECTED"));
    assert!(text(output, 80, 24).contains("REMOTE_STILL_SELECTED"));
}

#[test]
fn committed_snapshot_pairs_text_and_cursor_after_synchronized_close() {
    let partial = b"BASE\x1b[?2026h\x1b[2J\x1b[HNEW\x1b[3;5H";
    assert!(committed_snapshot(partial, 80, 24).is_none());
    let mut closed = partial.to_vec();
    closed.extend_from_slice(b"\x1b[?2026l");
    let snapshot = committed_snapshot(&closed, 80, 24).expect("committed snapshot");
    assert!(snapshot.text.contains("NEW"));
    assert_eq!(snapshot.cursor, Some((4, 2, true)));
}
