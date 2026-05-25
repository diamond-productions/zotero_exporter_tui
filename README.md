# Zotero Exporter TUI (Rust)

Terminal UI tool to select a Zotero collection/subcollection and export all PDFs from that branch.
By default, files already present in the destination path are skipped. You can toggle force-export from the TUI when needed.
You can also skip the TUI and export a collection directly from the CLI with a slash-delimited collection path.

## Run

```bash
cd zotero_exporter_tui
cargo run -- \
  --zotero-base-path "/path/to/Zotero" \
  --destination-dir "/path/to/export"
```

Export a specific collection or subcollection directly from the CLI:

```bash
cargo run -- \
  --zotero-base-path "/path/to/Zotero" \
  --destination-dir "/path/to/export" \
  --collection "Parent Collection/Subcollection With Spaces"
```

Filter by one or more Zotero tags:

```bash
cargo run -- \
  --zotero-base-path "/path/to/Zotero" \
  --destination-dir "/path/to/export" \
  --tag "reviewed" \
  --tag "course"
```

Tag filters use AND matching. If you select or pass multiple tags, a PDF is exported only when its parent Zotero item has every selected tag. The collection list is narrowed to collections whose branch contains at least one item matching all selected tags. CLI mode skips files already present in the destination by default. Add `--force-export-all` if you want to overwrite that behavior and copy everything again.

## Build

Compile an optimized binary:

```bash
cargo build --release
```

The binary will be available at `target/release/zotero_exporter_tui`.

Run the compiled binary directly:

```bash
./target/release/zotero_exporter_tui \
  --zotero-base-path "/path/to/Zotero" \
  --destination-dir "/path/to/export"
```

## Shell Completion

Generate a completion script for your shell:

```bash
cargo run -- --generate-completion bash
```

The generated script uses runtime completion, so once the completion script is loaded your shell can tab-complete:

- `--zotero-base-path`
- `--destination-dir`
- `--collection`
- `--tag`

Example for Bash:

```bash
source <(cargo run -- --generate-completion bash)
```

`--collection` and `--tag` completion read from the Zotero database after `--zotero-base-path` is provided. Collection paths use forward slashes between collection names, and collection names themselves may contain spaces, so quote the full argument when running commands manually.

## Controls

- `Up` / `k`: move up
- `Down` / `j`: move down
- `t`: open tag filter picker; selected tags use AND matching
- Type in tag picker: fuzzy-find possible tags
- `Backspace` in tag picker: edit tag search
- `Enter` in tag picker: toggle selected tag
- `Esc` in tag picker: return to collection picker
- `c` in collection picker: clear selected tag filters
- `f`: toggle between skipping existing files and forcing export of all files
- `Enter`: export selected collection + all subcollections
- `q` or `Esc`: quit without exporting
