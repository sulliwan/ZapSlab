# Fonts

Bundled at compile time via `include_bytes!` in `src/main.rs`
(`bundled_fonts()`), so the app needs no network beyond the initial asset load
and works offline immediately.

| Family | Files | License |
|---|---|---|
| Iosevka Slab (default monospace) | `IosevkaSlab-Regular.ttf` | `OFL.txt` |
| IBM Plex Sans (`.ZedSans` / `.SystemUIFont`) | `IBMPlexSans-{Regular,SemiBold,Italic,SemiBoldItalic}.ttf` | `license.txt` |

Iosevka Slab comes from the v34.9.1 release
(<https://github.com/be5invis/Iosevka/releases>) and is the editor's monospace
font because of its broad Unicode coverage. Only the Regular face is bundled:
the editor never renders bold or italic monospace, and every extra Iosevka face
adds roughly 8 MB to the wasm binary. The *unhinted* build is used because the
web renderer rasterizes with hinting disabled
(`cosmic_text::Hinting::Disabled` in `gpui_wgpu`), so hinting tables would be
dead weight.

The editor asks for the family by name (`MONO_FONT_FAMILY` in `src/app.rs`)
rather than GPUI's `.ZedMono` virtual name, which resolves to Lilex. Glyphs that
Iosevka lacks still go through the text system's normal fallback stack.

The files stay in the repo because the build reads them at compile time; they
are not copied into `dist/`.
