# ForthDB File Format

## Status

This document specifies the experimental version 1 format written by `FileCommitStore`. It defines physical encoding and recovery behavior without changing the semantic rules in `WORLD_CONTRACT.md`.

All integers are unsigned little-endian values. All strings are UTF-8 preceded by their byte length as a `u64`.

## File header

Every nonempty file begins with a fixed 16-byte header.

| Offset | Size | Field | Version 1 value |
| ---: | ---: | --- | --- |
| 0 | 8 | Magic | ASCII `FTHDB001` |
| 8 | 4 | Format version | `1` |
| 12 | 4 | Flags | `0` |

Unknown versions or nonzero flags fail closed.

## Commit-frame record

Frames follow the header consecutively with no padding.

| Size | Field |
| ---: | --- |
| 4 | Record magic: ASCII `FRM1` |
| 8 | Payload length |
| 8 | Payload checksum |
| payload length | Canonical payload |
| 4 | Completion trailer: ASCII `END1` |

The version 1 checksum is 64-bit FNV-1a over the payload bytes. It is an integrity check for accidental corruption, not an authentication mechanism.

The completion trailer distinguishes a complete record from an interrupted append after enough of the prefix has been written.

## Canonical payload

The fixed payload prefix is:

| Size | Field |
| ---: | --- |
| 8 | Parent world identifier |
| 8 | Resulting world identifier |
| 8 | Parent version |
| 8 | Resulting version |
| 8 | Resulting allocator state |
| 8 | Operation count |

Operations then appear in transaction order.

### Allocate entity

| Size | Field |
| ---: | --- |
| 1 | Tag `0` |
| 8 | Entity identifier |

### Define slot

| Size | Field |
| ---: | --- |
| 1 | Tag `1` |
| variable | Slot string |
| variable | Subject atom |
| variable | Predicate string |
| variable | Object atom |

### Forget slot

| Size | Field |
| ---: | --- |
| 1 | Tag `2` |
| variable | Slot string |

### Atoms

An entity atom is tag `0` followed by an entity identifier. A literal atom is tag `1` followed by a string.

No trailing bytes are permitted inside a payload. Unknown tags, invalid UTF-8, impossible lengths, and noncanonical structure fail closed.

## Limits

Version 1 rejects payloads larger than 64 MiB and frames declaring more than 1,000,000 operations. These are defensive decoding limits, not semantic database limits.

## Append and publication

`FileCommitStore::append` performs the following sequence:

1. Verify that the frame extends the cached linear history by exactly one version.
2. Canonically encode the complete record.
3. Append the record with ordinary file I/O.
4. Call `sync_data()`.
5. Return success to the database transaction engine.

The database publishes the new immutable `World` only after step 5 succeeds. If writing or synchronization fails, the store attempts to truncate back to the pre-append offset and returns an error without publishing.

## Reopening and recovery

Opening a file reads and validates frames from the beginning.

For every complete record it verifies:

- record magic and completion trailer
- declared length and defensive limits
- checksum
- canonical payload decoding
- parent world and version continuity
- exactly one version of forward progress
- allocator behavior, kernel invariants, and resulting world identity through logical reconstruction

An incomplete final record is ignored and the file is truncated to the end of the newest complete frame. Established corruption in the header or any complete record fails closed; recovery never skips a corrupt committed frame or invents state.

## Intentional exclusions

Version 1 does not define:

- mmap access
- io_uring submission
- checkpoints
- compaction
- compression
- encryption or authentication
- cross-process writer locking
- distributed consensus

Those mechanisms may be added behind the same `CommitStore` and committed-world contracts.
