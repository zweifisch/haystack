#!/usr/bin/env sh
set -eu

usage() {
  cat <<'EOF'
Usage:
  upload-static-files-to-r2.sh [--remote] [--skip-existing] BUCKET [MANIFEST] [SOURCE_DIR]

Options:
  --remote         Upload to the real Cloudflare R2 bucket instead of local storage.
  --skip-existing  Skip upload when the content-hashed object key already exists.

Arguments:
  BUCKET      Cloudflare R2 bucket name.
  MANIFEST    Newline-delimited static file list. Default: static-files.txt
  SOURCE_DIR  Directory containing source assets. Default: src

Example:
  scripts/upload-static-files-to-r2.sh --remote --skip-existing haystack-assets
  scripts/upload-static-files-to-r2.sh haystack-assets static-files.txt src

Each manifest line contains:
  - source path under SOURCE_DIR
  - R2 object key
  - content hash

Content-Type is set from the source file extension (case-insensitive).
Unknown extensions use application/octet-stream. Omit --skip-existing to
update Content-Type on objects that have already been uploaded.
EOF
}

content_type_for() {
  extension=$(printf '%s' "${1##*.}" | LC_ALL=C tr '[:upper:]' '[:lower:]')
  case "$extension" in
    html|htm) printf '%s' 'text/html' ;;
    css) printf '%s' 'text/css' ;;
    js|mjs|cjs) printf '%s' 'text/javascript' ;;
    json|map) printf '%s' 'application/json' ;;
    xml) printf '%s' 'application/xml' ;;
    txt|md|org) printf '%s' 'text/plain' ;;
    csv) printf '%s' 'text/csv' ;;
    svg) printf '%s' 'image/svg+xml' ;;
    png) printf '%s' 'image/png' ;;
    jpg|jpeg) printf '%s' 'image/jpeg' ;;
    gif) printf '%s' 'image/gif' ;;
    webp) printf '%s' 'image/webp' ;;
    avif) printf '%s' 'image/avif' ;;
    ico) printf '%s' 'image/x-icon' ;;
    pdf) printf '%s' 'application/pdf' ;;
    woff) printf '%s' 'font/woff' ;;
    woff2) printf '%s' 'font/woff2' ;;
    ttf) printf '%s' 'font/ttf' ;;
    otf) printf '%s' 'font/otf' ;;
    mp3) printf '%s' 'audio/mpeg' ;;
    wav) printf '%s' 'audio/wav' ;;
    ogg|oga) printf '%s' 'audio/ogg' ;;
    m4a) printf '%s' 'audio/mp4' ;;
    mp4|m4v) printf '%s' 'video/mp4' ;;
    webm) printf '%s' 'video/webm' ;;
    ogv) printf '%s' 'video/ogg' ;;
    mov) printf '%s' 'video/quicktime' ;;
    wasm) printf '%s' 'application/wasm' ;;
    zip) printf '%s' 'application/zip' ;;
    gz) printf '%s' 'application/gzip' ;;
    *) printf '%s' 'application/octet-stream' ;;
  esac
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  usage
  exit 0
fi

remote_flag=""
skip_existing=0
while [ "${1:-}" = "--remote" ] || [ "${1:-}" = "--skip-existing" ]; do
  case "$1" in
    --remote)
      remote_flag="--remote"
      ;;
    --skip-existing)
      skip_existing=1
      ;;
  esac
  shift
done

bucket="${1:-}"
manifest="${2:-static-files.txt}"
source_dir="${3:-src}"

if [ -z "$bucket" ]; then
  usage >&2
  exit 2
fi

if ! command -v wrangler >/dev/null 2>&1; then
  echo "error: wrangler is not installed or not on PATH" >&2
  exit 1
fi

if [ ! -f "$manifest" ]; then
  echo "error: manifest not found: $manifest" >&2
  exit 1
fi

if [ ! -d "$source_dir" ]; then
  echo "error: source directory not found: $source_dir" >&2
  exit 1
fi

uploaded=0
skipped=0
missing=0

while IFS='	' read -r source key hash || [ -n "${source:-}" ]; do
  case "${source:-}" in
    ""|\#*) continue ;;
    /*|*..*)
      echo "skip unsafe manifest source: $source" >&2
      missing=$((missing + 1))
      continue
      ;;
  esac
  case "${key:-}" in
    ""|/*|*..*)
      echo "skip unsafe manifest key for $source: ${key:-}" >&2
      missing=$((missing + 1))
      continue
      ;;
  esac
  if [ -z "${hash:-}" ]; then
    echo "skip manifest row without hash: $source" >&2
    missing=$((missing + 1))
    continue
  fi

  file="$source_dir/$source"
  if [ ! -f "$file" ]; then
    echo "missing: $file" >&2
    missing=$((missing + 1))
    continue
  fi

  if [ "$skip_existing" -eq 1 ] && wrangler r2 object get "$bucket/$key" --pipe $remote_flag >/dev/null 2>&1; then
    echo "skip existing: r2://$bucket/$key"
    skipped=$((skipped + 1))
    continue
  fi

  echo "upload: $file -> r2://$bucket/$key"
  content_type=$(content_type_for "$source")
  wrangler r2 object put "$bucket/$key" --file "$file" --content-type "$content_type" $remote_flag
  uploaded=$((uploaded + 1))
done < "$manifest"

echo "uploaded: $uploaded"
echo "skipped existing: $skipped"

if [ "$missing" -ne 0 ]; then
  echo "missing/skipped: $missing" >&2
  exit 1
fi
