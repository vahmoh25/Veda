# Third-party code

Vindows uses a few mature external crates where an established, reviewed
implementation of a protocol or of cryptography is safer than a new one
(see "Dependencies" in `docs/CODING.md`). Most come from crates.io and are
pinned by `Cargo.lock`. This directory holds the ones that Vindows has to
modify; each has a `VINDOWS-PATCHES.md` describing every change.

| Directory | Crate | License | Why it is vendored |
|-----------|-------|---------|--------------------|
| `smoltcp/` | smoltcp 0.14.0, the TCP/IP stack | 0BSD | cryptographically random TCP sequence numbers and protocol ids |
