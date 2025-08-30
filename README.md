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
haystack build
```

- Scans `src/` for `*.md` and `*.org` (recursively).
- Writes corresponding `*.html` into `output/`, preserving subdirectories.

### Serve on-demand HTML from `src/`:

```sh
haystack serve --port 4000
```

- Request `/<path>.html` → serves `src/<path>.md` or `src/<path>.org` rendered to HTML.
- Request `/` → serves `src/index.md|org` as `index.html` if present.

## Features

- Markdown via `pulldown-cmark`
- Org via `orgize`
- Responsive, minimal built-in CSS with dark-mode support.

## Examples

- `src/blog/post.md` → `output/blog/post.html`
- GET `http://localhost:4000/blog/post.html` → renders `src/blog/post.md|org`
