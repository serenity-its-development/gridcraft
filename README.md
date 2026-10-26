# Sheets

Sheets is OneOS's spreadsheet: it opens, edits and saves Excel (.xlsx) and OpenDocument (.ods) workbooks, running in the browser as
WebAssembly inside OneOS, and edits one workbook live with other people through OneOS's collaboration room.

It is based on **GridCraft** by the ArtCraft team (MIT OR Apache-2.0). This repository is OneOS's fork: it keeps the upstream crate
names and file layout so upstream changes merge cleanly, and carries the OneOS host bridge and the OneOS identity.

## What is different from upstream

- **Host bridge.** The web build (`apps/gridcraft-web`) talks to OneOS over `postMessage` to the one parent origin the frame page
  declares: open from bytes, report unsaved changes, save as a new version of the file with a stated base version
  (`src/host.rs`), take the assistant's allow-listed commands (`crates/ui-egui/src/host_commands.rs`), and edit live
  (`src/collab.rs`).
- **Live editing shim.** `crates/collab` (`gridcraft-collab`) maps the workbook onto OneOS's shared operations: sheets, row and column
  ids, cells keyed `<rowId>:<colId>`, styles by content, row heights, column widths and merges. It is pure Rust, tested natively
  here and against OneOS's real CRDT engine from the OneOS repository. Not shared live: charts, pictures, conditional formats,
  validations, tables, pivots, names and the other workbook features; they are the file's, as opened.
- **Undo and Redo** go to the shared workbook while a workbook is live, so they never take back what others typed.
- **Size build.** `cargo build --profile web-release --target wasm32-unknown-unknown -p gridcraft-web --locked`, then
  `wasm-bindgen --target web` and `wasm-opt -Oz`. The OneOS build script (`workspace/scripts/build-craft-libs.mjs`) does all of it.
- **Identity.** The product is called Sheets; the app id is `one.wkspace.sheets`; the upstream community links and brand marks are
  removed.

## Building the desktop app

```
cargo run --release -p gridcraft
```

## Licence

Dual-licensed under [MIT](LICENSE-MIT) or [Apache-2.0](LICENSE-APACHE), at your option. The upstream copyright notice is kept in
[LICENSE-MIT](LICENSE-MIT) and [NOTICE](NOTICE); third-party material is listed in [ATTRIBUTION.md](ATTRIBUTION.md).
