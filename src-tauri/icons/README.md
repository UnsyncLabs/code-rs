# Tauri icons (placeholders)

Tauri's bundler expects the icon files referenced by `src-tauri/tauri.conf.json`
under `bundle.icon`:

- `32x32.png`
- `128x128.png`
- `128x128@2x.png`
- `icon.icns`   (macOS)
- `icon.ico`    (Windows)

These are **not** committed yet. Generate them from a single square PNG
(1024x1024 recommended) once the tauri CLI is installed:

```sh
# from the repo root, after `cargo install tauri-cli --version '^2'`
cargo tauri icon path/to/app-icon.png
```

That command writes all of the above files into `src-tauri/icons/`.

Until real icons exist you can either run `cargo tauri icon` on any PNG, or
temporarily remove the `bundle.icon` entries from `tauri.conf.json` for
`cargo build`/`cargo check` (a plain build does not require the icons; only
bundling does).
