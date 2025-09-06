# haystack

Tiny CLI to build and serve Markdown/Org files as responsive HTML.

## Install

- Prereq: Rust toolchain (stable). Install via https://rustup.rs
- From this repo:

```sh
cargo install --path .
```

## Usage

### Build site to `output/`:

```sh
haystack build [--theme-light NAME] [--theme-dark NAME]
```

- Scans `src/` for `*.md` and `*.org` (recursively).
- Writes corresponding `*.html` into `output/`, preserving subdirectories.

### Serve on-demand HTML from `src/`:

```sh
haystack serve --port 4000 [--theme-light NAME] [--theme-dark NAME]
```

- Request `/<path>.html` → serves `src/<path>.md` or `src/<path>.org` rendered to HTML.
- Request `/` → serves `src/index.md|org` as `index.html` if present.

## Features

- Markdown via `pulldown-cmark`
- Org via `orgize`
- Responsive, minimal built-in CSS with dark-mode support
- Built-in Share button to capture and share/download a screenshot of the page content
- Dynamic HTML `<title>` from first heading/`#+TITLE`
- Server-side code highlighting with `syntect` (no CDN)
- Theme selection via `--theme-light` / `--theme-dark`
- Static assets: copies non-`.md`/`.org` files from `src/` to `output/` during build, and serves them directly during `serve` with proper Content-Type.

## Configuration

- Choose highlighting themes from syntect's default set, e.g.:
  - Light: `InspiredGitHub`, `base16-ocean.light`, `Solarized (light)`
  - Dark: `base16-ocean.dark`, `Solarized (dark)`
- Example:

```sh
haystack serve --port 4000 --theme-light "InspiredGitHub" --theme-dark "Solarized (dark)"
```

### List available themes

```sh
haystack themes
```

Prints all theme names available in syntect’s default theme set.

### Custom head include

- If `theme/head.html` exists (relative to the working directory), its contents are injected into the `<head>` of every page (both build and serve). Useful for custom meta tags, analytics, fonts, or additional styles.

## Examples

- `src/blog/post.md` → `output/blog/post.html`
- GET `http://localhost:4000/blog/post.html` → renders `src/blog/post.md|org`
