# storage/

Page-based storage engine.

## Files

| File      | Purpose                                         |
|-----------|-------------------------------------------------|
| `page.rs` | Constants (`DEFAULT_PAGE_SIZE`) and type aliases (`PageId`) |

## Planned

- Buffer pool for caching pages in memory
- Tablespace manager for reading/writing data pages from `.DAT` files
- Page layout (header, slot directory, row data)
