mod app;
mod commands;
mod find;
mod storage;
mod transforms;

use std::borrow::Cow;

use gpui::{App, AppContext as _, WindowOptions};

fn main() {
    #[cfg(target_arch = "wasm32")]
    console_error_panic_hook::set_once();

    application().run(|cx: &mut App| {
        // The web backend has no system fonts. GPUI's virtual font names
        // resolve as follows:
        //   .SystemUIFont -> falls back through .ZedSans / .ZedMono
        //   .ZedSans      -> "IBM Plex Sans"
        //   .ZedMono      -> "Lilex"
        // Embedding the families keeps the app self-contained (no font
        // fetches, instant offline start) and avoids the runtime panic when
        // text is first laid out with no fonts registered.
        //
        // Lilex and IBM Plex Sans cover Latin, Latin Extended, Greek, and
        // Cyrillic. The Noto faces in `bundled_fonts` cover the other common
        // scripts (Arabic, Hebrew, Devanagari, Thai); cosmic-text scans the
        // font database for a face when the primary family lacks a glyph, so
        // they are picked up as fallbacks automatically. CJK and emoji are
        // handled by the browser instead — see `application`.
        cx.text_system()
            .add_fonts(bundled_fonts())
            .expect("failed to register bundled fonts");

        cx.open_window(WindowOptions::default(), |_window, cx| {
            cx.new(|cx| app::Scratchpad::new(cx))
        })
        .expect("failed to open window");
    });
}

/// Builds the platform application.
///
/// On the web we construct `WebPlatform` directly instead of using
/// `gpui_platform::application()` so we can opt into
/// `CanvasFontFallback::EmojiAndCjk`. The web backend has no system fonts and
/// the bundled families do not cover CJK, so without this CJK would render as
/// tofu. The Canvas fallback renders emoji and eligible horizontal CJK through
/// the browser's own fonts; `gpui_platform`'s helpers only expose the default
/// emoji-only policy.
fn application() -> gpui::Application {
    #[cfg(target_family = "wasm")]
    {
        let platform: std::rc::Rc<dyn gpui::Platform> =
            std::rc::Rc::new(gpui_web::WebPlatform::new_with_backend_and_font_fallback(
                true,
                gpui_web::WebBackendPreference::Auto,
                gpui_web::CanvasFontFallback::EmojiAndCjk,
            ));
        gpui::Application::with_platform(platform)
    }
    #[cfg(not(target_family = "wasm"))]
    {
        gpui_platform::application()
    }
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
        // Fallback families for scripts neither bundled family covers. These are
        // never selected as a primary family (they have no Latin `m` glyph), but
        // cosmic-text falls back to them from the font database when the primary
        // face lacks a glyph.
        Cow::Borrowed(include_bytes!("../web/fonts/NotoSansArabic-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/NotoSansHebrew-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/NotoSansDevanagari-Regular.ttf")),
        Cow::Borrowed(include_bytes!("../web/fonts/NotoSansThai-Regular.ttf")),
    ]
}
