mod app;
mod storage;
mod transforms;

use std::borrow::Cow;

use gpui::{App, AppContext as _, WindowOptions};

fn main() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    gpui_platform::application().run(|cx: &mut App| {
        // The web backend has no system fonts. GPUI's virtual font names
        // resolve as follows:
        //   .SystemUIFont -> falls back through .ZedSans / .ZedMono
        //   .ZedSans      -> "IBM Plex Sans"
        //   .ZedMono      -> "Lilex" (not registered; the editor asks for
        //                    "Iosevka Slab" by name, see `MONO_FONT_FAMILY`)
        // Embedding the families keeps the app self-contained (no font
        // fetches, instant offline start) and avoids the runtime panic when
        // text is first laid out with no fonts registered.
        cx.text_system()
            .add_fonts(bundled_fonts())
            .expect("failed to register bundled fonts");

        cx.open_window(WindowOptions::default(), |_window, cx| {
            cx.new(|cx| app::Scratchpad::new(cx))
        })
        .expect("failed to open window");
    });
}

fn bundled_fonts() -> Vec<Cow<'static, [u8]>> {
    vec![
        // Default monospace. Only the Regular face is bundled: the editor
        // never renders bold/italic mono, and each Iosevka weight is ~8 MB in
        // the wasm binary. The unhinted build is used because the web renderer
        // disables hinting (gpui_wgpu uses `cosmic_text::Hinting::Disabled`).
        Cow::Borrowed(include_bytes!("../web/fonts/IosevkaSlab-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-Italic.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-SemiBoldItalic.ttf")),
    ]
}
