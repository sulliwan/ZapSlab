# Fonts

The UI families are vendored from the pinned zed revision (`assets/fonts/`),
which is the same set Zed ships. The Noto faces are fallback-only: they cover
scripts Lilex and IBM Plex Sans do not, and are never selected as the primary
family.

| Family | Files | License |
|---|---|---|
| Lilex (`.ZedMono`) | `Lilex-{Regular,Bold,Italic,BoldItalic}.ttf` | `OFL.txt` |
| IBM Plex Sans (`.ZedSans`) | `IBMPlexSans-{Regular,SemiBold,Italic,SemiBoldItalic}.ttf` | `license.txt` |
| Noto Sans Arabic | `NotoSansArabic-Regular.ttf` | `LICENSE-Noto.txt` |
| Noto Sans Hebrew | `NotoSansHebrew-Regular.ttf` | `LICENSE-Noto.txt` |
| Noto Sans Devanagari | `NotoSansDevanagari-Regular.ttf` | `LICENSE-Noto.txt` |
| Noto Sans Thai | `NotoSansThai-Regular.ttf` | `LICENSE-Noto.txt` |

Lilex and IBM Plex Sans cover Latin, Latin Extended, Greek, and Cyrillic. The
embedded Noto faces cover Arabic, Hebrew, Devanagari, and Thai. CJK and emoji
are *not* bundled: the web text system's Canvas fallback renders them through
the browser's own fonts (see `CanvasFontFallback::EmojiAndCjk` in
`src/main.rs`).

All files are embedded in the wasm binary via `include_bytes!` in
`src/main.rs` (`bundled_fonts()`), not fetched at runtime, so the app needs no
network beyond the initial asset load and works offline immediately. The files
stay in the repo because the build reads them at compile time; they are not
copied into `dist/`.
