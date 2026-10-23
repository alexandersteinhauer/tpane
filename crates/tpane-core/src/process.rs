//! Process inspection for session cleanup and working-directory queries:
//! libproc on macOS, `/proc` on Linux. Identities include the start time, so
//! a reused PID is not mistaken for a process we saw before.

use std::path::PathBuf;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Process {
    pub pid: libc::pid_t,
    pub ppid: libc::pid_t,
    pub sid: libc::pid_t,
    pub start: (u64, u64),
    pub zombie: bool,
}

impl Process {
    pub fn same_process(&self, other: &Self) -> bool {
        self.pid == other.pid && self.start == other.start
    }

    pub fn is_running(&self) -> bool {
        Self::read(self.pid).is_some_and(|p| self.same_process(&p) && !p.zombie)
    }

    pub fn signal(&self, signal: libc::c_int) {
        // Start time prevents signaling most PID reuse cases. A tiny
        // check-to-kill race remains (no pidfd on macOS), so never signal a
        // whole group.
        if self.pid != std::process::id() as libc::pid_t && self.is_running() {
            unsafe {
                libc::kill(self.pid, signal);
            }
        }
    }
}

#[cfg(target_os = "macos")]
impl Process {
    pub fn read(pid: libc::pid_t) -> Option<Self> {
        if pid <= 1 {
            return None;
        }
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let len = std::mem::size_of::<libc::proc_bsdinfo>() as libc::c_int;
        // libproc fills exactly this versioned structure on success.
        let read = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                len,
            )
        };
        if read != len {
            return None;
        }
        let info = unsafe { info.assume_init() };
        Some(Self {
            pid,
            ppid: info.pbi_ppid as libc::pid_t,
            sid: unsafe { libc::getsid(pid) },
            start: (info.pbi_start_tvsec, info.pbi_start_tvusec),
            zombie: info.pbi_status == libc::SZOMB,
        })
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn snapshot() -> Vec<Process> {
    // PROC_ALL_PIDS from <sys/proc_info.h> is not exported by libc.
    const PROC_ALL_PIDS: u32 = 1;
    let bytes = unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
    if bytes <= 0 {
        return Vec::new();
    }
    // Leave headroom for forks between the sizing and reading calls. Bounded
    // retries in Session's Drop handle additional churn without unbounded
    // allocation.
    let count = (bytes as usize / std::mem::size_of::<libc::pid_t>() + 256).min(1_048_576);
    let mut pids = vec![0 as libc::pid_t; count];
    let bytes = unsafe {
        libc::proc_listpids(
            PROC_ALL_PIDS,
            0,
            pids.as_mut_ptr().cast(),
            (pids.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int,
        )
    };
    if bytes <= 0 {
        return Vec::new();
    }
    pids.truncate(bytes as usize / std::mem::size_of::<libc::pid_t>());
    pids.into_iter().filter_map(Process::read).collect()
}

#[cfg(target_os = "macos")]
pub(crate) fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
    use std::{ffi::OsString, os::unix::ffi::OsStringExt};
    let mut info = std::mem::MaybeUninit::<libc::proc_vnodepathinfo>::zeroed();
    let len = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
    let read = unsafe {
        libc::proc_pidinfo(
            pid,
            libc::PROC_PIDVNODEPATHINFO,
            0,
            info.as_mut_ptr().cast(),
            len,
        )
    };
    if read != len {
        return None;
    }
    let info = unsafe { info.assume_init() };
    let path = info.pvi_cdir.vip_path.as_flattened();
    let end = path.iter().position(|&byte| byte == 0)?;
    let path = PathBuf::from(OsString::from_vec(
        path[..end].iter().map(|&byte| byte as u8).collect(),
    ));
    path.is_absolute().then_some(path)
}

#[cfg(target_os = "linux")]
impl Process {
    pub fn read(pid: libc::pid_t) -> Option<Self> {
        if pid <= 1 {
            return None;
        }
        let stat = std::fs::read(format!("/proc/{pid}/stat")).ok()?;
        // The command name is parenthesized and may contain spaces or ')'.
        let close = stat.iter().rposition(|&byte| byte == b')')?;
        let fields: Vec<&[u8]> = stat.get(close + 2..)?.split(|&byte| byte == b' ').collect();
        let number = |index: usize| -> Option<u64> {
            std::str::from_utf8(fields.get(index)?).ok()?.parse().ok()
        };
        // Fields from 3 on (proc(5)): state, ppid, pgrp, session, ...,
        // starttime (22, in clock ticks since boot).
        let state = *fields.first()?.first()?;
        Some(Self {
            pid,
            ppid: number(1)? as libc::pid_t,
            sid: number(3)? as libc::pid_t,
            start: (number(19)?, 0),
            zombie: matches!(state, b'Z' | b'X'),
        })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn snapshot() -> Vec<Process> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .filter_map(Process::read)
        .collect()
}

#[cfg(target_os = "linux")]
pub(crate) fn cwd(pid: libc::pid_t) -> Option<PathBuf> {
    let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
    // A removed directory reads as "<path> (deleted)"; it is not usable.
    let deleted = path
        .to_str()
        .is_some_and(|path| path.ends_with(" (deleted)"));
    (path.is_absolute() && !deleted).then_some(path)
}

/// Without process inspection, cleanup can only signal the shell itself.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
impl Process {
    pub fn read(pid: libc::pid_t) -> Option<Self> {
        (pid > 1 && unsafe { libc::kill(pid, 0) } == 0).then(|| Self {
            pid,
            ppid: 0,
            sid: unsafe { libc::getsid(pid) },
            start: (0, 0),
            zombie: false,
        })
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn snapshot() -> Vec<Process> {
    Vec::new()
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub(crate) fn cwd(_: libc::pid_t) -> Option<PathBuf> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_this_process_and_its_parent() {
        let me = Process::read(std::process::id() as libc::pid_t).unwrap();
        assert!(!me.zombie);
        assert_eq!(me.ppid, unsafe { libc::getppid() });
        assert_eq!(me.sid, unsafe { libc::getsid(0) });
        assert!(me.is_running());
        let again = Process::read(me.pid).unwrap();
        assert!(me.same_process(&again));
        assert!(snapshot().iter().any(|process| process.pid == me.pid));
        assert_eq!(cwd(me.pid), Some(std::env::current_dir().unwrap()));
    }
}
