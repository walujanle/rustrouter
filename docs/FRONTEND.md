# Frontend

`web/` is a Vue 3 + Vite single-page app: vue-router in history mode, Pinia for state, Tailwind v4 for styling, Biome for lint and format, and a fetch wrapper in `src/utils/api.ts`. It is client-rendered end to end — every page is a component that fetches from the Rust API on mount. The release build embeds `web/dist` into the binary (see Serving).

For orientation: 105 `.vue` files, 72 under `src/views` and 32 under `src/components`, 5 Pinia stores, and a 753-line `src/style.css`.

## Libraries

| Package | Used for |
|---|---|
| `vue-router` ^5 | routing, `createWebHistory` |
| `pinia` ^4 | the 5 stores |
| `vue-chartjs` ^5 + `chart.js` ^4 | 3 charts (`UsageChart`, `TopModelsChart`, `ProviderBarChart`) |
| `@vue-flow/core` ^1 | `ProviderTopology`, `UsageStats` |
| `@guolao/vue-monaco-editor` ^1 (+ `monaco-editor` ^0.57) | the `translator` page |
| `vue-draggable-plus` ^0.6 | `ComboFormModal` |
| `material-symbols` ^0.47.5 | icon font (CSS + woff2 only) |

## Library notes

- **vue-chartjs.** Three chart shapes total, each with a tokens/requests/cost view toggle. Chart.js runs `responsive: true` / `maintainAspectRatio: false`, so the canvas fills its container.
- **`@vue-flow/core`.** `ProviderTopology` is a static graph (`nodesDraggable: false`). `Handle`/`Position`/`Controls`/`BaseEdge`/`getBezierPath` are the API; the `import '@vue-flow/core/dist/style.css'` is load-bearing, and the controls are restyled in the `.vue-flow-controls-custom` block of `src/style.css`.
- **`@guolao/vue-monaco-editor`.** One file (`TranslatorPage.vue`), with a fixed `EDITOR_OPTIONS` object.
- **`vue-draggable-plus`.** One file (`ComboFormModal.vue`): a vertical sortable with a drag handle.

## Scaffold

**`vite.config.ts`**: `resolve.alias` maps `@` → `src`, and `server.proxy` maps `/api` and `/v1` to `http://localhost:20129` for `vite dev`. The release build is embedded into that same server, so production needs no proxy. A `htmlOneLine` plugin's `transformIndexHtml` post hook emits `dist/index.html` on one line: it masks the `<script>`/`<style>` bodies, collapses every other run of whitespace, drops comments, folds the inline scripts through rolldown's `minifySync` (the same oxc pass Vite runs on the bundles, so newlines inside JS fold without breaking automatic semicolon insertion), then restores the blocks. Vite minifies the JS and CSS but writes the entry HTML verbatim, so without this the comments and 76 newlines ship.

**`index.html`**: the app shell. It carries the pre-paint theme script, the fonts-loaded script, and a `<noscript>` fallback that says the dashboard needs JS while `/v1/*` keeps working without a browser.

The pre-paint script does `JSON.parse(localStorage.getItem('theme')).state.theme` and mirrors the `dark` class it finds. The Pinia theme store (`src/stores/theme.ts`) writes that exact `{state:{theme}}` envelope, so the two must change together. Getting this wrong flashes the wrong theme on every reload.

**`src/style.css`**: the whole stylesheet, 753 lines. `@import "tailwindcss" source("../")` sets the Tailwind scan base to `web/`. Getting this wrong produces an empty utility sheet and a completely unstyled app **that still builds successfully**. Silent failure, high blast radius.

**`main.ts`**: loads the registry store, initialises the theme, then mounts the app with Pinia and the router.

**`npm run build` is the whole frontend gate**, in order: `vue-tsc -b`, then `biome check`, then `vite build`. Biome runs `check`, never `--write`, so a release build cannot rewrite source files or leave the working tree dirty.

Colour utilities resolve only through the tokens declared in `@theme inline` in `src/style.css`: `bg-surface`, `bg-surface-2`, `bg-surface-3`. A class naming a token that does not exist (`bg-bg-base`, `bg-bg-subtle`, `bg-background`) compiles to nothing and the element falls through to whatever sits behind it — a silent failure, not a build error.

**`biome.json`**: `html.experimentalFullSupportEnabled: true` (Biome 2.5 gates full Vue/Svelte/Astro support behind this flag) and `css.parser.tailwindDirectives: true` for the Tailwind directives in `style.css`. Tab indent and double quotes match the scaffold, and `assist.actions.source.organizeImports: "on"` covers import ordering. The `recommended` preset enables `useVueMultiWordComponentNames`, which reads the *filename*, not `defineOptions({ name })`, so a new `Button.vue` fires the rule while `UiButton.vue` does not. Vue-specific lint gaps (v-for keys, prop mutation) have no Biome equivalent; accept the gap rather than bolting on ESLint.

