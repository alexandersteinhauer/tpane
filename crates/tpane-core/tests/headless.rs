//! End-to-end tests of headless terminals: real shells and programs in PTYs,
//! driven by input events, observed through frames and Ghostty's formatter.

use std::{path::Path, sync::Arc, time::Duration};

use tokio::{sync::mpsc, time::timeout};
use tpane_core::{
    Command, ExitStatus, Frame, InputEvent, Key, KeyEvent, Modifiers, MouseAction, MouseButton,
    MouseEvent, ScreenModel, Size, Terminal, TerminalEvent, TerminalOptions, Viewer,
    frame::MouseTracking,
};

const WAIT: Duration = Duration::from_secs(10);

/// bash without personal configuration and with a fixed prompt.
fn bash(size: Size) -> TerminalOptions {
    let mut options = TerminalOptions::new("/bin/bash", std::env::temp_dir(), size);
    options.command.args = vec!["--noprofile".into(), "--norc".into()];
    options.command.env = vec![("PS1".into(), "$ ".into())];
    options
}

async fn start(size: Size) -> (Terminal, mpsc::UnboundedReceiver<TerminalEvent>) {
    Terminal::spawn(bash(size)).await.unwrap()
}

fn type_text(terminal: &Terminal, text: &str) {
    for ch in text.chars() {
        let event = if ch == '\r' {
            KeyEvent::press(Key::Enter)
        } else {
            KeyEvent::text(ch)
        };
        terminal.input(InputEvent::Key(event)).unwrap();
    }
}

fn ctrl(ch: char) -> InputEvent {
    InputEvent::Key(KeyEvent::text(ch).with_mods(Modifiers {
        ctrl: true,
        ..Modifiers::NONE
    }))
}

