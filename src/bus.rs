use std::{io, sync::Arc};

use tokio::sync::{OwnedSemaphorePermit, mpsc};

use crate::tree::Id;

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
    WriteError {
        id: Id,
        error: io::Error,
    },
    // The completed frame's buffer is handed back: only one frame is ever in
    // flight, so the owner can clear and reuse it instead of reallocating.
    FrameWritten {
        result: io::Result<()>,
        frame: Vec<u8>,
    },
    ClipboardFinished(Result<(), String>),
}

#[derive(Clone)]
pub struct Bus {
    pub pty: mpsc::Sender<PtyEvent>,
    pub control: mpsc::UnboundedSender<Control>,
    pub clipboard: Arc<tokio::sync::Semaphore>,
}

pub fn channel() -> (
    Bus,
    mpsc::Receiver<PtyEvent>,
    mpsc::UnboundedReceiver<Control>,
) {
    let (pty, data) = mpsc::channel(128);
    let (control, controls) = mpsc::unbounded_channel();
    (
        Bus {
            pty,
            control,
            clipboard: Arc::new(tokio::sync::Semaphore::new(1)),
        },
        data,
        controls,
    )
}
