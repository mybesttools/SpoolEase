# SpoolEase Copilot Instructions

## Build Overview

SpoolEase firmware is an embedded Rust project targeting the ESP32-S3 (`xtensa-esp32s3-none-elf`).
The workspace has two Rust crates:

- `core/` — firmware binary (the main ESP32-S3 application)
- `shared/` — library shared between firmware and any host tools

## Required Toolchain

The firmware uses a custom Rust toolchain (`esp1`) that includes the xtensa Rust backend and associated
linker (`xtensa-esp32s3-elf-gcc`). This is managed by [espup](https://github.com/esp-rs/espup).

After running `espup install`, the toolchain environment is activated by sourcing:
```sh
source ~/export-esp.sh
```
This puts the xtensa linker on `PATH`. Without it, `cargo build` fails with
_"linker `xtensa-esp32s3-elf-gcc` not found"_.

The `core/.cargo/config.toml` configures the default build target and linker flags.
The `core/rust-toolchain.toml` pins the channel to `esp1`.

## Build Sequence

Always build from the `core/` directory:

```sh
cd core/
source ~/export-esp.sh          # adds xtensa linker to PATH
source ./deploy-shell-init.sh   # sets CARGO_CMD, sources cargo env
"$CARGO_CMD" build --release    # compiles firmware (4–5 min from clean)
```

After a successful build the ELF is at:
`core/target/xtensa-esp32s3-none-elf/release/SpoolEase`

## Packaging (OTA .bin)

The deploy scripts call `cargo xtask ota build` (from `esp-hal-app`).
**The xtask only packages a pre-built ELF — it does NOT call `cargo build` itself.**

```sh
# After cargo build --release:
cd /path/to/esp-hal-app   # found via deploy-vars.sh / ~/.cargo/git/checkouts
"$CARGO_CMD" xtask ota build \
  --input /path/to/SpoolEase/core \
  --output /path/to/SpoolEase/build/bins/0.6/console/ota
```

The deploy scripts (`deploy-rel.sh`, `deploy-beta.sh`, `deploy-debug.sh`) now run both steps
automatically when executed from `core/`.

## Static HTML Embedding (Important!)

All web pages served by the firmware are embedded at **compile time** via `include_bytes_gz!` proc
macros (from `framework_macros`). This macro gzip-compresses the file and embeds it in the binary.

**Cargo does NOT detect changes to embedded static files automatically.**
If you modify any file in `core/static/` (e.g., `config.html`, `inventory/index.html`, `styles.css`),
you must force a recompile of `main.rs` or `web_app.rs` before rebuilding:

```sh
touch core/src/main.rs     # forces main.rs (and config.html/captive.html) to recompile
touch core/src/web_app.rs  # forces web_app.rs (and styles.css) to recompile
# or clean only the SpoolEase crate (faster than full clean):
cargo clean -p SpoolEase --target xtensa-esp32s3-none-elf --release
```

A tell-tale sign that stale HTML is embedded: the binary size does not change between builds that
should have different HTML content.

## Flashing

```sh
esptool.py --port /dev/cu.usbmodem101 --baud 921600 write_flash \
  0x200000 build/bins/0.6/console/ota/SpoolEase-0.6.2.bin \
  0x900000 build/bins/0.6/console/ota/SpoolEase-0.6.2.bin
```

Or use the cargo runner (requires the device to be connected):
```sh
cargo run --release
```

## SpoolRecord CSV Format

The SpoolEase inventory uses a 15-column CSV (comma-delimited):

```
id,brand,material,subtype,color_name,rgba,label_weight,core_weight,note,slicer_filament,full_unused,tag_id,assigned_location,actual_location,spools_count
```

- `rgba` — color in `#RRGGBBAA` format (8-digit hex with alpha, e.g. `#FF6A13FF`)
- `spools_count` — number of physical spools; defaults to 1 on import
- `id` — SpoolEase internal ID; leave empty when importing new records

The inventory importer (`core/static/inventory/index.html`) also accepts BambuLabs-style CSV with
column aliases (`material_type`, `color_code`, `weight_advertised`, etc.) and auto-detects `;` or `,`
as delimiters.

`BambuLabs-Filament-V2.csv` in the repo root is a Bambu Labs filament catalog pre-formatted in the
native SpoolEase format, ready to import via the Spool Inventory Editor.

## Key Source Files

| File | Purpose |
|---|---|
| `core/src/main.rs` | Entry point; embeds `config.html` and `captive.html` |
| `core/src/web_app.rs` | HTTP request handlers; embeds `styles.css` |
| `core/src/store.rs` | CSV database; `query_spools()` pads old records to 24 columns |
| `core/src/spool_record.rs` | `SpoolRecord` struct (24 CSV columns) |
| `core/static/inventory/index.html` | Inventory web SPA; handles CSV import/export |
| `core/static/config.html` | Device configuration web UI |
| `core/deploy-shell-init.sh` | Shared bootstrap: sources espup + sets CARGO_CMD |
| `core/deploy-rel.sh` | Full release build + OTA + web-install packaging |
| `core/deploy-beta.sh` | Beta/unstable OTA packaging |
