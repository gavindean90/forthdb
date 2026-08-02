# ForthDB Conformance

This directory defines the language-neutral contract that independent ForthDB implementations must reproduce.

The conformance fixtures are deliberately narrower than the complete Python implementation. They freeze demonstrated logical behavior without freezing Python object layout, internal record identifiers, physical commit-frame bytes, timing, allocation strategy, or storage-engine choices.

## Version 1

`v1/` contains three kinds of evidence:

- `kernel_cases.json` — executable operations and assertions for the semantic kernel
- `library_expected.json` — the normalized result of the library application
- `deployment_expected.json` — the normalized result of the deployment control-plane application

Run the Python reference implementation with:

```bash
python conformance_runner.py
```

The default report is written to:

```text
artifacts/python-conformance-v1.json
```

## Fixture vocabulary

Entity names in a fixture are local symbolic labels. Their numeric runtime IDs are not part of the contract.

Atoms are encoded as:

```json
{"entity": "copy_1"}
{"literal": "v2"}
```

Pattern-only terms may also use:

```json
{"variable": "copy"}
```

Human-facing compilation may use:

```json
{"symbol": "Foundation"}
```

Facts and patterns use the same structural form:

```json
{
  "subject": {"entity": "copy_1"},
  "predicate": "located_at",
  "object": {"entity": "shelf_a"}
}
```

`kernel_cases.json` currently exercises:

- two-hop indexed joins
- slot redefinition
- newest-first definition chains
- `forget` revealing the previous definition
- complete operation history
- compiled symbolic identity surviving rename and rebinding
- duplicate facts with distinct and provenance modes
- current-head lookup remaining independent of retained history

The application fixtures exercise the same normalized semantic projections through both:

- the direct Python semantic kernel
- the durable committed-world Python implementation

## What v1 does not freeze

Conformance v1 does not require:

- Python-compatible classes or APIs
- numeric entity IDs
- list-position record IDs
- byte-identical commit logs across languages
- the current Python frame encoding
- the current world digest algorithm
- query-plan identity
- performance numbers
- filesystem or concurrency mechanisms

Those may receive separate versioned contracts after they are deliberately specified.

## Rust milestone

The first Rust kernel milestone is complete when it can consume `kernel_cases.json` and produce the same normalized results.

The next application milestone is complete when Rust reproduces the checked-in library and deployment projections.

Durable cross-language world identity will be addressed only after a canonical logical commit and digest specification exists.
