#!/usr/bin/env sh
set -eu

usage() {
  cat <<'EOF'
Usage:
  upload-static-files-to-r2.sh [--remote] BUCKET [MANIFEST] [SOURCE_DIR]

Options:
  --remote   Upload to the real Cloudflare R2 bucket instead of local storage.

Arguments:
  BUCKET      Cloudflare R2 bucket name.
  MANIFEST    Newline-delimited static file list. Default: static-files.txt
  SOURCE_DIR  Directory containing source assets. Default: src

Example:
  scripts/upload-static-files-to-r2.sh --remote haystack-assets
  scripts/upload-static-files-to-r2.sh haystack-assets static-files.txt src

Each manifest line is used as both:
  - source path under SOURCE_DIR
  - R2 object key
  - content hash
EOF
}

if [ "${1:-}" = "-h" ] || [ "${1:-}" = "--help" ]; then
  usage
  exit 0
fi

remote_flag=""
if [ "${1:-}" = "--remote" ]; then
  remote_flag="--remote"
  shift
fi

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

  echo "upload: $file -> r2://$bucket/$key"
  wrangler r2 object put "$bucket/$key" --file "$file" $remote_flag
  uploaded=$((uploaded + 1))
done < "$manifest"

echo "uploaded: $uploaded"

if [ "$missing" -ne 0 ]; then
  echo "missing/skipped: $missing" >&2
  exit 1
fi
