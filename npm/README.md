# rustrouter

A local AI routing gateway with a Vue dashboard. One OpenAI-compatible endpoint
(`/v1/*`) fans out across the upstream providers you configure, with format
translation, model-combo fallback, multi-account fallback, OAuth and API-key
credential management, token refresh, usage tracking, and an optional quota
tracker. It listens on **20129** and shares one SQLite file with 9router.

## Install

```bash
npm install -g rustrouter
```

If npm reports `EACCES` on `/usr/lib/node_modules`, your Node came from a distro
package and the global prefix belongs to root. Use a prefix you own:

```bash
npm config set prefix ~/.local
npm install -g rustrouter
```

Use `npm config set` rather than a one-off `--prefix`: the config form is
persisted in `~/.npmrc`, which the in-app update command (`npm i -g
rustrouter@latest`) needs later. If the command is not found, add
`$HOME/.local/bin` to `PATH`. Windows needs no change; npm installs into
`%AppData%\npm`.

To try it without installing: `npx rustrouter serve`.

On Termux, npm 12 blocks the postinstall script that downloads the Android
binary. Install with:

```bash
npm install -g --allow-scripts=rustrouter rustrouter
```

## Run

```bash
rustrouter serve           # foreground
rustrouter start           # restart on crash
rustrouter stop
rustrouter --port 8080
```

Then open `http://localhost:20129/dashboard`.

Full documentation, build instructions, and the Docker image are in the
[repository](https://github.com/walujanle/rustrouter).

## License

MIT.