Verified against the installed 2.5.14 binary: `biome format --stdin-file-path=x.vue` reformats `<script setup>` blocks, and `biome lint` parses the template.

## API client (`src/utils/api.ts`)

Every dashboard fetch goes through this wrapper. Four behaviours matter:

- **A 30 s default timeout** via an `AbortController` (overridable per request). The router guard
  awaits a probe on every `/dashboard/*` navigation, so a backend that accepts the socket and stalls
  would otherwise blank the SPA with no error and no recovery.
- **The body is read as text and then parsed.** A non-JSON body (an HTML error page from a proxy)
  becomes `{ error: text }` rather than throwing inside `response.json()`.
- **Concurrent identical GETs share one in-flight request** (`inflight` map). A layout and its page
  mounting in the same tick otherwise fire the same call twice and the second response races the
  first.
- **A GET may opt into a TTL cache** with `cacheMs`; `invalidateCache(prefix)` drops entries after a
  mutation. The router guard caches `/api/settings/require-login` for 30 s; `/api/auth/status` is
  deliberately uncached so a logout is seen on the next navigation.

## Overlay stack (`src/utils/overlayStack.ts`)

`UiModal` and drawers register on a module-level stack instead of toggling
`document.body.style.overflow` directly. Body scroll is hidden only while the stack is non-empty,
Escape routes to the topmost overlay only, and Tab is trapped within the topmost container. Without
the stack a nested modal restores scrolling underneath its still-open parent and one Escape closes
both.

## Routes (vue-router, `createWebHistory`)

```
/                                        → redirect /dashboard
/landing, /login, /callback
/dashboard                               (EndpointPageClient)
/dashboard/endpoint
/dashboard/providers, /dashboard/providers/new, /dashboard/providers/:id
/dashboard/combos
/dashboard/usage
/dashboard/quota
/dashboard/token-saver
/dashboard/cli-tools, /dashboard/cli-tools/:toolId
/dashboard/console-log
/dashboard/translator
/dashboard/proxy-pools
/dashboard/skills
/dashboard/media-providers/web
/dashboard/media-providers/:kind, /dashboard/media-providers/:kind/:id
/dashboard/media-providers/combo/:id
/dashboard/profile
/dashboard/basic-chat
/dashboard/settings/pricing
```

A catch-all `/:pathMatch(.*)*` renders the not-found view.

Two layout slots: a `DashboardLayout` wrapper for `/dashboard/*` children, a bare layout for `/landing`, `/login`, `/callback`. The `basic-chat` route gets `flex`/`h-full`/no padding from a route-name check in `DashboardLayout`; everything else gets `max-w-7xl p-6 lg:p-10`.

The media-providers tree serves the four kinds in `MEDIA_PROVIDER_KINDS`: `embedding` and `systemone` have their own listing pages, `webSearch` and `webFetch` share the combined `/web` page, and every kind has a detail page and a combo editor. The `agent skills` markdown is served by the app at `GET /api/skills/{id}/SKILL.md` (`routes/skills.rs`), and the copy link is built from `window.location.origin`.

## Large views

Six views carry most of the frontend complexity; budget accordingly before touching them.

- `views/ProviderDetailPage.vue`: 1,683 lines, 9 modals, model-caps resolution.
- `views/usage/components/ProviderLimits/ProviderLimitsPanel.vue` + `utils.ts`: 1,377 + 590 lines.
- `views/ProxyPoolsPage.vue`: 1,186 lines.
- `views/BasicChatPage.vue`: 1,154 lines.
- `components/UsageStats.vue`: 551 lines, one SSE stream (`/api/usage/stream`), plus the `TimeAgo` component that confines its 1-second re-render to the timestamp span so per-event updates do not jank the page.
- `views/usage/components/ProviderTopology.vue`: 509 lines of graph layout.

## Client-side constants coupling

The provider registry is served once by `GET /api/registry` and installed into `useRegistryStore` at boot, before the router mounts. The payload is `providers` (five category maps: `free`, `freeTier`, `oauth`, `apikey`, `webCookie`), `mediaProviderKinds`, `usageSupportedProviders`, `usageApikeyProviders`, `providerModels` and `providerIdToAlias`. Every consumer reads those tables synchronously off the store: `src/constants/providers.ts` and `src/constants/models.ts` are thin accessors over it, and there is no compile-time copy client-side. The store is the single source of truth for provider lists, model lists and aliases — add a field to the registry response, not to a client constant.

