use std::{
    collections::VecDeque,
    io::{self, Read, Write},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
        mpsc::{self, Receiver, SyncSender, TrySendError},
    },
    thread,
};

use anyhow::{Context, Result, ensure};

const MAX_PENDING_INPUT: usize = 8 * 1024 * 1024;

pub fn read_output(mut reader: Box<dyn Read + Send>) -> Result<Receiver<io::Result<Vec<u8>>>> {
    let (tx, rx) = mpsc::sync_channel(16);
    thread::Builder::new()
        .name("pty-reader".into())
        .spawn(move || {
            let mut buffer = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => break,
                    Ok(count) => {
                        if tx.send(Ok(buffer[..count].to_vec())).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) if error.raw_os_error() == Some(libc::EIO) => break,
                    Err(error) => {
                        let _ = tx.send(Err(error));
                        break;
                    }
                }
            }
        })?;
    Ok(rx)
}

pub struct Writer {
    tx: SyncSender<Vec<u8>>,
    pending: VecDeque<Vec<u8>>,
    outstanding: Arc<AtomicUsize>,
    errors: Receiver<io::Error>,
}

impl Writer {
    pub fn new(mut writer: Box<dyn Write + Send>) -> Result<Self> {
        let (tx, rx) = mpsc::sync_channel::<Vec<u8>>(16);
        let (error_tx, errors) = mpsc::sync_channel(1);
        let outstanding = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&outstanding);
        thread::Builder::new()
            .name("pty-writer".into())
            .spawn(move || {
                while let Ok(bytes) = rx.recv() {
                    let result = writer.write_all(&bytes);
                    count.fetch_sub(bytes.len(), Ordering::Relaxed);
                    if let Err(error) = result {
                        let _ = error_tx.send(error);
                        break;
                    }
                }
            })?;
        Ok(Self {
            tx,
            pending: VecDeque::new(),
            outstanding,
            errors,
        })
    }

    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        ensure!(
            bytes.len()
                <= MAX_PENDING_INPUT.saturating_sub(self.outstanding.load(Ordering::Relaxed)),
            "terminal input exceeds the 8 MiB queue limit"
        );
        self.outstanding.fetch_add(bytes.len(), Ordering::Relaxed);
        self.pending.push_back(bytes.to_vec());
        self.flush()
    }

    pub fn flush(&mut self) -> Result<()> {
        if let Ok(error) = self.errors.try_recv() {
            return Err(error).context("write terminal input");
        }
        while let Some(bytes) = self.pending.pop_front() {
            match self.tx.try_send(bytes) {
                Ok(()) => {}
                Err(TrySendError::Full(bytes)) => {
                    self.pending.push_front(bytes);
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    anyhow::bail!("terminal input worker stopped")
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BlockedWriter(Receiver<()>);
    impl Write for BlockedWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            let _ = self.0.recv();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn a_full_pty_cannot_block_the_main_thread_and_input_is_bounded() {
        let (_release, blocked) = mpsc::channel();
        let mut writer = Writer::new(Box::new(BlockedWriter(blocked))).unwrap();
        writer.write(&vec![b'x'; MAX_PENDING_INPUT]).unwrap();
        assert!(writer.write(b"x").is_err());
        writer.flush().unwrap();
    }
}
