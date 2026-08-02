# ForthDB World Contract

## Purpose

This document defines the semantic contract of ForthDB's committed-world model. It intentionally specifies semantics rather than storage.

## Core Principle

The authoritative state of a ForthDB database is an immutable sequence of valid worlds. Transactions construct candidate successor worlds; they never mutate an existing world.

## World

A World is the complete immutable logical state visible to readers. It contains active slot definitions, allocator state, version information, and any other authoritative kernel state. Derived indexes and caches are implementation details.

## Transaction

A Transaction has exactly one base World and an ordered sequence of operations. It constructs a private Candidate World.

## Candidate World

A Candidate World is visible only during transaction execution. It either becomes a committed World after validation and durable publication or is discarded.

## Validation

Validation is deterministic and checks kernel invariants plus any application-defined invariants. Validation never mutates the Candidate World.

## Commit Frame

A Commit Frame is the durable description of one successful transition from World N to World N+1. It is immutable and is the smallest durable publication unit.

## Publication

Publication is atomic. Readers observe either the previous World or the newly committed World, never an intermediate state.

## Commit Store

A Commit Store records Commit Frames. Its semantic responsibilities are limited to appending and enumerating commit frames. Storage technology (memory, files, mmap, io_uring, etc.) is an implementation concern.

## Recovery

Recovery reconstructs the newest valid World from committed history, ignoring incomplete commit frames and never inventing state.

## Derived Structures

Indexes, caches, compiled queries, and storage-specific metadata are derived state and must always be reconstructible from committed history.

## Historical Truth

History is authoritative. Rollback creates a new World rather than erasing prior Worlds.

## Implementation Milestones

1. MemoryCommitStore
2. FileCommitStore
3. MmapCommitStore
4. IoUringCommitStore
5. MmapIoUringCommitStore
