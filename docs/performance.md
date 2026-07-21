# Performance and resource matrix

Measured on 2026-07-21. Latencies below are warm-cache Criterion intervals from the file-backed `benchmarks/benches/local_store.rs` fixture. The 10,000- and 100,000-message databases use the production schema, WAL mode, a 512KiB query-only page cache, bounded 50-row projections, cached 64KiB reader excerpts, and the production FTS5 query shape.

| Workload | 10,000 messages | 100,000 messages |
| --- | ---: | ---: |
| First screen, 20 complete summary rows | 96.2–96.5µs | 99.6–100.5µs |
| Recent 50 complete summary rows operable | 142.1–142.6µs | 148.2–148.7µs |
| Cached 64KiB body | 24.1–25.0µs | 23.7–23.8µs |
| Sender search | 56.1–56.5µs | 495.1–497.2µs |
| Subject search | 55.2–55.8µs | 488.6–490.6µs |
| Body full-text search | 54.1–54.6µs | 489.9–501.6µs |
| Sender + subject + body | 71.5–72.0µs | 546.5–556.0µs |
| Chinese keyword (`项目计划`) | 51.0–52.4µs | 473.3–476.6µs |
| First screen during an active writer transaction | 94.8–95.6µs | 97.9–98.5µs |

These numbers isolate the local data path; they do not include process launch, renderer startup, keyring access, or network latency. The active-writer case holds an independent immediate writer transaction throughout each UI read. Its result verifies that WAL plus the dedicated reader prevents background synchronization from queuing ahead of the mailbox projection.

## Real-account local-first path

The release `bench-harness` was run against a disposable copy of a real 1,154-message account database. The newest message's cached content row was removed only from the copy so the run had to cross the production on-demand BODY fetch. The normal account database and content directory were not modified.

| Gate | Result |
| --- | ---: |
| Harness install to locally operable first screen | 220ms |
| Manual incremental metadata sync through database and UI commit | 2,165ms |
| Click to fetched, imported, and displayed body | 977ms |
| Complete metadata + body scenario | 3,142ms |
| New authenticated IMAP sessions | 1 |
| Foreground selected-session reuse | 1 |
| IDLE handoffs cancelled cleanly | 1 |

The first-screen clock begins when the benchmark hook is installed immediately before the native event loop, so it includes local controller/database/UI projection but not binary loading or `AppWindow` construction. Public-network timings naturally vary with the provider. The session counters are the important protocol invariant: metadata, IDLE, and foreground body used one authenticated selected session; the foreground request did not pay a second TCP/TLS/LOGIN handshake.

## Resource bounds

| Resource | Production bound |
| --- | ---: |
| Tokio runtimes | 1 global current-thread runtime |
| Core Tokio tasks | 3 long-lived tasks: core driver, file GC, SMTP Outbox |
| Per-account OS threads | 0 |
| Per-account Tokio spawned tasks | 0; account and IDLE futures are polled inline |
| Simultaneous account workflows | 1 global foreground/background workflow |
| IDLE watches | 10 global maximum |
| Selected IMAP sessions retained | 10 global maximum |
| Configured plaintext buffers per retained IMAP session | 32KiB |
| Configured plaintext buffers for ten IDLE sessions | 320KiB |
| SQLite connections | 2 global: writer + UI reader |
| SQLite configured page caches | 1MiB writer + 512KiB reader |
| SQLite worker threads | 0 |
| Search migration batch | 16 messages per idle transaction |

The 32KiB connection figure covers Nivalis's two fixed 16KiB plaintext buffers. TCP, Rustls, allocator, kernel socket, certificate, and server-dependent state are additional and must be assessed with whole-process RSS/PSS measurements. A metadata page may temporarily open one secondary IMAP connection for a balanced batch, but it is logged out rather than retained. Foreground body and manual-sync work cancel the matching IDLE command, complete `DONE`, and reclaim the already authenticated/selected session instead of keeping a second warm connection.

Disconnected IDLE sessions are dropped, successful/time-limited/cancelled sessions return to the bounded cache, and every ready watch is removed with `swap_remove`. Tests fill all ten watch slots and exercise the disconnect path, so reconnect attempts cannot grow the watch vector or session cache.

## Whole-process resource sample

The same release binary was sampled on the current Wayland/Skia environment after stabilization. These figures must not be treated as per-connection allocation sizes; they include renderer surfaces, font/file mappings, SQLite, the secret-service client, Rustls, and kernel-visible process state.

| State | Threads | FDs | RSS | PSS | Anonymous | IMAP sockets |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| Empty account catalog | 8 | 30 | 51,116KiB | 33,172KiB | 7,196KiB | 0 |
| 1,154 cached mails, automatic sync disabled | 8 | 30 | 93,600KiB | 69,045KiB | 28,136KiB | 0 |
| Same cache after sync, one stable IDLE | 10 | 32 | 97,336KiB | 72,815KiB | 30,328KiB | 1 |

The measured incremental cost from the populated local-only state to the synchronized/IDLE state was 3,736KiB RSS, 3,770KiB PSS, two threads, two file descriptors, and one IMAP socket. The extra threads are the lazy credential worker and an additional secret-service connection thread; they are process-global first-use cost, not one runtime/thread pair per account. Nivalis-owned SQLite/core thread count stayed fixed.

The populated mailbox currently exceeds the historical 90MiB RSS gate even before IDLE, while remaining below it by PSS. Most of that step is renderer/font/file mapping plus 18MiB of anonymous huge pages, not the 32KiB IMAP buffer budget. It is recorded as an unresolved renderer/UI working-set regression; the ten-live-account soak must not be approved until this baseline is reduced or the gate is deliberately requalified with current-platform evidence.

## Reproduction

```bash
cd benchmarks
cargo bench --locked --bench local_store
```

The real-account GUI benchmark is opt-in and always points `NIVALIS_DATA_DIR` at a disposable database copy. It reports cold local-first-screen time, incremental metadata time, on-demand body time, new protocol-session count, foreground session reuse, and IDLE cancellations. Never point a destructive fixture preparation step at the normal application data directory.
