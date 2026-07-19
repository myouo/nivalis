# Nivalis parser bound

This directory vendors `imap-proto` 0.16.7 (`MIT OR Apache-2.0`). Nivalis adds
one resource limit in `src/parser/core.rs`: a server literal declaration above
1 MiB is rejected before `nom` can return a large `Needed::Size` value to
`async-imap`.

The upstream `async-imap` buffer permits allocations up to 512 MiB. Nivalis
fetches raw messages one at a time with a 1 MiB limit, so retaining that parser
default would violate the application's memory contract before transport-level
read accounting could run.