/// Applies frames to `model` until `done` holds for the model and the last
/// frame; returns that frame.
async fn until(
    viewer: &mut Viewer,
    model: &mut ScreenModel,
    mut done: impl FnMut(&ScreenModel, &Frame) -> bool,
) -> Arc<Frame> {
    let result = timeout(WAIT, async {
        loop {
            let frame = viewer.next().await.expect("terminal ended");
            model.apply(&frame).unwrap();
            if done(model, &frame) {
                return frame;
            }
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("timed out; screen:\n{}", model.text()))
}

async fn screen_until(terminal: &Terminal, mut done: impl FnMut(&str) -> bool) -> String {
    let result = timeout(WAIT, async {
        loop {
            let text = terminal.screen_text().await.unwrap();
            if done(&text) {
                return text;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match result {
        Ok(text) => text,
        Err(_) => panic!(
            "timed out; screen:\n{}",
            terminal.screen_text().await.unwrap()
        ),
    }
}

/// Waits until the last non-empty row is the bare prompt.
async fn wait_for_prompt(viewer: &mut Viewer, model: &mut ScreenModel) {
    let at_prompt =
        |model: &ScreenModel| model.text().lines().last().map(str::trim_end) == Some("$");
    if !at_prompt(model) {
        until(viewer, model, |model, _| at_prompt(model)).await;
    }
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|candidate| candidate.trim_end() == line)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn typed_command_shows_in_screen_text_and_frames() {
    let (terminal, _events) = start(Size::new(40, 10)).await;
    let mut viewer = terminal.attach().unwrap();
    let mut model = ScreenModel::default();
    let first = until(&mut viewer, &mut model, |_, _| true).await;
    assert!(first.full);
    assert_eq!((first.cols, first.rows), (40, 10));
    type_text(&terminal, "echo hi\r");
    until(&mut viewer, &mut model, |model, _| {
        has_line(&model.text(), "hi")
    })
    .await;
    let screen = screen_until(&terminal, |text| has_line(text, "hi")).await;
    assert!(screen.contains("$ echo hi"), "{screen}");
    // Once output settles, the frames rebuilt exactly what Ghostty's
    // formatter reads.
    let mut screen = terminal.screen_text().await.unwrap();
    while model.text() != screen.trim_end() {
        let frame = timeout(WAIT, viewer.next()).await.unwrap().unwrap();
        model.apply(&frame).unwrap();
        screen = terminal.screen_text().await.unwrap();
    }
    terminal.shutdown().await;
    assert!(viewer.next().await.is_none());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn login_shell_gets_cwd_environment_and_exit_status() {
    let cwd = std::env::temp_dir().canonicalize().unwrap();
    let mut options = TerminalOptions::new("/bin/sh", &cwd, Size::new(200, 8));
    options.command.env = vec![("TPANE_TEST".into(), "from-host".into())];
    let (terminal, mut events) = Terminal::spawn(options).await.unwrap();
    assert!(terminal.pid() > 1);
    type_text(
        &terminal,
        "echo \"[$TPANE_TEST:$TERM:$(pwd -P)]\"; exit 3\r",
    );
    let expected = format!("[from-host:xterm-256color:{}]", cwd.display());
    let status = timeout(WAIT, terminal.wait()).await.unwrap().unwrap();
    assert_eq!(
        status,
        ExitStatus {
            code: Some(3),
            signal: None
        }
    );
    assert_eq!(terminal.exit_status(), Some(status.clone()));
    // The screen stays readable after the shell exited.
    let screen = terminal.screen_text().await.unwrap();
    assert!(has_line(&screen, &expected), "{screen}");
    let mut exited = None;
    while let Ok(event) = events.try_recv() {
        if let TerminalEvent::Exited(status) = event {
            exited = Some(status);
        }
    }
    assert_eq!(exited, Some(status));
    // Input after exit is ignored, not an error.
    terminal.input(ctrl('c')).unwrap();
    let mut viewer = terminal.attach().unwrap();
    let frame = timeout(WAIT, viewer.next()).await.unwrap().unwrap();
    assert!(frame.full);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_screen_program_opens_moves_and_quits() {
    let (terminal, _events) = start(Size::new(50, 12)).await;
    let mut viewer = terminal.attach().unwrap();
    let mut model = ScreenModel::default();
    let file = std::env::temp_dir().join(format!("tpane-headless-{}.txt", std::process::id()));
    std::fs::write(
        &file,
        (1..=100).map(|n| format!("line {n}\n")).collect::<String>(),
    )
    .unwrap();
    let vim = Path::new("/usr/bin/vim").exists() || Path::new("/bin/vim").exists();
    if vim {
        type_text(&terminal, &format!("vim -u NONE -N {}\r", file.display()));
    } else {
        type_text(&terminal, &format!("less {}\r", file.display()));
    }
    let frame = until(&mut viewer, &mut model, |model, frame| {
        frame.modes.alternate_screen && has_line(&model.text(), "line 1")
    })
    .await;
    assert!(frame.modes.alternate_screen);
    // Move to the end of the file: the last line comes into view.
    terminal
        .input(InputEvent::Key(KeyEvent::text('G')))
        .unwrap();
    until(&mut viewer, &mut model, |model, _| {
        has_line(&model.text(), "line 100") && !has_line(&model.text(), "line 1")
    })
    .await;
    let screen = terminal.screen_text().await.unwrap();
    assert!(has_line(&screen, "line 100"), "{screen}");
    // Quit, back to the shell's primary screen.
    if vim {
        type_text(&terminal, ":q\r");
    } else {
        type_text(&terminal, "q");
    }
    until(&mut viewer, &mut model, |model, frame| {
        !frame.modes.alternate_screen && model.text().contains("$ ")
    })
    .await;
    std::fs::remove_file(file).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn resize_reaches_the_child_and_yields_a_full_frame() {
    let (terminal, _events) = start(Size::new(40, 10)).await;
    let mut viewer = terminal.attach().unwrap();
    let mut model = ScreenModel::default();
    until(&mut viewer, &mut model, |model, _| {
        model.text().contains('$')
    })
    .await;
    terminal
        .resize(Size {
            cols: 70,
            rows: 20,
            cell_width: 8,
            cell_height: 16,
        })
        .unwrap();
    let frame = until(&mut viewer, &mut model, |_, frame| frame.cols == 70).await;
    assert!(frame.full);
    assert_eq!(frame.rows, 20);
    type_text(&terminal, "stty size\r");
    until(&mut viewer, &mut model, |model, _| {
        has_line(&model.text(), "20 70")
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn viewers_attach_and_detach_while_the_terminal_runs() {
    let (terminal, _events) = start(Size::new(40, 10)).await;
    let mut first = terminal.attach().unwrap();
    let mut first_model = ScreenModel::default();
    type_text(&terminal, "echo one\r");
    until(&mut first, &mut first_model, |model, _| {
        has_line(&model.text(), "one")
    })
    .await;

    // A late viewer starts from a full frame with everything so far.
    let mut second = terminal.attach().unwrap();
    let mut second_model = ScreenModel::default();
    let frame = until(&mut second, &mut second_model, |_, _| true).await;
    assert!(frame.full);
    assert!(has_line(&second_model.text(), "one"));
    drop(second);

    // The first viewer continues incrementally (ScreenModel rejects gaps).
    type_text(&terminal, "echo two\r");
    until(&mut first, &mut first_model, |model, _| {
        has_line(&model.text(), "two")
    })
    .await;

    // A viewer that stops asking falls behind and resynchronizes in full.
    let mut idle = terminal.attach().unwrap();
    let mut idle_model = ScreenModel::default();
    until(&mut idle, &mut idle_model, |_, _| true).await;
    for n in 0..40 {
        type_text(&terminal, &format!("echo n{n}\r"));
        until(&mut first, &mut first_model, |model, _| {
            has_line(&model.text(), &format!("n{n}"))
        })
        .await;
    }
    let frame = until(&mut idle, &mut idle_model, |_, _| true).await;
    assert!(
        frame.full,
        "a viewer more than 16 frames behind gets a full frame"
    );
    assert!(has_line(&idle_model.text(), "n39"));

    // A cancelled `next` loses its frame, so the viewer resynchronizes in
    // full instead of seeing a gap.
    assert!(
        timeout(Duration::from_millis(50), first.next())
            .await
            .is_err()
    );
    type_text(&terminal, "echo three\r");
    // Give the owner time to render the frame the cancelled request missed.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let frame = timeout(WAIT, first.next()).await.unwrap().unwrap();
    assert!(frame.full);
    first_model.apply(&frame).unwrap();
    if !has_line(&first_model.text(), "three") {
        until(&mut first, &mut first_model, |model, _| {
            has_line(&model.text(), "three")
        })
        .await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_report_mouse_and_paste_modes_and_input_follows_them() {
    let (terminal, _events) = start(Size::new(60, 10)).await;
    let mut viewer = terminal.attach().unwrap();
    let mut model = ScreenModel::default();
    until(&mut viewer, &mut model, |model, _| {
        model.text().contains('$')
    })
    .await;
    // The job prints a marker itself, after bash made it the foreground
    // process group; input typed before that would race the shell.
    type_text(
        &terminal,
        "printf '\\033[?1002h\\033[?2004h\\033[?1004h'; sh -c 'echo rea\"\"dy; exec cat -v'\r",
    );
    let frame = until(&mut viewer, &mut model, |model, frame| {
        frame.modes.mouse == MouseTracking::Button && has_line(&model.text(), "ready")
    })
    .await;
    assert!(frame.modes.bracketed_paste);
    assert!(frame.modes.focus_reporting);
    terminal
        .input(InputEvent::Paste { text: "xy".into() })
        .unwrap();
    terminal
        .input(InputEvent::Mouse(MouseEvent {
            action: MouseAction::Press,
            button: Some(MouseButton::Left),
            x: 2,
            y: 3,
            mods: Modifiers::NONE,
        }))
        .unwrap();
    terminal
        .input(InputEvent::Focus { focused: false })
        .unwrap();
    type_text(&terminal, "\r");
    // cat -v shows the bracketed paste, the X10-encoded press and focus out:
    // once as the terminal's echo, once as cat's output.
    let line = "^[[200~xy^[[201~^[[M #$^[[O";
    until(&mut viewer, &mut model, |model, _| {
        model
            .text()
            .lines()
            .filter(|text| text.trim_end() == line)
            .count()
            == 2
    })
    .await;
    terminal.input(ctrl('c')).unwrap();
    wait_for_prompt(&mut viewer, &mut model).await;
    type_text(&terminal, "printf '\\033[?1002l'\r");
    // (Newer bash enables bracketed paste itself at its prompt.)
    let frame = until(&mut viewer, &mut model, |_, frame| {
        frame.modes.mouse == MouseTracking::Off
    })
    .await;
    assert!(!frame.modes.alternate_screen);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn ctrl_c_interrupts_the_foreground_job_and_returns_to_the_prompt() {
    let (terminal, _events) = start(Size::new(60, 10)).await;
    let mut viewer = terminal.attach().unwrap();
    let mut model = ScreenModel::default();
    wait_for_prompt(&mut viewer, &mut model).await;
    for round in 0..3 {
        // The job prints its PID itself, after it took the foreground; the
        // echoed command line shows `$$` instead, so it cannot match.
        type_text(&terminal, "sh -c 'echo sleeping:$$; exec sleep 100'\r");
        let mut pid = None;
        until(&mut viewer, &mut model, |model, _| {
            pid = model.text().lines().find_map(|line| {
                line.trim_end()
                    .strip_prefix("sleeping:")?
                    .parse::<i32>()
                    .ok()
            });
            pid.is_some()
        })
        .await;
        let pid = pid.unwrap();
        terminal.input(ctrl('c')).unwrap();
        wait_for_prompt(&mut viewer, &mut model).await;
        // The interrupted job is gone, not just backgrounded.
        type_text(
            &terminal,
            &format!("clear; kill -0 {pid} 2>/dev/null || echo gone{round}\r"),
        );
        until(&mut viewer, &mut model, |model, _| {
            has_line(&model.text(), &format!("gone{round}"))
        })
        .await;
        wait_for_prompt(&mut viewer, &mut model).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn mouse_selection_becomes_a_copy_event() {
    let (terminal, mut events) = start(Size::new(40, 6)).await;
    type_text(&terminal, "clear; echo copy-me\r");
    let screen = screen_until(&terminal, |text| has_line(text, "copy-me")).await;
    let row = screen
        .lines()
        .position(|line| line.trim_end() == "copy-me")
        .unwrap() as u16;
    let mouse = |action, x| {
        InputEvent::Mouse(MouseEvent {
            action,
            button: Some(MouseButton::Left),
            x,
            y: row,
            mods: Modifiers::NONE,
        })
    };
    terminal.input(mouse(MouseAction::Press, 0)).unwrap();
    terminal.input(mouse(MouseAction::Motion, 3)).unwrap();
    terminal.input(mouse(MouseAction::Release, 3)).unwrap();
    let copied = timeout(WAIT, async {
        loop {
            if let TerminalEvent::Copy(text) = events.recv().await.unwrap() {
                return text;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(copied, "copy");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn scrollback_text_keeps_history_beyond_the_screen() {
    let (terminal, _events) = start(Size::new(30, 5)).await;
    type_text(&terminal, "for i in $(seq 1 40); do echo row$i; done\r");
    screen_until(&terminal, |text| has_line(text, "row40")).await;
    let screen = terminal.screen_text().await.unwrap();
    assert!(!has_line(&screen, "row1"), "{screen}");
    let all = terminal.scrollback_text().await.unwrap();
    for n in 1..=40 {
        assert!(has_line(&all, &format!("row{n}")), "row{n} in {all}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn many_terminals_run_side_by_side_in_one_process() {
    let mut terminals = Vec::new();
    for _ in 0..8 {
        terminals.push(start(Size::new(30, 6)).await);
    }
    for (n, (terminal, _)) in terminals.iter().enumerate() {
        type_text(terminal, &format!("echo terminal-{n}\r"));
    }
    for (n, (terminal, _)) in terminals.iter().enumerate() {
        screen_until(terminal, |text| has_line(text, &format!("terminal-{n}"))).await;
    }
    for (terminal, _) in &terminals {
        terminal.shutdown().await;
        assert!(terminal.input(ctrl('c')).is_err());
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn spawn_reports_errors_and_needs_a_multi_thread_runtime() {
    let options = TerminalOptions {
        command: Command::login_shell("/does/not/exist", std::env::temp_dir()),
        ..bash(Size::new(10, 10))
    };
    assert!(Terminal::spawn(options).await.is_err());
    let result = std::thread::spawn(|| {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Terminal::spawn(bash(Size::new(10, 10))))
            .map(|_| ())
    })
    .join()
    .unwrap();
    assert!(result.is_err());
}

#[cfg(feature = "serde")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn frames_serialize_compactly_and_round_trip() {
    let (terminal, _events) = start(Size::new(40, 6)).await;
    let mut viewer = terminal.attach().unwrap();
    let mut model = ScreenModel::default();
    type_text(&terminal, "printf '\\033[1;31mred\\033[0m\\n'\r");
    let frame = until(&mut viewer, &mut model, |model, _| {
        has_line(&model.text(), "red")
    })
    .await;
    let json = serde_json::to_string(&*frame).unwrap();
    let back: Frame = serde_json::from_str(&json).unwrap();
    assert_eq!(back, *frame);
    let full = {
        let mut late = terminal.attach().unwrap();
        late.next().await.unwrap()
    };
    let json = serde_json::to_string(&*full).unwrap();
    // Default blanks are trimmed: a 40x6 screen with a few short lines.
    assert!(json.len() < 1500, "{} bytes: {json}", json.len());
    assert!(json.contains(r#""bold":true"#), "{json}");
}
