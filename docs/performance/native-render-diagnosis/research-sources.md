# External references checked during investigation

- [Tokio select](https://docs.rs/tokio/1.53.1/tokio/macro.select.html): branches run concurrently on one task, not in parallel; biased polling makes fairness the caller's responsibility.
- [Tokio yield_now](https://docs.rs/tokio/1.53.1/tokio/task/fn.yield_now.html): a yield does not guarantee another task or the I/O driver runs before resumption. It is not a reliable end-of-I/O-batch boundary.
- [Tokio AsyncFd](https://docs.rs/tokio/1.53.1/tokio/io/unix/struct.AsyncFd.html): readiness is a hint; clear it only after the actual I/O operation returns WouldBlock.

Located through Exa and checked against local pinned Tokio 1.53.1 sources. These document API guarantees; they do not prove which design is fastest for tpane.
