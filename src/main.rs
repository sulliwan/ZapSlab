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
        //   .ZedMono      -> "Lilex"
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
        Cow::Borrowed(include_bytes!("../web/fonts/Lilex-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/Lilex-Bold.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/Lilex-Italic.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/Lilex-BoldItalic.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-SemiBold.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-Italic.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/IBMPlexSans-SemiBoldItalic.ttf")),
    ]
}
