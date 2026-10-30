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
    // The real picker also requires a cell-size response; Kitty OK alone would
    // still make it fall back to half-blocks, especially through ConPTY.
    print!("\x1b[c\x1b[16t\x1b]11;?\x07\x1b[5n");
    std::io::stdout().flush().unwrap();
    reply.clear();
    while !reply.ends_with(b"\x1b[0n") {
        let mut b = [0];
        std::io::stdin().read_exact(&mut b).unwrap();
        reply.push(b[0]);
        assert!(reply.len() < 1024);
    }
    assert!(reply.windows(9).any(|s| s == b"\x1b[6;16;8t"), "picker lost cell metrics: {reply:?}");
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
    #[cfg(windows)]
    assert!(pty.modern_backend(), "CI must test the bundled modern ConPTY, not the system fallback");
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

#[test]
#[ignore = "requires the separately built real ratatui-image fixture, no user configuration"]
fn real_picker_without_forced_console_modes() {
    let probe = std::env::var("CONGMINAL_KITTY_PROBE").expect("fixture binary path");
    let pool = Budget::new(64 * 1024 * 1024, 0);
    let _scope = Scope::enter(pool);
    let (send, recv) = mpsc::sync_channel(16);
    let sink: EventSink = Arc::new(move |e| send.send(e).is_ok());
    let command = if std::env::var("CONGMINAL_KITTY_VIA_PWSH").as_deref() == Ok("1") {
        Command {
            program: Some("pwsh.exe".into()),
            args: vec![
                "-NoLogo".into(),
                "-NoProfile".into(),
                "-Command".into(),
                "& $env:CONGMINAL_KITTY_PROBE --probe; exit $LASTEXITCODE".into(),
            ],
            ..Command::default()
        }
    } else {
        Command { program: Some(probe), args: vec!["--probe".into()], ..Command::default() }
    };
    let mut pty = Pty::spawn(&command, 80, 24, sink).unwrap();
    #[cfg(windows)]
    assert!(pty.modern_backend());
    let mut term = Terminal::new(80, 24);
    term.set_cell_pixels(8, 16);
    let mut parser = Parser::new();
    let mut captured = Vec::new();
    let mut exit = None;
    let mut eof = false;
    let deadline = Instant::now() + Duration::from_secs(12);
    while !(exit.is_some() && eof) && Instant::now() < deadline {
        match recv.recv_timeout(Duration::from_millis(100)) {
            Ok(PtyEvent::Data(data)) => {
                assert!(captured.len() + data.len() < 65536);
                captured.extend_from_slice(&data);
                parser.advance(&mut term, &data);
                let reply = term.take_response();
                if !reply.is_empty() {
                    pty.write(&reply).unwrap();
                }
            }
            Ok(PtyEvent::Exit(code)) => exit = Some(code),
            Ok(PtyEvent::Eof) => eof = true,
            Ok(PtyEvent::Error(e)) => panic!("{e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("{e}"),
        }
    }
    drop(pty);
    let captured = String::from_utf8_lossy(&captured);
    println!("REAL_PICKER_WIRE {captured:?}");
    assert_eq!(exit, Some(Some(0)), "real picker failed: {captured:?}");
    assert!(eof);
    assert!(captured.contains("REAL_PICKER protocol=Kitty"), "{captured:?}");
}

#[cfg(windows)]
#[test]
#[ignore = "isolated native cost child"]
fn conpty_cost_child() {
    assert_eq!(std::env::var("CONGMINAL_COST_CHILD").as_deref(), Ok("1"));
    println!("CONPTY_COST_READY");
    std::io::stdout().flush().unwrap();
    std::thread::sleep(Duration::from_secs(2));
}
#[cfg(windows)]
#[test]
#[ignore = "run in fresh processes for system/bundled A/B sampling"]
fn conpty_cost_parent() {
    let system = std::env::var("CONGMINAL_COST_MODE").as_deref() == Ok("system");
    crate::pty::force_system_backend(system);
    let (tx, rx) = mpsc::sync_channel(16);
    let sink: EventSink = Arc::new(move |e| tx.send(e).is_ok());
    let command = Command {
        program: Some(std::env::current_exe().unwrap().to_string_lossy().into_owned()),
        args: vec![
            "--exact".into(),
            "graphics::native_tests::conpty_cost_child".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        env: vec![("CONGMINAL_COST_CHILD".into(), "1".into())],
        ..Command::default()
    };
    let started = Instant::now();
    let pty = Pty::spawn(&command, 80, 24, sink).unwrap();
    let modern = pty.modern_backend();
    assert_eq!(modern, !system);
    let mut bytes = Vec::new();
    let mut ready = None;
    let mut exit = None;
    let mut eof = false;
    while !(exit.is_some() && eof) && started.elapsed() < Duration::from_secs(10) {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(PtyEvent::Data(data)) => {
                assert!(bytes.len() + data.len() < 8192);
                bytes.extend(data);
                if ready.is_none() && bytes.windows(17).any(|b| b == b"CONPTY_COST_READY") {
                    ready = Some(started.elapsed().as_micros());
                }
            }
            Ok(PtyEvent::Exit(code)) => exit = Some(code),
            Ok(PtyEvent::Eof) => eof = true,
            Ok(PtyEvent::Error(e)) => panic!("{e}"),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(e) => panic!("{e}"),
        }
    }
    drop(pty);
    crate::pty::force_system_backend(false);
    assert_eq!(exit, Some(Some(0)));
    assert!(eof);
    println!("CONPTY_COST mode={} ready_us={}", if modern { "bundled" } else { "system" }, ready.expect("child ready"));
}
