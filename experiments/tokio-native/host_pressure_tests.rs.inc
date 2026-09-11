
#[test]
fn slow_host_output_does_not_block_input_or_bounded_shutdown() {
    let mut host = Host::new();
    std::fs::write(host.home.join("host_pressure.py"), r#"import os, select, termios, tty
saved = termios.tcgetattr(0)
try:
    tty.setraw(0)
    os.set_blocking(1, False)
    os.write(1, b'\r\n__PRESSURE_PID:' + str(os.getpid()).encode() + b'__\r\n')
    flooding = False
    while True:
        ready, writable, _ = select.select([0], [1] if flooding else [], [])
        if ready:
            data = os.read(0, 1024)
            if not data: break
            if b'f' in data: flooding = True
            if b'r' in data: open('input-received', 'w').close()
        if writable:
            try: os.write(1, b'HOST_OUTPUT_PRESSURE_0123456789\r\n' * 512)
            except BlockingIOError: pass
finally:
    termios.tcsetattr(0, termios.TCSANOW, saved)
"#).unwrap();
    host.send(b"/usr/bin/python3 host_pressure.py\n");
    let producer = host.track_pid("__PRESSURE_PID:");
    host.send(b"f");
    host.master.resize(PtySize { rows: 100, cols: 200, pixel_width: 0, pixel_height: 0 }).unwrap();
    // Deliberately stop reading the host PTY, including while sending input.
    std::thread::sleep(Duration::from_millis(300));
    host.writer.write_all(b"r").unwrap();
    let deadline = Instant::now() + Duration::from_secs(2);
    while !host.home.join("input-received").exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(host.home.join("input-received").exists(), "blocked host output prevented input dispatch");
    if std::env::var_os("TPANE_LLDB_TEST").is_some() {
        let target = host.process;
        let signal = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(1));
            target.signal(libc::SIGTERM);
        });
        let mut debugger = std::process::Command::new("/usr/bin/lldb");
        debugger.args(["--batch", "-p", &host.process.pid.to_string(),
                       "-o", "breakpoint set -n exit -n _exit",
                       "-o", "process handle SIGTERM -n false -p true -s false",
                       "-o", "continue",
                       "-o", "thread backtrace all",
                       "-o", "expression -- (int)fcntl(1, 3, 0)"]);
        if std::env::var_os("TPANE_LLDB_FLUSH").is_some() {
            debugger.args(["-o", "expression -- (int)tcflush(1, 2)"]);
        }
        let debug = debugger.args(["-o", "process detach"]).output().unwrap();
        signal.join().unwrap();
        std::fs::write("/tmp/tpane-lldb.txt", [&debug.stdout[..], &debug.stderr[..]].concat()).unwrap();
        eprintln!("LLDB status: {}; output saved to /tmp/tpane-lldb.txt", debug.status);
    }
    host.process.signal(libc::SIGTERM);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !host.exited && Instant::now() < deadline {
        host.exited = host.child.try_wait().unwrap().is_some();
        std::thread::sleep(Duration::from_millis(5));
    }
    if !host.exited {
        eprintln!("trace: {:?}", std::fs::read_to_string(host.home.join("trace.log")));
        let _ = std::process::Command::new("/bin/ps").args(["-p", host.process.pid.to_string().as_str(), "-o", "pid,ppid,stat,wchan,command"]).status();
    }
    assert!(host.exited, "shutdown waited indefinitely for an unread host");
    assert!(!producer.alive(), "shutdown left the producer alive");
    let after = termios(&*host.master);
    let mask = libc::ICANON | libc::ECHO | libc::ISIG;
    assert_eq!(after.c_lflag & mask, host.before.c_lflag & mask);
    // Visual reset cannot be guaranteed when the receiver refuses all output.
}
