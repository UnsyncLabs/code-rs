# code-rs logo

| File | Use |
|---|---|
| `code-rs.svg` | Vector source of the logo |
| `code-rs-1024.png` | 1024×1024 raster, source for the app icons |

The app icons in [`src-tauri/icons/`](../../src-tauri/icons) (`.ico`, `.icns`, the PNG sizes and
the Windows Store logos) are generated from the PNG:

```bash
cd src-tauri
cargo tauri icon ../resources/code-rs/code-rs-1024.png
```

Commit the regenerated `src-tauri/icons/` together with any logo change.
