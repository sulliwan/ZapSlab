# Zapslab

## What is this?

It's a scratchpad similar to [Boop](https://github.com/IvanMathy/Boop), but running as a WASM web app to make it available for all platforms. We are using [GPUI](https://gpui.rs/) as our UI framework. The app is entirely self-contained and can be hosted as a static site. Scratchpad contents are persisted to localstorage. Nothing gets uploaded to any server.

## How do I use this?

Currently there is no public demo site or official builds yet. But basically just `make web` and host the files in `dist` somewhere.

The actual app itself is very simple - you have a text buffer, a few toggles in statusbar and you hit `ctrl+shift+space` to access the command palette. The commands will apply to selection or the whole contents if nothing is selected.

You can install it as a PWA.

## FAQ

**Q: Is this vibe-coded?**
> A: Yes. Nearly all code is written by Deepseek 4.1 Flash. I provided directorial input.
