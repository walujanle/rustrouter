# rustrouter dashboard

The Vue 3 dashboard for rustrouter. Built with Vite, Tailwind v4, vue-router,
Pinia, and Biome. It is compiled to `dist/` and embedded into the Rust binary by
`router-server` at build time, so a release needs no Node at runtime.

## Develop

```bash
npm install
npm run dev        # Vite dev server, proxies /api and /v1 to localhost:20129
```

Run `rustrouter serve` in another terminal so the proxy has a backend to talk to.

## Build and check

```bash
npm run build      # vue-tsc -b && biome check && vite build
./node_modules/.bin/biome ci
npm audit
```

`npx biome ci` is unreliable here: npm resolves `ci` as a package name. Call the
local binary instead.

## Notes

- Route definitions live in `src/router/index.ts`; the API client is
  `src/utils/api.ts`.
- `src/style.css` holds the design tokens and the Tailwind v4 theme block. The
  `@import "tailwindcss" source(...)` scan base is `source("../")` — the `web/`
  root — so both `src/` and `index.html` are scanned. Get it wrong and the
  utility sheet comes out empty and the app builds unstyled.
- Icons are Material Symbols ligatures. They stay hidden until the font loads;
  the inline script in `index.html` flips the `.fonts-loaded` class and has a 3s
  timeout, which is the only error surface.
- Design decisions are in `../docs/FRONTEND.md`.
