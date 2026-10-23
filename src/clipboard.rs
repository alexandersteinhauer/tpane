//! Copies selected text with pbcopy, off the UI caller.

use std::time::Duration;

use anyhow::Result;

use crate::bus::{Bus, Control};

pub fn copy(bus: &Bus, text: String) -> Result<()> {
    let permit = bus
        .clipboard
        .clone()
        .try_acquire_owned()
        .map_err(|_| anyhow::anyhow!("clipboard is busy"))?;
    let control = bus.control.clone();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        let result: Result<()> = async {
            let mut child = tokio::process::Command::new("/usr/bin/pbcopy")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            let operation = async {
                let mut stdin = child
                    .stdin
                    .take()
                    .ok_or_else(|| anyhow::anyhow!("clipboard input unavailable"))?;
                stdin.write_all(text.as_bytes()).await?;
                drop(stdin);
                anyhow::ensure!(child.wait().await?.success(), "clipboard command failed");
                Ok::<_, anyhow::Error>(())
            };
            tokio::time::timeout(Duration::from_secs(1), operation).await??;
            Ok(())
        }
        .await;
        drop(permit);
        let _ = control.send(Control::ClipboardFinished(
            result.map_err(|error| error.to_string()),
        ));
    });
    Ok(())
}