Model capabilities (vision / reasoning / context window) come from a separate `GET /api/models` fetch shared by every `useModelCaps` instance (`src/hooks/useModelCaps.ts`). Custom models change at runtime, so that fetch drops its cache and refetches on the `customModelChanged` event.

`liveAppPort()` in `src/constants/config.ts` returns `window.location.port` and falls back to `UPDATER_CONFIG.appPort` (20129) only where there is no browser. Any code that builds a URL back into the running server must use it — `BaseUrlSelect.vue` and `cliEndpointPresets.ts` for the "local" endpoint preset, and the media combo curl snippet, which builds from `window.location.origin`.

## Branding and functional identifiers

User-visible strings read "RustRouter": the page title, the sidebar/login/landing wordmarks, the updater dialog and the CLI-tool copy. Functional identifiers stay on `9router`, because changing them breaks something real: the `~/.9router` data directory, the `sk_9router` default key, the `9router.cliTool*` localStorage keys, the `model_provider = "9router"` / `model_providers.9router` TOML keys written for Codex, and the served agent skills, whose ids (`9router`, `9router-chat`, `9router-embeddings`, `9router-web-search`, `9router-web-fetch`) and `/api/skills/9router*` URLs external clients fetch.

## i18n

**English-only.** There is no i18n layer, no locale switcher and no runtime string translator; UI copy is written inline. Date and number formatting uses `en-US` in the usage/quota and endpoint views rather than the browser locale.

## Auth boundary

Client-side route guards are cosmetic. The Rust backend owns the 401 and the redirect to `/login`; the Vue `beforeEach` guard in `src/router/index.ts` only mirrors that decision so the SPA does not flash a dashboard it cannot populate. A `/dashboard/*` navigation probes `GET /api/settings/require-login` (cached 30s) and then `GET /api/auth/status`. This is a security boundary, not a frontend detail.

## Static assets

`web/public/` holds `favicon.svg`, `icons/` (PWA icons), `fonts/` (self-hosted Geist and JetBrains Mono) and a `providers/` tree of 30 PNGs — one per shipped provider, plus the shared refs `claude`, `codex`, `hermes`, `ollama` and the compatibility badges `oai-r`, `oai-cc`, `anthropic-m`. Every shipped provider has an icon file, so no provider falls back to the initial badge.

`src/utils/providerIcon.ts` resolves `/providers/{id}.png` from a normalised provider id and remembers a 404 for the rest of the session, so a missing icon does not re-request on every mount. `ProviderIcon.vue` calls it and renders a text fallback when the file is absent.

## Serving

`vite build` → `dist/`, embedded into the Rust binary with `rust-embed` and served from the axum router on 20129 (`crates/router-server/src/static_assets.rs`). Route order: register `/api/*`, `/v1/*`, `/v1beta/*`, `/codex`, `/responses`, `/systemone` first, then a fallback serving the embedded asset by path with an `index.html` fallback for non-file paths, so client routes like `/dashboard/providers/abc` survive a hard refresh. Hashed assets under `assets/` are immutable, unhashed files get a day, and `index.html` is `no-cache`.

`web/dist` is ~5.6 MB. The Material Symbols variable woff2 (`dist/assets/material-symbols-outlined-*.woff2`) is 4.0 MB, about 68% of it. The rest is ~1.2 MB JS, ~129 KB CSS, ~296 KB provider icons, ~72 KB fonts. The icon font is the one lever left: a subsetted build (only the ligature glyphs the app names) would cut most of the payload, at the cost of a build step to regenerate it when an icon is added.

Use `vite dev` with `server.proxy` during development; the Rust rebuild loop is far slower than HMR.

## Material Symbols

Icons are font ligatures. They are hidden until the font loads via `opacity:0` plus a `.fonts-loaded` class toggled by an inline script in `index.html` that calls `document.fonts.load('24px "Material Symbols Outlined"')` with a 3s timeout. If the font fails, every icon in the app is invisible and the script has no error surface — keep the timeout and verify the font ships. `material-symbols/outlined.css` is imported from the JS entry (`main.ts`), not `@import`'d in CSS.

## Accessibility

`style.css` honours `prefers-reduced-motion`, and icon-only controls carry an `aria-label`. `UiModal` sets `role="dialog"` and `aria-modal`, names the dialog from its title, and restores focus to the element that was focused before it opened.

## OAuth callback relay

`views/CallbackPage.vue` posts OAuth codes to an allowlist (`window.location.origin` + `http://localhost:1455` for Codex) via `postMessage`, `BroadcastChannel 'oauth_callback'`, and a `localStorage` fallback, then auto-closes. If the backend's port or the Codex integration changes, this relay silently stops working with no error; the popup just closes. Re-verify the Codex callback end-to-end.
