# Vendored Starbase

Rocket web components and design tokens from [Starbase](https://starbase.zweiundeins.gmbh/)
(<https://github.com/zweiundeins/starbase>, commit `46bc319`, 2026-09-29), MIT licensed
(`LICENSE`). Fonts: Pixelify Sans and JetBrains Mono, SIL Open Font License
(`fonts/OFL-*.txt`). Prism (used lazily by `code-editor`): MIT (`c/code-editor/vendor/`).

| Path | Source | Changes |
| --- | --- | --- |
| `c/<name>/<name>.min.js` | the site's pinned `c/<name>@<hash>/<name>.min.js` builds | none |
| `css/tokens.css` | `static/css/tokens.css` | font URLs made absolute (`/static/starbase/fonts/…`) |
| `css/theme.css` | `static/css/theme.css` | none |
| `css/daylight.css` | the `daylight` block of `static/css/themes/showcase.css` + the site's `theme/auto.css` | concatenated |
| `fonts/` | `static/fonts/` | none |

Components import only `datastar`, which the page's import map points at the vendored Datastar + Rocket
bundle (`/static/datastar.js?v=…`). To update: copy newer builds over these files and keep
this table current.
