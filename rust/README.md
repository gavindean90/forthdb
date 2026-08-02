# ForthDB Rust

This workspace is the beginning of an independent Rust implementation of ForthDB.

Its current scope is intentionally narrow:

- `forthdb-core` defines typed semantic values such as entities, slots, literals, predicates, variables, facts, and patterns.
- `forthdb-conformance` parses and validates `conformance/v1/kernel_cases.json`.

It does **not** yet implement:

- definition storage
- current-head indexes
- `define` or `forget`
- query execution
- symbols or compiled patterns
- committed worlds
- persistence or recovery
- benchmarks

The parser validates the fixture as a versioned contract before a future Rust engine executes it. Fixture entity names remain local labels and are materialized into runtime `EntityId` values through an explicit mapping.

Run the current Rust checks from the repository root:

```bash
cargo test --manifest-path rust/Cargo.toml
cargo run --quiet --manifest-path rust/Cargo.toml \
  -p forthdb-conformance -- conformance/v1/kernel_cases.json
```

A successful parser report proves only that Rust understands and validates the conformance vocabulary. It does not yet claim semantic conformance with the Python kernel.

The next step is to implement the smallest in-memory definition store needed to execute the first `define`, `resolve`, `definitions`, `forget`, and history assertions. Query execution will follow only after those state-transition semantics pass their fixtures.
