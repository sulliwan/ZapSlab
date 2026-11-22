# ZapSlab

## What is this?

It's a scratchpad heavily inspired by [Boop](https://github.com/IvanMathy/Boop), but running as a WASM web app to make it available for all platforms. We are using [GPUI](https://gpui.rs/) as our UI framework. The app is entirely self-contained and can be hosted as a static site. Scratchpad contents are persisted to IndexedDB. Nothing gets uploaded to any server.

## How do I use this?

Currently there is no public demo site or official builds yet. But basically just `make web` and host the files in `dist` somewhere.

The actual app itself is very simple - you have a text buffer, a few toggles in statusbar and you hit `ctrl+k` (or `ctrl+shift+space`) to access the command palette. The commands will apply to selection or the whole contents if nothing is selected.

You can install it as a PWA.

### Deploying to Cloudflare Pages

`make deploy` builds `dist/` and uploads it with [Wrangler](https://developers.cloudflare.com/workers/wrangler/), all inside a container, so the host only needs `make` and a container runtime.

First create `wrangler.toml` (gitignored) with your Pages project settings:

```toml
name = "zapslab"
pages_build_output_dir = "dist"
# account_id = "..." # optional, when the login sees multiple accounts
```

Then run `make deploy`. On the first run Wrangler prints a URL and code for an OAuth device-flow login; open the URL on the host, enter the code, and approve. The credentials are stored in `.wrangler/` (gitignored), so later deploys reuse the login. `make deploy-login` runs the login without deploying.

`make deploy` publishes the branch named by `PAGES_BRANCH` (default `main`), which Cloudflare treats as production only when it matches the project's production branch. Any other value creates a preview deployment, e.g. `make deploy PAGES_BRANCH=vibing`.

## FAQ

**Q: Is this vibe-coded?**
> A: Yes. Nearly all code is written by Deepseek 4.1 Flash. I provided directorial input.
