# haystack

Tiny CLI to build and serve Markdown/Org files as responsive HTML.

## Install

- Prereq: Rust toolchain (stable). Install via https://rustup.rs
- From this repo:

```sh
cargo install --path .
```

## Releases

Pushing a semantic version tag such as `v0.2.0` builds Linux x86_64, Linux
ARM64, and macOS ARM64 archives, generates SHA-256 checksums, and publishes
them to a GitHub Release.

To build the macOS ARM64 target locally:

```sh
rustup target add aarch64-apple-darwin
cargo build --locked --release --target aarch64-apple-darwin
```

## Usage

### Build site to `output/`:

```sh
haystack build [--theme-light NAME] [--theme-dark NAME] [--index] [--langs CODES] [--asset-prefix URL] [--static-file-list PATH]
```

- Scans `src/` for `*.md` and `*.org` (recursively).
- Writes corresponding `*.html` into `output/`, preserving subdirectories.
- Writes `output/static-files.txt` with one prefixed static asset reference per line.
- With `--index`, also writes `output/index.html` with links to all Markdown/Org files.
 - Multi-language (optional):
   - Provide `--langs en,zh,fr` (first is default, unprefixed).
   - Default language lives in `src/`; others in `src/<lang>/`.
   - Output mirrors this: `output/` (default) and `output/<lang>/` for others.
- With `--asset-prefix`, root-relative asset URLs in rendered Markdown/Org and copied
  HTML are rewritten to the prefix. Page links such as `/post.html` are left unchanged.
  This is useful when large assets are ignored in git and uploaded separately:

```sh
aws s3 sync ./src/audio s3://my-bucket/audio \
  --endpoint-url https://<ACCOUNT_ID>.r2.cloudflarestorage.com
haystack build --asset-prefix https://assets.example.com
```

Or upload exactly the referenced prefixed assets with Wrangler:

```sh
haystack build --asset-prefix https://assets.example.com
scripts/upload-static-files-to-r2.sh --remote my-bucket
```

### Serve on-demand HTML from `src/`:

```sh
haystack serve --port 4000 [--theme-light NAME] [--theme-dark NAME] [--langs CODES] [--allow-exec] [--asset-prefix URL]
```

- Request `/<path>.html` → serves `src/<path>.md` or `src/<path>.org` rendered to HTML (default language, unprefixed).
- Request `/` → serves a generated index for default language.
- With languages: `/<lang>/...` resolves within `src/<lang>/...` and `/<lang>/` serves that language’s index.
 - Pages include a language switcher when languages are configured.

### Executable Markdown code blocks

Pass `--allow-exec` in serve mode to add a **Run** button to supported fenced
code blocks. Execution is disabled by default and is never enabled in built
static pages.

Built-in fence languages are `sh`, `bash`, `python`, `py`, `js`,
`javascript`, `node`, and `codex`. Additional types, or overrides for built-in types,
are configured in `~/.haystack.toml`. Built-in runners are represented by the
same configuration schema and loaded as defaults before the user configuration:

The built-in `python` runner uses `uv run -`, so `uv` must be available on
`PATH`.

```toml
[code_blocks.codex]
command = "codex"
args = ["exec", "--json", "-"]
output_format = "codex"
```

`{code}` is replaced with the complete block text as one process argument. If
no argument contains `{code}`, as in the Codex example, Haystack sends the code
to the process on standard input. Commands are launched directly without a
shell.

`output_format` defaults to `"text"`. Set it to `"markdown"` to have Haystack
render the completed command output as HTML. The `"codex"` renderer parses
Codex JSONL output, renders agent messages as Markdown, and presents command
executions, errors, diagnostics, and token usage separately. Text output remains
streamed; rendered formats are displayed after the command completes.

Fence settings follow the language as whitespace-separated `key=value` pairs:

````markdown
```python cwd=examples env.MODE=development
print("hello")
```
````

- `cwd` is relative to the Markdown file and must remain inside `src/`.
- `env.NAME=value` adds an environment variable to the child process.
- The server re-reads the Markdown and selects the block by ID; code is not
  accepted from the browser.
- Output from stdout and stderr is streamed into the rendered page.

On the first run, Haystack adds a stable `id` to the executable fence. Output
is written back to a marked result fence after every completed run:

````markdown
```python id=b-7fa31c2d
print("hello")
```

<!-- haystack-result: b-7fa31c2d -->
```text haystack-result=b-7fa31c2d
hello
```
````

An existing result for the same ID is replaced. Result fences are not
executable, and Haystack expands their backtick fence when the output itself
contains backticks. Markdown and Codex result fences use `markdown` and
`codex` types and are rendered with their corresponding output renderers.

Enabling this option permits visitors who can access the server to run marked
code using the server process's OS permissions. Use it only on trusted content
and trusted networks.

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
