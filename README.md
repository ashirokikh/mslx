# mslx

Turn a Microsoft Learn certification, exam, or learning path into a single, well-structured
EPUB study book for offline reading: paths become chapters, modules and units become sections,
knowledge-check questions are rendered for self-testing, and a sources appendix cites every
original page.

A hosted browser version runs the same engine (compiled to WebAssembly) entirely on your
machine at <https://mslx.ashirokikh.com>.

## Install

```
cargo build --release
```

The binary is `target/release/mslx`.

## Usage

```
mslx book <cert-url-or-code> [out.epub]
```

Examples:

```
mslx book az-305
mslx book https://learn.microsoft.com/credentials/certifications/azure-solutions-architect/
mslx book https://learn.microsoft.com/training/paths/get-started-with-microsoft-azure-fundamentals/
```

A bare exam code is accepted in any common form (`az-305`, `az305`, `AZ-305`). Other
subcommands:

- `mslx <cert>` prints the resolved tree (paths, modules, units).
- `mslx epub <module-uid> [out.epub]` exports a single module.
- `mslx unit <module-url-or-slug> <unit-uid>` prints one unit's Markdown.
- `mslx --help` shows full usage.

## How it works

mslx resolves a certification or exam to its learning paths through the Microsoft Learn catalog
API, reads each unit's Markdown from the public `MicrosoftDocs/learn` repository (the open
mirror of Microsoft's authoring source), and assembles everything into a navigable EPUB. The
engine (`mslx-core`) is IO-agnostic and compiles to both native and WebAssembly, so the CLI and
the browser app share one codebase.

Some content (for example Dynamics 365 / Business Central) is authored in private repositories
with no public mirror, so it resolves but cannot be exported; mslx reports this clearly.

## Layout

- `crates/core` - the engine: resolution, assembly, EPUB packaging.
- `crates/cli` - the `mslx` command-line tool.
- `crates/wasm` - WebAssembly bindings for the browser.

## Content and licensing

The code is MIT licensed (see [LICENSE](./LICENSE)). Exported content is Microsoft's, published
under CC BY 4.0 for its Learn modules; every book cites its sources. The bundled Carlito font,
used only on WebAssembly to render diagram text, is under the SIL Open Font License (see
[crates/core/fonts/OFL.txt](./crates/core/fonts/OFL.txt)).
