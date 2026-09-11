//! Poll Crossterm only on its own wakeups, not on every PTY/output wakeup.
use std::io;

use crossterm::event::{Event, EventStream};
use futures_util::{Stream, StreamExt};
use tokio::{sync::mpsc, task::JoinHandle};

pub struct HostInput {
    pub events: mpsc::Receiver<io::Result<Event>>,
    task: Option<JoinHandle<()>>,
}

impl HostInput {
    pub fn new() -> Self {
        let (sender, events) = mpsc::channel(128);
        let task = tokio::spawn(async move {
            forward(EventStream::new(), sender).await;
        });
        Self {
            events,
            task: Some(task),
        }
    }

    pub async fn shutdown(mut self) {
        self.events.close();
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

impl Drop for HostInput {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn forward(
    mut stream: impl Stream<Item = io::Result<Event>> + Unpin,
    sender: mpsc::Sender<io::Result<Event>>,
) {
    loop {
        let event = tokio::select! {
            _ = sender.closed() => return,
            event = stream.next() => match event { Some(event) => event, None => return },
        };
        let failed = event.is_err();
        if sender.send(event).await.is_err() || failed {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::stream;
    use std::time::Duration;

    #[tokio::test(flavor = "current_thread")]
    async fn bounded_forwarding_preserves_order_and_stops_on_error() {
        let (sender, mut events) = mpsc::channel(1);
        let task = tokio::spawn(forward(
            stream::iter([
                Ok(Event::FocusGained),
                Ok(Event::FocusLost),
                Err(io::Error::from_raw_os_error(libc::EIO)),
                Ok(Event::FocusGained),
            ]),
            sender,
        ));
        assert_eq!(events.recv().await.unwrap().unwrap(), Event::FocusGained);
        assert_eq!(events.recv().await.unwrap().unwrap(), Event::FocusLost);
        assert_eq!(
            events.recv().await.unwrap().unwrap_err().raw_os_error(),
            Some(libc::EIO)
        );
        assert!(events.recv().await.is_none());
        task.await.unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn receiver_drop_cancels_idle_and_backpressured_forwarders() {
        let (sender, events) = mpsc::channel(1);
        let idle = tokio::spawn(forward(stream::pending(), sender));
        drop(events);
        tokio::time::timeout(Duration::from_secs(1), idle)
            .await
            .unwrap()
            .unwrap();
        let (sender, mut events) = mpsc::channel(1);
        let busy = tokio::spawn(forward(
            stream::repeat_with(|| Ok(Event::FocusGained)),
            sender,
        ));
        assert!(events.recv().await.is_some());
        drop(events);
        tokio::time::timeout(Duration::from_secs(1), busy)
            .await
            .unwrap()
            .unwrap();
    }
}
