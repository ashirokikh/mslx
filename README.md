# mslx

**Microsoft Learn Exporter to EPUB.**

Turn a Microsoft Learn certification, exam, learning path, course, or module into a single,
well-structured EPUB study book for offline reading: paths become chapters, modules and units
become sections, knowledge-check questions are rendered for self-testing, and a sources appendix
cites every original page.

A hosted browser version runs the same engine (compiled to WebAssembly) entirely on your machine
at <https://mslx.ashirokikh.com>.

## How it works (the whole flow)

1. **Resolve.** mslx takes your input (a cert/exam/path/course/module, as a URL or a bare code
   like `az-305`) and resolves it to its learning paths, modules, and units through the Microsoft
   Learn catalog API.
2. **Read the content.** Each unit's Markdown is read from the public
   [`MicrosoftDocs/learn`](https://github.com/MicrosoftDocs/learn) repository - the open mirror
   of Microsoft's authoring source.
3. **Scrape fallback.** Some tracks (for example Dynamics 365 / Business Central, parts of Power
   Platform) are authored in private repos with no public mirror. For those, mslx falls back to
   Learn's rendered Markdown endpoint so they still export; truly unavailable content is reported
   clearly rather than silently dropped.
4. **Assemble.** Images are downloaded and embedded, knowledge-check questions are rendered, and
   everything is packaged into a navigable EPUB with a sources appendix.

The engine (`mslx-core`) is **IO-agnostic**: all network access goes through a single `Fetcher`
trait, the only seam between the engine and the outside world. That lets the exact same code
compile to a **native binary** (using `reqwest`) and to **WebAssembly** (using the browser's
`fetch`), so the CLI and the hosted browser app share one codebase.

## Privacy and trust

- The **CLI runs entirely on your machine** and fetches directly from Microsoft Learn and GitHub.
  It has no accounts, no config phone-home, and **no telemetry**: nothing is sent anywhere except
  the public content requests it makes to build your book.
- The **hosted browser version** runs this same engine as WebAssembly on your machine. It adds a
  thin same-origin proxy (for the few resources that aren't CORS-enabled) and light usage
  analytics to decide which exams to feature; what it does and does not collect is documented in
  the [mslx-web](https://github.com/ashirokikh/mslx-web) repo. The book itself is always built
  locally and never uploaded.
- It's all open source - read it, build it, or run the CLI and skip the website entirely.

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

A bare code is accepted in any common form (`az-305`, `az305`, `AZ-305`). Other subcommands:

- `mslx <cert>` prints the resolved tree (paths, modules, units); add `--json` for machine output.
- `mslx epub <module-uid> [out.epub]` exports a single module.
- `mslx unit <module-url-or-slug> <unit-uid>` prints one unit's Markdown.
- `mslx questions <cert> [out.json]` exports the knowledge-check bank as domain-tagged JSON.
- `mslx --help` shows full usage. `--locale <code>` selects a locale (default `en-us`).

## Layout

- `crates/core` - the engine: resolution, scrape fallback, EPUB assembly and packaging. The
  `Fetcher` trait is the only IO seam.
- `crates/cli` - the native `mslx` command-line tool (this binary).
- `crates/wasm` - WebAssembly bindings (`wasm-bindgen`) for the browser app.

Native `cargo build`/`test` cover `core` + `cli`; the wasm crate is built separately:

```
wasm-pack build crates/wasm --target web --out-dir pkg
```

## Content and licensing

The code is MIT licensed (see [LICENSE](./LICENSE)). Exported content is Microsoft's, published
under CC BY 4.0 for its Learn modules; every book cites its sources. mslx does not host, mirror,
or relicense that content - it packages what Learn already serves publicly into an EPUB for
offline study. The bundled Carlito font, used only on WebAssembly to render diagram text, is
under the SIL Open Font License (see [crates/core/fonts/OFL.txt](./crates/core/fonts/OFL.txt)).
