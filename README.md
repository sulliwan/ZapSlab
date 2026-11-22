# ZapSlab

## What is this?

It's a scratchpad heavily inspired by [Boop](https://github.com/IvanMathy/Boop), but running as a WASM web app to make it available for all platforms. We are using [GPUI](https://gpui.rs/) as our UI framework. The app is entirely self-contained and can be hosted as a static site. Scratchpad contents are persisted to IndexedDB. Nothing gets uploaded to any server.

## How do I use this?

Feel free to just use the public site [ZapSlab](https://zabslab.app). To build and host yourself, just `make web` and host the files in `dist` somewhere.

The actual app UI itself is very simple - you have a text buffer, a few toggles in statusbar and you hit `ctrl+k` (or `ctrl+shift+space`) to access the command palette. The commands will apply to selection or the whole contents if nothing is selected. Ctrl+f for find/replace.

You can install it as a PWA and use as a desktop app.

## FAQ

**Q: Is this vibe-coded?**
> A: Yes. Nearly all code is written in Zed Agent harness by Deepseek V4.1 Flash. Deepseek V4 Pro was used to plan out more complex tasks. I provided directorial input and the icon.

**Q: Is this secure?**
> A: Probably? The WASM sandbox and Rust provide decent enough security guarantees. Maybe don't use this to process credit card PANs though. The local IndexedDB is not encrypted, so bad guys can steal your text buffer if you get pwned. I have plans to solve this with device-bound encryption keys at some point.

**Q: Are mobile devices supported?**
> A: No, not planned right now. Perhaps in the future, but unlikely.

**Q: Are you accepting contributions?**
> A: No, feel free to file bug reports or feature requests to Github issues.
