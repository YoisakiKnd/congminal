//! Real platform PTY, not a memory-only parser test. Child is this isolated test executable.
use crate::{
    pty::{Command, EventSink, Pty, PtyEvent},
    resources::{Budget, Scope},
    term::Terminal,
    vt::Parser,
};
use std::{
    io::{Read, Write},
    sync::{Arc, mpsc},
    time::{Duration, Instant},
};
#[test]
#[ignore = "only launched by native_pty_graphics_roundtrip; generated data, no user shell/config"]
fn graphics_wire_child() {
    assert_eq!(std::env::var("CONGMINAL_KITTY_CHILD").as_deref(), Ok("1"));
    #[cfg(unix)]
    unsafe {
        let mut state = std::mem::zeroed();
        assert_eq!(libc::tcgetattr(0, &mut state), 0);
        libc::cfmakeraw(&mut state);
        assert_eq!(libc::tcsetattr(0, libc::TCSANOW, &state), 0);
    }
    #[cfg(windows)]
    unsafe {
        use std::ffi::c_void;
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetStdHandle(n: u32) -> *mut c_void;
            fn GetConsoleMode(h: *mut c_void, m: *mut u32) -> i32;
            fn SetConsoleMode(h: *mut c_void, m: u32) -> i32;
        }
        let input = GetStdHandle(-10i32 as u32);
        let output = GetStdHandle(-11i32 as u32);
        let mut mode = 0;
        assert_ne!(GetConsoleMode(input, &mut mode), 0);
        assert_ne!(SetConsoleMode(input, (mode & !6) | 0x200), 0);
        assert_ne!(GetConsoleMode(output, &mut mode), 0);
        assert_ne!(SetConsoleMode(output, mode | 4), 0);
    }
    print!("\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\");
    std::io::stdout().flush().unwrap();
    let mut reply = Vec::new();
    while !reply.ends_with(b"\x1b\\") {
        let mut b = [0];
        std::io::stdin().read_exact(&mut b).unwrap();
        reply.push(b[0]);
        assert!(reply.len() < 128);
    }
    assert_eq!(reply, b"\x1b_Gi=31;OK\x1b\\");
    print!(
        "\x1b_Gq=2,i=42,a=T,U=1,f=32,t=d,s=2,v=1,m=1;/wAA\x1b\\\x1b_Gq=2,m=0;/wD/AP8=\x1b\\\x1b[38;2;0;0;42m\u{10eeee}\u{305}\u{305}\u{305}\u{10eeee}\x1b[39mGRAPHICS_CHILD_OK"
    );
    std::io::stdout().flush().unwrap();
}
#[test]
#[ignore = "real native PTY/ConPTY; explicit CI invocation, no desktop required"]
fn native_pty_graphics_roundtrip() {
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool.clone());
    let (send, recv) = mpsc::sync_channel(16);
    let sink: EventSink = Arc::new(move |event| send.send(event).is_ok());
    let command = Command {
        program: Some(std::env::current_exe().unwrap().to_string_lossy().into_owned()),
        args: vec![
            "--exact".into(),
            "graphics::native_tests::graphics_wire_child".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        env: vec![("CONGMINAL_KITTY_CHILD".into(), "1".into())],
        ..Command::default()
    };
    let mut pty = Pty::spawn(&command, 80, 24, sink).unwrap();
    let mut term = Terminal::new(80, 24);
    term.set_cell_pixels(8, 16);
    let mut parser = Parser::new();
    let deadline = Instant::now() + Duration::from_secs(12);
    let mut exit = None;
    let mut eof = false;
    let mut captured = Vec::new();
    let mut answered = false;
    while !(exit.is_some() && eof) && Instant::now() < deadline {
        match recv.recv_timeout(Duration::from_millis(100)) {
            Ok(PtyEvent::Data(bytes)) => {
                assert!(captured.len() + bytes.len() < 65536);
                captured.extend_from_slice(&bytes);
                parser.advance(&mut term, &bytes);
                let answer = term.take_response();
                if answer.windows(7).any(|s| s == b"Gi=31;O") {
                    answered = true;
                }
                if !answer.is_empty() {
                    pty.write(&answer).unwrap();
                }
            }
            Ok(PtyEvent::Exit(code)) => exit = Some(code),
            Ok(PtyEvent::Eof) => eof = true,
            Ok(PtyEvent::Error(e)) => panic!("PTY error: {e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("PTY events closed: {e}"),
        }
    }
    drop(pty);
    assert!(answered, "APC query did not reach the terminal: {}", String::from_utf8_lossy(&captured));
    assert_eq!(exit, Some(Some(0)), "child/response failed: {}", String::from_utf8_lossy(&captured));
    assert!(eof, "PTY did not finish");
    let placement = term.graphics.virtual_placement(42, 0).expect("ConPTY/PTY must preserve chunked APC");
    assert_eq!(term.graphics.tile(placement, (8, 16)).unwrap().pixel(0, 0), Some([255, 0, 0, 255]));
    assert!(
        (0..term.rows()).any(|r| term
            .visible_row(r)
            .cells
            .iter()
            .any(|c| c.c == crate::graphics::PLACEHOLDER && c.marks != 0)),
        "Unicode placeholders/diacritics were lost by the native PTY"
    );
    println!("NATIVE_KITTY_PTY query=OK chunked_rgba=OK unicode_placeholders=OK exit=0");
}
