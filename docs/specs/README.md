# Specifications

| Spec | Scope |
|------|-------|
| [features.md](features.md) | Complete feature list (tiered) |
| [matching-engine.md](matching-engine.md) | Matching Engine (`crates/shared`) |
| [common-functions.md](common-functions.md) | Shared: Order Book Locking (`crates/shared`) |

## Architecture

For AWS deployment architecture and infrastructure details, see [../aws-architecture.md](../aws-architecture.md).

## Note: Legacy Specs

The specs in this directory (`features.md`, `matching-engine.md`, `common-functions.md`) were written
pre-implementation and are **partially outdated**. They describe architectural concepts that were
later refined (e.g., stream-based locking replaced by Lua EVAL, worker-lambda removed in favor of inline mode).

**Behavioral specs as of current implementation:** see `docs/openspec/specs/`
**Architectural decisions:** see `docs/adr/`
