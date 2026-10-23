//! Channels from the Send-only I/O tasks to the thread that owns the panes.

use std::io;

use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::Id;

pub enum PtyEvent {
    Output {
        id: Id,
        bytes: crate::transport::EventBuffer,
        _credit: OwnedSemaphorePermit,
    },
    Eof(Id),
    ReadError {
        id: Id,
        error: io::Error,
    },
}

pub enum Control {
    ChildExited(Id),
    WriteError { id: Id, error: io::Error },
}

#[derive(Clone)]
pub struct Bus {
    pub pty: mpsc::Sender<PtyEvent>,
    pub control: mpsc::UnboundedSender<Control>,
}

pub fn channel() -> (
    Bus,
    mpsc::Receiver<PtyEvent>,
    mpsc::UnboundedReceiver<Control>,
) {
    let (pty, data) = mpsc::channel(128);
    let (control, controls) = mpsc::unbounded_channel();
    (Bus { pty, control }, data, controls)
}
