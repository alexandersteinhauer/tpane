//! Coordinator tests use independent sockets, never the live shells' output.
use super::*;
use libghostty_vt::{
    Terminal, TerminalOptions,
    terminal::{Point, PointCoordinate},
};
use std::{os::unix::net::UnixStream, sync::Arc};
use tokio::{io::unix::AsyncFd, sync::mpsc, task::JoinHandle, time::timeout};

const DEADLINE: Duration = Duration::from_secs(5);
// Disable the wall-clock budget: these tests exercise exact read-count bounds.
const READ_BUDGET_ONLY: Duration = Duration::MAX;
const COLS: u16 = 40;
const ROWS: u16 = 8;

struct Harness {
    app: App,
    bus: Bus,
    data: mpsc::Receiver<PtyEvent>,
    tasks: Vec<JoinHandle<()>>,
    // Retain, but never consume, the real shells' notifications. Only fake PTY
    // data reaches the terminal engines, independent of prompts and shell rc.
    _shell_data: mpsc::Receiver<PtyEvent>,
    _shell_control: mpsc::UnboundedReceiver<Control>,
    _control: mpsc::UnboundedReceiver<Control>,
}

impl Harness {
    fn new() -> Self {
        let (shell_bus, shell_data, shell_control) = crate::bus::channel();
        let app = App::new(PathBuf::from("/bin/sh"), COLS, ROWS, (0, 0), shell_bus).unwrap();
        let (bus, data, control) = crate::bus::channel();
        Self {
            app,
            bus,
            data,
            tasks: Vec::new(),
            _shell_data: shell_data,
            _shell_control: shell_control,
            _control: control,
        }
    }

    fn socket(&mut self, id: Id) -> UnixStream {
        let (reader, peer) = UnixStream::pair().unwrap();
        reader.set_nonblocking(true).unwrap();
        peer.set_nonblocking(true).unwrap();
        let master = Arc::new(AsyncFd::new(reader.into()).unwrap());
        let tx = self.bus.pty.clone();
        self.tasks.push(tokio::spawn(async move {
            tokio::select! {
                _ = tx.closed() => {},
                _ = crate::readiness::watch(master, id, &tx) => {},
            }
        }));
        peer
    }

    async fn notification(&mut self, expected: Id) -> PtyEvent {
        let event = timeout(DEADLINE, self.data.recv())
            .await
            .expect("fake PTY watcher stalled")
            .expect("fake PTY channel closed");
        match &event {
            PtyEvent::Ready { id, .. } => assert_eq!(*id, expected),
            PtyEvent::ReadError { error, .. } => panic!("fake PTY readiness failed: {error}"),
        }
        event
    }

    async fn enqueue(&mut self, id: Id) {
        let event = self.notification(id).await;
        self.app.data(event).unwrap();
    }

    fn turn(&mut self, reads: usize) {
        self.app.drain_ready_turn(reads, READ_BUDGET_ONLY).unwrap();
    }

    fn queued(&self) -> Vec<Id> {
        self.app.ready.iter().map(|(id, _)| *id).collect()
    }

