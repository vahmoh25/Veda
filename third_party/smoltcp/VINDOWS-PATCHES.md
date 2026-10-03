# Vindows changes to smoltcp 0.14.0

This directory holds smoltcp 0.14.0 as published on crates.io (0BSD
license, see `LICENSE-0BSD.txt`), with the changes below. Everything else is
unmodified. Every changed spot is marked with a `VINDOWS PATCH` comment.

## Unpredictable protocol numbers (`src/rand.rs`, `src/iface/interface/mod.rs`)

Upstream draws TCP initial sequence numbers, DHCP transaction ids, DNS ids,
the initial IPv4 identification and random source ports from an sPCG32
generator seeded with a 64-bit value. sPCG32's state can be recovered from a
few outputs: a server that sees the initial sequence numbers of its own
connections could predict those of the machine's other connections and
inject data into them off-path (RFC 6528 explains the attack).

* `Config` gains `random_key: Option<[u8; 32]>`.
* With a key, `Rand` produces a ChaCha20 keystream (RFC 8439 block function,
  64-bit block counter, zero nonce). The Vindows network service keys every
  interface with 32 bytes from the kernel's CSPRNG.
* Without a key, `Rand` is upstream's sPCG32 exactly, so the upstream test
  suite still passes byte for byte.

## Manifest

The packaged `Cargo.toml` is kept as published, minus the bench, example and
integration-test targets (those directories are not vendored) and the
`[profile.release]` section (profiles of non-root packages are ignored).

## Running smoltcp's own tests

```text
cd third_party/smoltcp
cargo test --lib
```

(The crate is excluded from the Vindows workspace, so this resolves its
dev-dependencies separately.)

## Updating

Copy the new release's `src/`, `build.rs`, `Cargo.toml`, `README.md`,
`CHANGELOG.md` and license over this directory, re-apply the changes above
(search for `VINDOWS PATCH` in this version), and run the tests.
