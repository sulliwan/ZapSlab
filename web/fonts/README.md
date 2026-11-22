# Fonts

Vendored from the pinned zed revision (`assets/fonts/`), which is the same set
Zed ships:

| Family | Files | License |
|---|---|---|
| Lilex (`.ZedMono`) | `Lilex-{Regular,Bold,Italic,BoldItalic}.ttf` | `OFL.txt` |
| IBM Plex Sans (`.ZedSans`) | `IBMPlexSans-{Regular,SemiBold,Italic,SemiBoldItalic}.ttf` | `license.txt` |

These are embedded in the wasm binary via `include_bytes!` in `src/main.rs`
(`bundled_fonts()`), not fetched at runtime, so the app needs no network beyond
the initial asset load and works offline immediately. The files stay in the
repo because the build reads them at compile time; they are not copied into
`dist/`.
