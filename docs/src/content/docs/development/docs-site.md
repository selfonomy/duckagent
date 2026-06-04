---
title: Docs Site
description: Maintain the Astro + Starlight documentation site under docs/.
draft: false
---

The documentation site lives in `docs/` as an independent Astro + Starlight project.
The GitHub Pages URL for this repository is `https://selfonomy.github.io/duckagent/`.

## Development

```bash
cd docs
pnpm install
pnpm run dev
```

## Build

```bash
cd docs
pnpm run build
```

The build script runs `astro check` and then `astro build`. Starlight Pagefind integration is enabled, so production builds include static full-text search.

## CI and publishing

GitHub Actions runs the docs build as a separate `Docs` job in `.github/workflows/ci.yml`. The job uses Node.js 22, pnpm 8, the checked-in `pnpm-lock.yaml`, and the same build script documented above.

On pushes to `main`, the workflow uploads `docs/dist` as a GitHub Pages artifact and deploys it through the `Deploy Docs` job. The repository Pages source should be set to GitHub Actions. Astro is configured with `site: "https://selfonomy.github.io"` and `base: "/duckagent"` so generated links and assets match GitHub Pages project-site hosting.

Markdown pages may use root-absolute internal links such as `/start/`. The docs build rewrites those links to include the project base path, so they resolve correctly on GitHub Pages.

## Directory layout

```text
docs/
  astro.config.mjs
  package.json
  public/
  src/
    assets/
    content/docs/
    pages/index.astro
    styles/starlight.css
```

The homepage is `src/pages/index.astro`. Documentation content is under `src/content/docs/` and is exposed through the Starlight sidebar in `astro.config.mjs`.

The top header uses the main user-facing product sections: Start, Avatar & Identity, Capabilities, Gateway, Sandbox, and Reference. Starlight sidebars are section-scoped so each area stays focused after a user chooses a top-level section.

## Content rules

- Published docs and README are English-first.
- Keep user guides focused on what users can do and where files live.
- Keep reference pages precise about fields, defaults, commands, and policy behavior.
- Update docs in the same change as user-visible feature changes.
- Keep legacy Markdown files directly under `docs/` as migration pointers instead of separate sources of truth.

## Search

`pagefind: true` is enabled in the Starlight config. Pages under `src/content/docs/` are indexed after production build unless a page explicitly opts out.
