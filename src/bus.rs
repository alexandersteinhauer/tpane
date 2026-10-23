//! The app's channels: the core's pane bus plus host-side completions.

use std::{io, sync::Arc};

use tokio::sync::mpsc;
pub use tpane_core::bus::{Control as PaneControl, PtyEvent};

pub enum Control {
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
    pub panes: tpane_core::bus::Bus,
    pub control: mpsc::UnboundedSender<Control>,
    pub clipboard: Arc<tokio::sync::Semaphore>,
}

pub struct Receivers {
    pub data: mpsc::Receiver<PtyEvent>,
    pub panes: mpsc::UnboundedReceiver<PaneControl>,
    pub control: mpsc::UnboundedReceiver<Control>,
}

pub fn channel() -> (Bus, Receivers) {
    let (panes, data, pane_control) = tpane_core::bus::channel();
    let (control, controls) = mpsc::unbounded_channel();
    (
        Bus {
            panes,
            control,
            clipboard: Arc::new(tokio::sync::Semaphore::new(1)),
        },
        Receivers {
            data,
            panes: pane_control,
            control: controls,
        },
    )
}
