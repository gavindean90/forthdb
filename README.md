# -forthdb

> **Milestone:** Freeze the kernel at v0.1. Build applications without modifying it. Record every friction point. Treat recurring friction—not intuition—as the source of future kernel evolution.

## The Spirit of the Project

There are many excellent databases.

This project is not an attempt to replace them.

It began with a much simpler question:

> *If we forgot everything we know about databases and started again from a very small set of ideas, what would naturally emerge?*

This repository is driven by curiosity, experimentation, and evidence rather than feature lists or benchmarks.

Whenever possible, we begin with the smallest executable idea we can imagine. We build it, observe it, and let the results teach us something. Only then do we decide whether the kernel itself should change.

The goal is not to invent complexity.

The goal is to discover simplicity.

Ideas are hypotheses.

Code is an experiment.

Regression tests are evidence.

The kernel is our current best explanation.

## Current Direction

We currently believe:

- Search and organization are more fundamental than computation.
- Stable identities should outlive human-readable names.
- History should be preserved rather than overwritten.
- Small primitives are preferable to large frameworks.
- Applications should reveal missing primitives.

These are working beliefs, not immutable truths. The project exists to test them.

## Success

Success is not measured by replacing existing databases.

Success is measured by whether this exploration teaches us something true about how databases, query engines, or information systems can be built from small, composable primitives.

The purpose of this repository is not to defend an idea.

It is to investigate one honestly.
