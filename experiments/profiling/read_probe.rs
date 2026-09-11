//! Diagnostic counters only; never used in normal application builds.
use std::sync::{Mutex, atomic::{AtomicU64, Ordering}};
static READS: AtomicU64 = AtomicU64::new(0);
static BYTES: AtomicU64 = AtomicU64::new(0);
static WOULD_BLOCK: AtomicU64 = AtomicU64::new(0);
static ERRORS: AtomicU64 = AtomicU64::new(0);
static BATCHES: Mutex<[u64; 65]> = Mutex::new([0; 65]);
pub fn read(count: isize) {
    READS.fetch_add(1, Ordering::Relaxed);
    if count >= 0 {
        BYTES.fetch_add(count as u64, Ordering::Relaxed);
    } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::WouldBlock {
        WOULD_BLOCK.fetch_add(1, Ordering::Relaxed);
    } else {
        ERRORS.fetch_add(1, Ordering::Relaxed);
    }
}
pub fn batch(n: usize) { BATCHES.lock().unwrap()[n] += 1; }
pub struct Report;
impl Drop for Report {
    fn drop(&mut self) {
        if let Some(path) = std::env::var_os("TPANE_PROFILE_FILE") {
            let path = std::path::PathBuf::from(path).with_extension("reads.json");
            let json = format!("{{\"performance_comparison_valid\":false,\"scope\":\"entire application lifetime, not only steady window\",\"read_syscalls\":{},\"read_bytes\":{},\"would_block\":{},\"other_errors\":{},\"receive_batch_histogram\":{:?}}}\n",
                READS.load(Ordering::Relaxed), BYTES.load(Ordering::Relaxed),
                WOULD_BLOCK.load(Ordering::Relaxed), ERRORS.load(Ordering::Relaxed), &*BATCHES.lock().unwrap());
            std::fs::write(path, json).unwrap();
        }
    }
}
