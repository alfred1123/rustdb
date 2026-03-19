# server/

TCP listener, wire protocol, and session management. Not yet implemented.

## Planned

- Async TCP server via `tokio`
- Wire protocol for client-server communication
- Session management and connection pooling

## TODO — Catalog Cache Concurrency

When multi-session support is added, the `CatalogCache` must be shared
across sessions via `Arc<RwLock<CatalogCache>>`. Query threads take shared
read locks; DDL acquires a write lock. No cache eviction is needed — the
catalog is small enough (~5 MB at 1K tables) to stay fully resident.