    async fn shutdown(&mut self) {
        for task in self.tasks.drain(..) {
            task.abort();
            let error = timeout(DEADLINE, task)
                .await
                .expect("watcher cancellation stalled")
                .expect_err("watcher exited unexpectedly");
            assert!(error.is_cancelled());
        }
        self.app.shutdown().await;
        assert!(self.app.panes.is_empty());
        assert!(!self.app.has_ready());
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        // Also cancel fake watchers on assertion failure; App's Drop owns the
        // live sessions. Successful tests additionally await explicit shutdown.
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn host() -> Terminal<'static, 'static> {
    Terminal::new(TerminalOptions {
        cols: COLS,
        rows: ROWS,
        max_scrollback: 0,
    })
    .unwrap()
}

fn row_prefix(terminal: &Terminal, row: u16, len: usize) -> String {
    (0..len)
        .map(|x| {
            let point = Point::Active(PointCoordinate {
                x: x as u16,
                y: u32::from(row),
            });
            let codepoint = terminal
                .grid_ref(point)
                .unwrap()
                .cell()
                .unwrap()
                .codepoint()
                .unwrap();
            if codepoint == 0 {
                ' '
            } else {
                char::from_u32(codepoint).unwrap()
            }
        })
        .collect()
}

fn pane_prefix(app: &mut App, id: Id, len: usize) -> String {
    let mut bytes = Vec::new();
    app.panes
        .get_mut(&id)
        .unwrap()
        .draw(&mut bytes, 0, true)
        .unwrap();
    let mut terminal = host();
    terminal.vt_write(&bytes);
    row_prefix(&terminal, 0, len)
}

#[tokio::test(flavor = "current_thread")]
async fn successful_read_retains_its_token_at_the_read_budget_boundary() {
    let mut h = Harness::new();
    let mut peer = h.socket(1);
    peer.write_all(b"first").unwrap();
    h.enqueue(1).await;

    // A zero-read turn must neither consume bytes nor lose the lease.
    h.turn(0);
    assert_eq!(h.queued(), [1]);
    assert_eq!(pane_prefix(&mut h.app, 1, 5), "     ");

    h.turn(1);
    assert!(h.app.has_ready());
    assert_eq!(h.queued(), [1]);
    assert_eq!(pane_prefix(&mut h.app, 1, 5), "first");

    // More bytes can be consumed by the retained lease without a bus roundtrip.
    peer.write_all(b"-second").unwrap();
    h.turn(1);
    assert_eq!(h.queued(), [1]);
    assert_eq!(pane_prefix(&mut h.app, 1, 12), "first-second");
    h.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn draining_to_would_block_rearms_for_later_data() {
    let mut h = Harness::new();
    let mut peer = h.socket(1);
    peer.write_all(b"before").unwrap();
    h.enqueue(1).await;
    h.turn(1);
    assert_eq!(h.queued(), [1]);
    h.turn(1); // Empty nonblocking socket: WouldBlock releases the lease.
    assert!(!h.app.has_ready());
    assert_eq!(pane_prefix(&mut h.app, 1, 6), "before");

    peer.write_all(b"-after").unwrap();
    h.enqueue(1).await;
    h.turn(2); // One successful read followed by WouldBlock in the same turn.
    assert!(!h.app.has_ready());
    assert_eq!(pane_prefix(&mut h.app, 1, 12), "before-after");
    h.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn ready_panes_take_round_robin_turns_even_when_the_first_stays_readable() {
    let mut h = Harness::new();
    h.app.action(Action::Add(1)).unwrap();
    assert_eq!(h.app.tree.active_terminal(), Some(2));
    let mut first = h.socket(1);
    let mut second = h.socket(2);
    // Establish queue order by awaiting each notification, not scheduler order.
    first.write_all(b"A").unwrap();
    h.enqueue(1).await;
    second.write_all(b"B").unwrap();
    h.enqueue(2).await;
    assert_eq!(h.queued(), [1, 2]);

    h.turn(1);
    assert_eq!(h.queued(), [2, 1]);
    assert_eq!(pane_prefix(&mut h.app, 1, 1), "A");
    assert_eq!(pane_prefix(&mut h.app, 2, 1), " ");
    first.write_all(b"a").unwrap();

    h.turn(1);
    assert_eq!(h.queued(), [1, 2]);
    assert_eq!(pane_prefix(&mut h.app, 1, 2), "A ");
    assert_eq!(pane_prefix(&mut h.app, 2, 1), "B");
    h.turn(1);
    assert_eq!(h.queued(), [2, 1]);
    assert_eq!(pane_prefix(&mut h.app, 1, 2), "Aa");
    h.turn(2);
    assert!(!h.app.has_ready());
    h.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn removing_a_pane_discards_held_tokens_and_ignores_stale_notifications() {
    let mut h = Harness::new();
    h.app.action(Action::Add(1)).unwrap();
    let mut removed = h.socket(1);
    let mut survivor = h.socket(2);
    removed.write_all(b"removed").unwrap();
    h.enqueue(1).await;
    h.turn(1); // Retain a successfully serviced token across removal.
    survivor.write_all(b"survivor").unwrap();
    h.enqueue(2).await;

    // A separate descriptor supplies an already-issued notification racing
    // with removal. Hold it outside App until after the pane is gone.
    let mut stale_peer = h.socket(1);
    stale_peer.write_all(b"stale").unwrap();
    let stale = h.notification(1).await;
    h.app.remove(1).unwrap();
    assert!(!h.app.panes.contains_key(&1));
    assert_eq!(h.queued(), [2]);
    h.app.data(stale).unwrap();
    assert_eq!(h.queued(), [2]);

    h.turn(2);
    assert!(!h.app.has_ready());
    assert_eq!(pane_prefix(&mut h.app, 2, 8), "survivor");
    h.shutdown().await;
}

#[tokio::test(flavor = "current_thread")]
async fn hidden_output_updates_its_terminal_without_dirtying_the_visible_frame() {
    let mut h = Harness::new();
    h.app.action(Action::Add(1)).unwrap();
    let mut terminal = host();
    let mut bytes = Vec::new();
    assert!(h.app.draw(&mut bytes).unwrap());
    terminal.vt_write(&bytes);
    assert!(!h.app.needs_render());
    let top = h.app.tree.layout(COLS, ROWS).top;
    assert_eq!(row_prefix(&terminal, top, 6), "      ");

    let mut peer = h.socket(1);
    peer.write_all(b"hidden").unwrap();
    h.enqueue(1).await;
    h.turn(1);
    assert_eq!(h.queued(), [1]);
    assert!(!h.app.dirty);
    assert!(!h.app.content_dirty);
    assert!(!h.app.needs_render());
    bytes.clear();
    assert!(!h.app.draw(&mut bytes).unwrap());
    assert!(bytes.is_empty());

    h.app.action(Action::Select { group: 3, child: 1 }).unwrap();
    assert_eq!(h.app.tree.active_terminal(), Some(1));
    assert!(h.app.needs_render());
    assert!(h.app.draw(&mut bytes).unwrap());
    terminal.vt_write(&bytes);
    let top = h.app.tree.layout(COLS, ROWS).top;
    assert_eq!(row_prefix(&terminal, top, 6), "hidden");
    h.shutdown().await;
}
