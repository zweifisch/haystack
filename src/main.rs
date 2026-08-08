use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::collections::BTreeMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser as MdParser, Tag, TagEnd};
use tiny_http::{Header, Method, Request, Response, Server, StatusCode};
use walkdir::WalkDir;
use orgize::Org;
use once_cell::sync::Lazy;
use regex::Regex;
use serde::Deserialize;
use syntect::html::{css_for_theme_with_class_style, ClassStyle, ClassedHTMLGenerator};
use syntect::highlighting::{Theme, ThemeSet};
use syntect::parsing::{SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

#[derive(Parser, Debug)]
#[command(name = "haystack", version, about = "Build and serve markdown/org to HTML")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Compile src/*.md and src/*.org to output/*.html
    Build {
        /// Light theme name for syntax highlighting (syntect)
        #[arg(long, value_name = "NAME")]
        theme_light: Option<String>,
        /// Dark theme name for syntax highlighting (syntect)
        #[arg(long, value_name = "NAME")]
        theme_dark: Option<String>,
        /// Also generate an index.html listing all docs
        #[arg(long, default_value_t = false)]
        index: bool,
        /// Languages (comma-separated). First is default (unprefixed). Example: "en,zh,fr"
        #[arg(long, value_name = "CODES")]
        langs: Option<String>,
        /// Prefix root-relative static asset URLs in rendered/copied HTML
        #[arg(long, value_name = "URL")]
        asset_prefix: Option<String>,
        /// Write a newline-delimited list of copied static files
        #[arg(long, value_name = "PATH", default_value = "static-files.txt")]
        static_file_list: PathBuf,
    },
    /// Serve on-demand HTML from src/*.md and src/*.org
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 4000)]
        port: u16,
        /// Light theme name for syntax highlighting (syntect)
        #[arg(long, value_name = "NAME")]
        theme_light: Option<String>,
        /// Dark theme name for syntax highlighting (syntect)
        #[arg(long, value_name = "NAME")]
        theme_dark: Option<String>,
        /// Languages (comma-separated). First is default (unprefixed). Example: "en,zh,fr"
        #[arg(long, value_name = "CODES")]
        langs: Option<String>,
        /// Allow supported Markdown code blocks to be executed from rendered pages
        #[arg(long, default_value_t = false)]
        allow_exec: bool,
        /// Prefix root-relative static asset URLs in rendered HTML
        #[arg(long, value_name = "URL")]
        asset_prefix: Option<String>,
    },
    /// List available syntax highlighting themes
    Themes,
}

#[derive(Debug, Clone, Default)]
struct ThemeConfig {
    light: Option<String>,
    dark: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct LangConfig {
    // Default language code; when None, no language routing/prefixing is applied.
    default: Option<String>,
    // Non-default languages (prefixed in routes): e.g., ["zh", "fr"].
    others: Vec<String>,
}

impl LangConfig {
    fn new(langs_csv: Option<String>) -> Self {
        let mut langs: Vec<String> = Vec::new();
        if let Some(csv) = langs_csv {
            for part in csv.split(',') {
                let code = part.trim();
                if !code.is_empty() {
                    // Dedup while preserving order
                    if !langs.iter().any(|s| s.eq_ignore_ascii_case(code)) {
                        langs.push(code.to_string());
                    }
                }
            }
        }
        if langs.is_empty() {
            LangConfig { default: None, others: Vec::new() }
        } else {
            let default = Some(langs[0].clone());
            let others = langs.into_iter().skip(1).collect();
            LangConfig { default, others }
        }
    }

    fn has_langs(&self) -> bool { self.default.is_some() || !self.others.is_empty() }

    fn all_langs(&self) -> Vec<String> {
        let mut v = Vec::new();
        if let Some(d) = &self.default { v.push(d.clone()); }
        v.extend(self.others.clone());
        v
    }
}

#[derive(Debug, Clone, Default)]
struct AssetConfig {
    prefix: Option<String>,
    source_dir: PathBuf,
    manifest: BTreeMap<String, StaticAsset>,
    generated: BTreeMap<String, StaticAsset>,
}

impl AssetConfig {
    fn new(prefix: Option<String>, source_dir: &Path, manifest_path: &Path) -> Result<Self> {
        let prefix = prefix
            .map(|value| value.trim().trim_end_matches('/').to_string())
            .filter(|value| !value.is_empty());
        let manifest = read_static_file_list(manifest_path)?;
        Ok(Self {
            prefix,
            source_dir: source_dir.to_path_buf(),
            manifest,
            generated: BTreeMap::new(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StaticAsset {
    source: String,
    key: String,
    hash: String,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { theme_light, theme_dark, index, langs, asset_prefix, static_file_list } => {
            let src = Path::new("src");
            let out = Path::new("output");
            let theme = ThemeConfig { light: theme_light, dark: theme_dark };
            let lang = LangConfig::new(langs);
            let mut assets = AssetConfig::new(asset_prefix, src, &static_file_list)?;
            build_all(src, out, &theme, &lang, index, &mut assets, &static_file_list)?;
        }
        Commands::Serve { port, theme_light, theme_dark, langs, allow_exec, asset_prefix } => {
            let src = Path::new("src");
            let theme = ThemeConfig { light: theme_light, dark: theme_dark };
            let lang = LangConfig::new(langs);
            let mut assets = AssetConfig::new(asset_prefix, src, Path::new("static-files.txt"))?;
            let execution = ExecutionConfig::load()?;
            serve(port, src, &theme, &lang, allow_exec, &execution, &mut assets)?;
        }
        Commands::Themes => {
            list_themes();
        }
    }

    Ok(())
}

fn build_all(src_dir: &Path, out_dir: &Path, theme: &ThemeConfig, lang: &LangConfig, generate_index: bool, assets: &mut AssetConfig, static_file_list: &Path) -> Result<()> {
    if !src_dir.exists() {
        return Err(anyhow!("src folder not found: {}", src_dir.display()));
    }
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;
    // Helper to process a single language subtree
    fn process_lang(
        base_src: &Path,
        base_out: &Path,
        theme: &ThemeConfig,
        rel_root: &Path,
        href_prefix: &str,
        generate_index: bool,
        cfg: &LangConfig,
        assets: &mut AssetConfig,
    ) -> Result<()> {
        let src_root = base_src.join(rel_root);
        let out_root = base_out.join(rel_root);
        fs::create_dir_all(&out_root)?;

        let mut doc_paths: Vec<std::path::PathBuf> = Vec::new();
        for entry in WalkDir::new(&src_root).into_iter().filter_map(|e| e.ok()) {
            let path = entry.path();
            if path.is_file() {
                match path.extension().and_then(|s| s.to_str()) {
                    Some("md") | Some("org") => {
                        let rel = path.strip_prefix(&src_root).unwrap();
                        doc_paths.push(rel.to_path_buf());
                        let mut out_path = out_root.join(rel);
                        out_path.set_extension("html");

                        if let Some(parent) = out_path.parent() { fs::create_dir_all(parent)?; }

                        // Build page with language switcher context
                        let html = convert_file_with_lang(path, theme, &build_page_ctx(&base_src, &rel_root, rel, cfg), assets)?;
                        fs::write(&out_path, html).with_context(|| format!("writing output file {}", out_path.display()))?;
                        println!("Built {} -> {}", path.display(), out_path.display());
                    }
                    Some("html") => {
                        let rel = path.strip_prefix(&src_root).unwrap();
                        doc_paths.push(rel.to_path_buf());
                        let out_path = out_root.join(rel);
                        if let Some(parent) = out_path.parent() { fs::create_dir_all(parent)?; }
                        if assets.prefix.is_some() {
                            let html = fs::read_to_string(path).with_context(|| format!("reading html {}", path.display()))?;
                            let page_dir = rel_root
                                .join(rel.parent().unwrap_or_else(|| Path::new("")))
                                .to_string_lossy()
                                .replace('\\', "/");
                            fs::write(&out_path, rewrite_asset_urls_collect(&html, assets, &page_dir)?).with_context(|| format!("writing html {}", out_path.display()))?;
                        } else {
                            fs::copy(path, &out_path).with_context(|| format!("copying html {} -> {}", path.display(), out_path.display()))?;
                        }
                        println!("Copied {} -> {}", path.display(), out_path.display());
                    }
                    _ => {
                        // Copy static
                        let rel = path.strip_prefix(&src_root).unwrap();
                        let out_path = out_root.join(rel);
                        if let Some(parent) = out_path.parent() { fs::create_dir_all(parent)?; }
                        fs::copy(path, &out_path).with_context(|| format!("copying static {} -> {}", path.display(), out_path.display()))?;
                        println!("Copied {} -> {}", path.display(), out_path.display());
                    }
                }
            }
        }
        if generate_index {
            doc_paths.sort_by(|a, b| a.to_string_lossy().to_ascii_lowercase().cmp(&b.to_string_lossy().to_ascii_lowercase()));
            let nav = build_lang_index_nav(cfg, href_prefix);
            let index_html = render_index_for_paths(&doc_paths, theme, "Index", href_prefix, nav.as_deref());
            let index_path = out_root.join("index.html");
            fs::write(&index_path, index_html).with_context(|| format!("writing index {}", index_path.display()))?;
            println!("Built index -> {}", index_path.display());
        }
        Ok(())
    }

    // Build default (unprefixed) subtree: skip language folders for non-default languages
    process_lang(src_dir, out_dir, theme, Path::new(""), "", generate_index, lang, assets)?;

    // Build each non-default language under its prefix if configured
    for lang_code in &lang.others {
        let rel_root = Path::new(lang_code);
        if src_dir.join(rel_root).exists() {
            process_lang(src_dir, out_dir, theme, rel_root, &format!("{}/", lang_code), generate_index, lang, assets)?;
        }
    }
    write_static_file_list(static_file_list, &assets.generated)?;
    Ok(())
}

fn serve(port: u16, src_dir: &Path, theme: &ThemeConfig, lang_cfg: &LangConfig, allow_exec: bool, execution: &ExecutionConfig, assets: &mut AssetConfig) -> Result<()> {
    if !src_dir.exists() {
        return Err(anyhow!("src folder not found: {}", src_dir.display()));
    }
    let addr = format!("0.0.0.0:{}", port);
    println!("Serving {} on http://{}/", src_dir.display(), addr);
    let server = Server::http(addr).map_err(|e| anyhow!("server error: {e}"))?;

    for request in server.incoming_requests() {
        let url_path = request.url().to_string(); // includes leading '/'
        let path = url_path.split('?').next().unwrap_or("").trim_start_matches('/');
        if path == "__haystack/run" {
            handle_run_request(request, &url_path, src_dir, allow_exec, execution);
            continue;
        }
        // Root route: default-language index
        if path.is_empty() {
            let docs = collect_docs_under(src_dir, None, &lang_cfg.others);
            let nav = build_lang_index_nav(lang_cfg, "");
            let html = render_index_for_paths(&docs, theme, "Index", "", nav.as_deref());
            let resp = Response::from_string(html)
                .with_status_code(200)
                .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap());
            let _ = request.respond(resp);
            continue;
        }

        // Language-prefixed index: /<lang>/
        if let Some(first) = path.split('/').next() {
            if lang_cfg.others.iter().any(|l| l == first) && (path == first || path == format!("{}/", first)) {
                let docs = collect_docs_under(&src_dir.join(first), None, &Vec::new());
                let nav = build_lang_index_nav(lang_cfg, &format!("{}/", first));
                let html = render_index_for_paths(&docs, theme, &format!("Index ({})", first), &format!("{}/", first), nav.as_deref());
                let resp = Response::from_string(html)
                    .with_status_code(200)
                    .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap());
                let _ = request.respond(resp);
                continue;
            }
        }

        // Basic path traversal guard
        if path.split('/').any(|seg| seg == ".." || seg.contains('\\')) {
            let resp = Response::from_string("Bad Request").with_status_code(400);
            let _ = request.respond(resp);
            continue;
        }

        // Determine language context
        let mut segments = path.split('/');
        let first_seg = segments.next().unwrap_or("");
        let (current_lang, tail) = if lang_cfg.others.iter().any(|l| l == first_seg) {
            (Some(first_seg.to_string()), segments.collect::<Vec<_>>().join("/"))
        } else {
            (None, path.to_string())
        };

        // Determine routing: HTML page (md/org) vs. static file
        let has_ext = std::path::Path::new(path).extension().is_some();
        let is_html_route = path.ends_with(".html") || !has_ext;

        let resp = if is_html_route {
            let base_in = if tail.ends_with(".html") { &tail[..tail.len() - ".html".len()] } else { tail.as_str() };
            let base_dir = match &current_lang { Some(l) => src_dir.join(l), None => src_dir.to_path_buf() };
            let html_path = base_dir.join(format!("{}.html", base_in));
            let md_path = base_dir.join(format!("{}.md", base_in));
            let org_path = base_dir.join(format!("{}.org", base_in));

            if html_path.exists() {
                match fs::read_to_string(&html_path) {
                    Ok(s) => match {
                        let page_dir = Path::new(path)
                            .parent()
                            .unwrap_or_else(|| Path::new(""))
                            .to_string_lossy()
                            .replace('\\', "/");
                        rewrite_asset_urls(&s, assets, &page_dir)
                    } {
                        Ok(html) => Response::from_string(html)
                            .with_status_code(200)
                            .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                        Err(error) => Response::from_string(error.to_string()).with_status_code(500),
                    },
                    Err(e) => Response::from_string(format!("Error reading {}: {}", html_path.display(), e))
                        .with_status_code(500),
                }
            } else if md_path.exists() {
                let mut ctx = build_runtime_page_ctx(src_dir, &current_lang, base_in, lang_cfg);
                if allow_exec {
                    let source = match &current_lang {
                        Some(lang) => format!("{}/{}.md", lang, base_in),
                        None => format!("{}.md", base_in),
                    };
                    let ctx = ctx.get_or_insert_with(PageLangCtx::default);
                    ctx.exec_source = Some(source);
                    ctx.exec_languages = execution.languages();
                }
                match fs::read_to_string(&md_path).and_then(|s| convert_markdown_to_html_with_ctx(&s, theme, ctx.as_ref(), assets).map_err(|error| std::io::Error::other(error.to_string()))) {
                    Ok(html) => Response::from_string(html)
                        .with_status_code(200)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                    Err(e) => Response::from_string(format!("Error reading {}: {}", md_path.display(), e))
                        .with_status_code(500),
                }
            } else if org_path.exists() {
                let ctx = build_runtime_page_ctx(src_dir, &current_lang, base_in, lang_cfg);
                match fs::read_to_string(&org_path).and_then(|s| convert_org_to_html_with_ctx(&s, theme, ctx.as_ref(), assets).map_err(|error| std::io::Error::other(error.to_string()))) {
                    Ok(html) => Response::from_string(html)
                        .with_status_code(200)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                    Err(e) => Response::from_string(format!("Error reading {}: {}", org_path.display(), e))
                        .with_status_code(500),
                }
            } else {
                Response::from_string("Not Found").with_status_code(404)
            }
        } else {
            // Serve static file from src/
            let static_path = match &current_lang { Some(l) => src_dir.join(l).join(tail), None => src_dir.join(path) };
            if static_path.is_file() {
                match fs::read(&static_path) {
                    Ok(bytes) => {
                        let mime = mime_guess::from_path(&static_path).first_or_octet_stream();
                        let mut resp = Response::from_data(bytes).with_status_code(200);
                        let header = Header::from_bytes(&b"Content-Type"[..], mime.to_string().as_bytes()).unwrap();
                        resp = resp.with_header(header);
                        resp
                    }
                    Err(e) => Response::from_string(format!("Error reading {}: {}", static_path.display(), e)).with_status_code(500),
                }
            } else {
                Response::from_string("Not Found").with_status_code(404)
            }
        };

        let _ = request.respond(resp);
    }

    Ok(())
}

#[derive(Debug)]
struct MarkdownCodeBlock {
    language: String,
    code: String,
    settings: BTreeMap<String, String>,
    source_start: usize,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OutputFormat {
    #[default]
    Text,
    Markdown,
    Codex,
}

#[derive(Debug, Clone, Deserialize)]
struct CodeBlockRunner {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    output_format: OutputFormat,
}

#[derive(Debug, Default, Deserialize)]
struct UserConfig {
    #[serde(default)]
    code_blocks: BTreeMap<String, CodeBlockRunner>,
}

#[derive(Debug)]
struct ExecutionConfig {
    runners: BTreeMap<String, CodeBlockRunner>,
}

const DEFAULT_EXECUTION_CONFIG: &str = r#"
[code_blocks.sh]
command = "sh"
args = ["-s"]

[code_blocks.bash]
command = "bash"
args = ["-s"]

[code_blocks.python]
command = "uv"
args = ["run", "-"]

[code_blocks.py]
command = "uv"
args = ["run", "-"]

[code_blocks.codex]
command = "codex"
args = ["exec", "--json", "-"]
output_format = "codex"

[code_blocks.js]
command = "node"
args = ["-"]

[code_blocks.javascript]
command = "node"
args = ["-"]

[code_blocks.node]
command = "node"
args = ["-"]
"#;

impl ExecutionConfig {
    fn load() -> Result<Self> {
        let defaults: UserConfig = toml::from_str(DEFAULT_EXECUTION_CONFIG)
            .context("parsing built-in execution configuration")?;
        let mut runners = defaults.code_blocks;

        let Some(home) = std::env::var_os("HOME") else {
            return Ok(Self { runners });
        };
        let path = Path::new(&home).join(".haystack.toml");
        if !path.exists() {
            return Ok(Self { runners });
        }
        let contents = fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let user: UserConfig = toml::from_str(&contents)
            .with_context(|| format!("parsing {}", path.display()))?;
        for (language, runner) in user.code_blocks {
            if language.trim().is_empty() || runner.command.trim().is_empty() {
                return Err(anyhow!(
                    "{} contains an empty code block type or command",
                    path.display()
                ));
            }
            runners.insert(language.to_ascii_lowercase(), runner);
        }
        Ok(Self { runners })
    }

    fn runner(&self, language: &str) -> Option<&CodeBlockRunner> {
        self.runners.get(&language.to_ascii_lowercase())
    }

    fn languages(&self) -> Vec<String> {
        self.runners.keys().cloned().collect()
    }
}

fn parse_fence_info(info: &str) -> (String, BTreeMap<String, String>) {
    let mut parts = info.split_whitespace();
    let language = parts.next().unwrap_or("").to_string();
    let mut settings = BTreeMap::new();
    for part in parts {
        if let Some((key, value)) = part.split_once('=') {
            settings.insert(key.to_string(), value.trim_matches(['"', '\'']).to_string());
        } else {
            settings.insert(part.to_string(), "true".to_string());
        }
    }
    (language, settings)
}

fn markdown_code_block(input: &str, wanted_id: &str) -> Option<MarkdownCodeBlock> {
    let parser = MdParser::new_ext(input, Options::all()).into_offset_iter();
    let mut current: Option<(String, String, usize)> = None;
    let mut matches = Vec::new();
    let mut occurrences: BTreeMap<String, usize> = BTreeMap::new();
    for (event, range) in parser {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                current = Some((info, String::new(), range.start));
            }
            Event::Text(text) if current.is_some() => {
                current.as_mut().unwrap().1.push_str(&text);
            }
            Event::End(TagEnd::CodeBlock) => {
                let (info, code, source_start) = current.take()?;
                let (language, settings) = parse_fence_info(&info);
                if settings.contains_key("haystack-result") {
                    continue;
                }
                let explicit_id = settings.get("id").map(String::as_str);
                let fingerprint = block_fingerprint(&language, &code);
                let occurrence = occurrences.entry(fingerprint.clone()).or_default();
                let temporary_id = format!("{fingerprint}-{}", *occurrence);
                *occurrence += 1;
                if explicit_id == Some(wanted_id) || temporary_id == wanted_id {
                    matches.push(MarkdownCodeBlock {
                        language,
                        code,
                        settings,
                        source_start,
                    });
                }
            }
            _ => {}
        }
    }
    if matches.len() == 1 { matches.pop() } else { None }
}

fn block_fingerprint(language: &str, code: &str) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in language.bytes().chain([0]).chain(code.bytes()) {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("temp-{hash:016x}")
}

static BLOCK_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

fn generate_block_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let counter = BLOCK_ID_COUNTER.fetch_add(1, Ordering::Relaxed);
    let mut value = nanos
        ^ u64::from(std::process::id()).rotate_left(17)
        ^ counter.wrapping_mul(0x9e3779b97f4a7c15);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58476d1ce4e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d049bb133111eb);
    value ^= value >> 31;
    format!("b-{:08x}", value as u32)
}

fn valid_block_id(id: &str) -> bool {
    !id.is_empty()
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'_')
}

fn ensure_block_id(path: &Path, input: &str, block: &MarkdownCodeBlock) -> Result<String> {
    if let Some(id) = block.settings.get("id") {
        if !valid_block_id(id) {
            return Err(anyhow!(
                "block id must contain only letters, numbers, '-' or '_'"
            ));
        }
        return Ok(id.clone());
    }
    let id = loop {
        let candidate = generate_block_id();
        if !input.contains(&format!("id={candidate}")) {
            break candidate;
        }
    };
    let line_end = input[block.source_start..]
        .find('\n')
        .map(|offset| block.source_start + offset)
        .unwrap_or(input.len());
    let insert_at = if line_end > 0 && input.as_bytes()[line_end - 1] == b'\r' {
        line_end - 1
    } else {
        line_end
    };
    let mut updated = String::with_capacity(input.len() + id.len() + 4);
    updated.push_str(&input[..insert_at]);
    updated.push_str(" id=");
    updated.push_str(&id);
    updated.push_str(&input[insert_at..]);
    atomic_write(path, &updated)?;
    Ok(id)
}

fn atomic_write(path: &Path, contents: &str) -> Result<()> {
    let file_name = path.file_name().and_then(|name| name.to_str()).unwrap_or("document");
    let temp = path.with_file_name(format!(".{file_name}.haystack.tmp"));
    fs::write(&temp, contents).with_context(|| format!("writing {}", temp.display()))?;
    if let Ok(metadata) = fs::metadata(path) {
        fs::set_permissions(&temp, metadata.permissions())
            .with_context(|| format!("preserving permissions for {}", path.display()))?;
    }
    fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))
}

#[derive(Clone)]
struct ResultSave {
    path: PathBuf,
    block_id: String,
    executed_code: String,
    output_format: OutputFormat,
}

fn fenced_block_end(input: &str, source_start: usize) -> Option<usize> {
    let line_start = input[..source_start].rfind('\n').map(|index| index + 1).unwrap_or(0);
    let opening_end = input[line_start..].find('\n').map(|offset| line_start + offset + 1)?;
    let opening = input[line_start..opening_end].trim_start();
    let marker = opening.as_bytes().first().copied()?;
    if marker != b'`' && marker != b'~' {
        return None;
    }
    let marker_len = opening.bytes().take_while(|byte| *byte == marker).count();
    if marker_len < 3 {
        return None;
    }
    let mut offset = opening_end;
    while offset < input.len() {
        let next = input[offset..]
            .find('\n')
            .map(|length| offset + length + 1)
            .unwrap_or(input.len());
        let line = input[offset..next].trim();
        let closing_len = line.bytes().take_while(|byte| *byte == marker).count();
        if closing_len >= marker_len && line[closing_len..].trim().is_empty() {
            return Some(next);
        }
        offset = next;
    }
    None
}

fn result_fence(output: &str) -> String {
    let mut longest = 0usize;
    let mut current = 0usize;
    for byte in output.bytes() {
        if byte == b'`' {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    "`".repeat(3.max(longest + 1))
}

fn save_result(save: &ResultSave, output: &[u8]) -> Result<()> {
    let output = String::from_utf8_lossy(output);
    let current = fs::read_to_string(&save.path)
        .with_context(|| format!("reading {}", save.path.display()))?;
    let block = markdown_code_block(&current, &save.block_id)
        .ok_or_else(|| anyhow!("code block {} no longer exists", save.block_id))?;
    if block.code != save.executed_code {
        return Err(anyhow!(
            "code block {} changed while it was running",
            save.block_id
        ));
    }
    let block_end = fenced_block_end(&current, block.source_start)
        .ok_or_else(|| anyhow!("cannot locate end of code block {}", save.block_id))?;
    let marker = format!("<!-- haystack-result: {} -->", save.block_id);
    let fence = result_fence(&output);
    let format = match save.output_format {
        OutputFormat::Text => "text",
        OutputFormat::Markdown => "markdown",
        OutputFormat::Codex => "codex",
    };
    let result = format!(
        "{marker}\n{fence}{format} haystack-result={}\n{}{newline}{fence}\n",
        save.block_id,
        output,
        newline = if output.ends_with('\n') { "" } else { "\n" },
    );

    let updated = if let Some(marker_start) = current.find(&marker) {
        let result_start = marker_start + marker.len();
        let following = current[result_start..]
            .find(|character: char| !character.is_whitespace())
            .map(|offset| result_start + offset)
            .ok_or_else(|| anyhow!("result marker {} has no fenced block", save.block_id))?;
        let result_end = fenced_block_end(&current, following)
            .ok_or_else(|| anyhow!("result marker {} is not followed by a fenced block", save.block_id))?;
        format!("{}{}{}", &current[..marker_start], result, &current[result_end..])
    } else {
        let separator = if block_end > 0 && current[..block_end].ends_with("\n\n") {
            ""
        } else if current[..block_end].ends_with('\n') {
            "\n"
        } else {
            "\n\n"
        };
        format!(
            "{}{}{}{}",
            &current[..block_end],
            separator,
            result,
            &current[block_end..],
        )
    };
    atomic_write(&save.path, &updated)
}

fn handle_run_request(request: Request, url: &str, src_dir: &Path, allow_exec: bool, execution: &ExecutionConfig) {
    if !allow_exec {
        let _ = request.respond(Response::from_string("Code execution is disabled").with_status_code(403));
        return;
    }
    if request.method() != &Method::Post {
        let _ = request.respond(Response::from_string("Method Not Allowed").with_status_code(405));
        return;
    }
    let requested_by_page = request.headers().iter().any(|header| {
        header.field.equiv("X-Haystack-Run") && header.value.as_str() == "1"
    });
    if !requested_by_page {
        let _ = request.respond(Response::from_string("Missing execution request header").with_status_code(403));
        return;
    }
    let query = url.split_once('?').map(|(_, q)| q).unwrap_or("");
    let params: BTreeMap<String, String> = query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .filter_map(|(key, value)| {
            Some((
                urlencoding::decode(key).ok()?.into_owned(),
                urlencoding::decode(value).ok()?.into_owned(),
            ))
        })
        .collect();
    let Some(source) = params.get("source") else {
        let _ = request.respond(Response::from_string("Missing source").with_status_code(400));
        return;
    };
    let Some(block_id) = params.get("id") else {
        let _ = request.respond(Response::from_string("Missing id").with_status_code(400));
        return;
    };
    if source.split('/').any(|part| part.is_empty() || part == ".." || part.contains('\\'))
        || !source.ends_with(".md")
    {
        let _ = request.respond(Response::from_string("Invalid source").with_status_code(400));
        return;
    }
    let source_path = src_dir.join(source);
    let canonical_root = match src_dir.canonicalize() {
        Ok(path) => path,
        Err(error) => {
            let _ = request.respond(Response::from_string(error.to_string()).with_status_code(500));
            return;
        }
    };
    let canonical_source = match source_path.canonicalize() {
        Ok(path) if path.starts_with(&canonical_root) => path,
        _ => {
            let _ = request.respond(Response::from_string("Source not found").with_status_code(404));
            return;
        }
    };
    let input = match fs::read_to_string(&canonical_source) {
        Ok(input) => input,
        Err(error) => {
            let _ = request.respond(Response::from_string(error.to_string()).with_status_code(500));
            return;
        }
    };
    let Some(block) = markdown_code_block(&input, block_id) else {
        let _ = request.respond(Response::from_string("Code block not found").with_status_code(404));
        return;
    };
    let Some(runner) = execution.runner(&block.language) else {
        let _ = request.respond(Response::from_string("Unsupported code block language").with_status_code(400));
        return;
    };
    let stable_id = match ensure_block_id(&canonical_source, &input, &block) {
        Ok(id) => id,
        Err(error) => {
            let _ = request.respond(
                Response::from_string(format!("Failed to assign block id: {error}"))
                    .with_status_code(500),
            );
            return;
        }
    };
    let code_argument = runner.args.iter().any(|arg| arg.contains("{code}"));
    let source_dir = canonical_source.parent().unwrap_or(&canonical_root);
    let working_dir = if let Some(relative) = block.settings.get("cwd") {
        match source_dir.join(relative).canonicalize() {
            Ok(path) if path.starts_with(&canonical_root) && path.is_dir() => path,
            _ => {
                let _ = request.respond(Response::from_string("Invalid cwd setting").with_status_code(400));
                return;
            }
        }
    } else {
        source_dir.to_path_buf()
    };
    let mut command = Command::new(&runner.command);
    command
        .args(runner.args.iter().map(|arg| arg.replace("{code}", &block.code)))
        .current_dir(working_dir)
        .stdin(if code_argument { Stdio::null() } else { Stdio::piped() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in &block.settings {
        if let Some(name) = key.strip_prefix("env.") {
            if !name.is_empty() {
                command.env(name, value);
            }
        }
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => {
            let _ = request.respond(Response::from_string(format!("Failed to start {}: {error}", runner.command)).with_status_code(500));
            return;
        }
    };
    if !code_argument {
        let mut stdin = child.stdin.take().unwrap();
        if let Err(error) = stdin.write_all(block.code.as_bytes()) {
            let _ = child.kill();
            let _ = request.respond(Response::from_string(format!("Failed to send code: {error}")).with_status_code(500));
            return;
        }
    }
    let stdout = child.stdout.take().unwrap();
    // Merge stderr into the response without buffering by forwarding it through a pipe is
    // not portable in std; runtimes used here generally report script errors on stderr.
    // A small forwarding thread keeps both streams live while the HTTP body is streamed.
    let stderr = child.stderr.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel::<OutputChunk>();
    let tx_out = tx.clone();
    std::thread::spawn(move || {
        let mut output = stdout;
        let mut buf = [0u8; 4096];
        while let Ok(n) = output.read(&mut buf) {
            if n == 0 { break; }
            if tx_out.send(OutputChunk::Stdout(buf[..n].to_vec())).is_err() { break; }
        }
    });
    std::thread::spawn(move || {
        let mut output = stderr;
        let mut buf = [0u8; 4096];
        while let Ok(n) = output.read(&mut buf) {
            if n == 0 { break; }
            if tx.send(OutputChunk::Stderr(buf[..n].to_vec())).is_err() { break; }
        }
    });
    let save = ResultSave {
        path: canonical_source.clone(),
        block_id: stable_id,
        executed_code: block.code.clone(),
        output_format: runner.output_format,
    };
    let stream_save = if matches!(runner.output_format, OutputFormat::Codex) {
        None
    } else {
        Some(save.clone())
    };
    let reader = ChannelExecutionOutput {
        child,
        receiver: rx,
        pending: Vec::new(),
        captured: Vec::new(),
        save: stream_save,
        finished: false,
    };
    match runner.output_format {
        OutputFormat::Text => {
            let response = Response::new(
                StatusCode(200),
                vec![Header::from_bytes(&b"Content-Type"[..], &b"text/plain; charset=utf-8"[..]).unwrap()],
                reader,
                None,
                None,
            );
            let _ = request.respond(response);
        }
        OutputFormat::Markdown => {
            let mut output = String::new();
            let mut reader = reader;
            match reader.read_to_string(&mut output) {
                Ok(_) => {
                    let response = Response::from_string(render_markdown_fragment(&output))
                        .with_status_code(200)
                        .with_header(Header::from_bytes(
                            &b"Content-Type"[..],
                            &b"text/html; charset=utf-8"[..],
                        ).unwrap());
                    let _ = request.respond(response);
                }
                Err(error) => {
                    let _ = request.respond(
                        Response::from_string(format!("Failed to read command output: {error}"))
                            .with_status_code(500),
                    );
                }
            }
        }
        OutputFormat::Codex => {
            let (stdout, stderr) = reader.collect_separated();
            let response = match save_result(&save, &stdout)
                .and_then(|_| render_codex_output(&stdout, &stderr))
            {
                Ok(html) => Response::from_string(html).with_status_code(200),
                Err(error) => Response::from_string(error.to_string()).with_status_code(500),
            }
            .with_header(Header::from_bytes(
                &b"Content-Type"[..],
                &b"text/html; charset=utf-8"[..],
            ).unwrap());
            let _ = request.respond(response);
        }
    }
}

enum OutputChunk {
    Stdout(Vec<u8>),
    Stderr(Vec<u8>),
}

struct ChannelExecutionOutput {
    child: std::process::Child,
    receiver: std::sync::mpsc::Receiver<OutputChunk>,
    pending: Vec<u8>,
    captured: Vec<u8>,
    save: Option<ResultSave>,
    finished: bool,
}

impl ChannelExecutionOutput {
    fn collect_separated(mut self) -> (Vec<u8>, Vec<u8>) {
        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        while let Ok(chunk) = self.receiver.recv() {
            match chunk {
                OutputChunk::Stdout(bytes) => stdout.extend(bytes),
                OutputChunk::Stderr(bytes) => stderr.extend(bytes),
            }
        }
        let _ = self.child.wait();
        (stdout, stderr)
    }

    fn finish(&mut self) -> std::io::Result<()> {
        if self.finished {
            return Ok(());
        }
        self.finished = true;
        self.child.wait()?;
        if let Some(save) = &self.save {
            save_result(save, &self.captured)
                .map_err(|error| std::io::Error::other(error.to_string()))?;
        }
        Ok(())
    }
}

impl Drop for ChannelExecutionOutput {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }
}

impl Read for ChannelExecutionOutput {
    fn read(&mut self, target: &mut [u8]) -> std::io::Result<usize> {
        while self.pending.is_empty() {
            match self.receiver.recv() {
                Ok(OutputChunk::Stdout(bytes)) | Ok(OutputChunk::Stderr(bytes)) => {
                    self.captured.extend_from_slice(&bytes);
                    self.pending = bytes;
                }
                Err(_) => {
                    self.finish()?;
                    return Ok(0);
                }
            }
        }
        let count = target.len().min(self.pending.len());
        target[..count].copy_from_slice(&self.pending[..count]);
        self.pending.drain(..count);
        Ok(count)
    }
}

fn executable_code_html(highlighted: &str, source: &str, block_id: &str, info: &str) -> String {
    format!(
        r#"<div class="executable-code" data-source="{}" data-block-id="{}"><div class="code-actions"><span>{}</span><button type="button" class="run-code">Run</button></div>{}<div class="code-output text-output" hidden aria-live="polite"></div></div>"#,
        escape_attr(source),
        escape_attr(block_id),
        escape_html(info),
        highlighted,
    )
}

fn is_graphviz_dot_language(lang: Option<&str>) -> bool {
    matches!(
        lang.map(|value| value.to_ascii_lowercase()),
        Some(value) if value == "dot" || value == "graphviz"
    )
}

fn graphviz_dot_html(source: &str) -> String {
    format!(
        r#"<div class="graphviz-dot" data-graphviz-pending="1"><template class="graphviz-dot-source">{}</template><div class="graphviz-status">Rendering diagram...</div></div>"#,
        escape_html(source),
    )
}

// removed legacy convert_file (replaced by convert_file_with_lang)

fn convert_file_with_lang(path: &Path, theme: &ThemeConfig, ctx: &PageLangCtx, assets: &mut AssetConfig) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening input file {}", path.display()))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("reading input file {}", path.display()))?;

    let html = match path.extension().and_then(|s| s.to_str()) {
        Some("md") => convert_markdown_to_html_with_ctx(&buf, theme, Some(ctx), assets)?,
        Some("org") => convert_org_to_html_with_ctx(&buf, theme, Some(ctx), assets)?,
        other => return Err(anyhow!("unsupported extension {:?} for {}", other, path.display())),
    };
    Ok(html)
}

// removed legacy convert_markdown_to_html (replaced by convert_markdown_to_html_with_ctx)

fn convert_markdown_to_html_with_ctx<'a>(input: &str, theme: &ThemeConfig, ctx: Option<&'a PageLangCtx>, assets: &mut AssetConfig) -> Result<String> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = MdParser::new_ext(input, options);

    // code highlighting same as above
    let mut events = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_info = String::new();
    let mut code_buf = String::new();
    let mut code_occurrences: BTreeMap<String, usize> = BTreeMap::new();
    for ev in parser {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        code_info = info.to_string();
                        let first = info.split_whitespace().next().unwrap_or("");
                        if first.is_empty() { None } else { Some(first.to_string()) }
                    }
                    CodeBlockKind::Indented => {
                        code_info.clear();
                        None
                    }
                };
            }
            Event::Text(t) if in_code => { code_buf.push_str(&t); }
            Event::End(TagEnd::CodeBlock) => {
                let (_, settings) = parse_fence_info(&code_info);
                let is_result = settings.contains_key("haystack-result");
                let mut html_snippet = if is_result {
                    match code_lang.as_deref() {
                        Some("markdown") => format!(
                            r#"<div class="saved-code-result">{}</div>"#,
                            render_markdown_fragment(&code_buf),
                        ),
                        Some("codex") => match render_codex_output(code_buf.as_bytes(), b"") {
                            Ok(rendered) => {
                                format!(r#"<div class="saved-code-result">{rendered}</div>"#)
                            }
                            Err(error) => format!(
                                r#"<div class="saved-code-result codex-error">{}</div>"#,
                                escape_html(&error.to_string()),
                            ),
                        },
                        _ => highlight_code(&code_buf, code_lang.as_deref()),
                    }
                } else if is_graphviz_dot_language(code_lang.as_deref()) {
                    graphviz_dot_html(&code_buf)
                } else {
                    highlight_code(&code_buf, code_lang.as_deref())
                };
                if let (Some(source), Some(lang)) =
                    (ctx.and_then(|c| c.exec_source.as_deref()), code_lang.as_deref())
                {
                    let is_configured = ctx
                        .map(|c| c.exec_languages.iter().any(|configured| configured.eq_ignore_ascii_case(lang)))
                        .unwrap_or(false);
                    if is_configured && !is_result {
                        let fingerprint = block_fingerprint(lang, &code_buf);
                        let occurrence = code_occurrences.entry(fingerprint.clone()).or_default();
                        let temporary_id = format!("{fingerprint}-{}", *occurrence);
                        *occurrence += 1;
                        let block_id = settings
                            .get("id")
                            .map(String::as_str)
                            .unwrap_or(&temporary_id);
                        html_snippet = executable_code_html(
                            &html_snippet,
                            source,
                            block_id,
                            &code_info,
                        );
                    }
                }
                events.push(Event::Html(CowStr::from(html_snippet)));
                in_code = false; code_lang = None;
            }
            other => { if !in_code { events.push(other); } }
        }
    }
    let mut body = String::new();
    html::push_html(&mut body, events.into_iter());
    // Optionally rewrite absolute internal links with language prefix
    if let Some(c) = ctx { if let Some(cur) = c.current_lang.as_ref() { if Some(cur) != c.default_lang.as_ref() {
        body = rewrite_internal_links(&body, &format!("/{}/", cur));
    }}}
    let page_dir = ctx.map(|c| c.page_dir.as_str()).unwrap_or("");
    body = rewrite_asset_urls_maybe_collect(&body, assets, page_dir)?;
    let title = extract_title_from_markdown(input);
    Ok(wrap_html_page_with_ctx(body, title, theme, ctx))
}

fn render_markdown_fragment(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = MdParser::new_ext(input, options);
    let mut events = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let language = info.split_whitespace().next().unwrap_or("");
                        if language.is_empty() { None } else { Some(language.to_string()) }
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Event::Text(text) if in_code => code_buf.push_str(&text),
            Event::End(TagEnd::CodeBlock) => {
                let html = if is_graphviz_dot_language(code_lang.as_deref()) {
                    graphviz_dot_html(&code_buf)
                } else {
                    highlight_code(&code_buf, code_lang.as_deref())
                };
                events.push(Event::Html(CowStr::from(html)));
                in_code = false;
                code_lang = None;
            }
            Event::Html(raw) | Event::InlineHtml(raw) if !in_code => {
                events.push(Event::Text(raw));
            }
            other if !in_code => events.push(other),
            _ => {}
        }
    }
    let mut fragment = String::new();
    html::push_html(&mut fragment, events.into_iter());
    fragment
}

fn render_codex_output(stdout: &[u8], stderr: &[u8]) -> Result<String> {
    let output = std::str::from_utf8(stdout).context("Codex returned non-UTF-8 JSONL")?;
    let mut rendered = String::from(r#"<div class="codex-output">"#);
    let mut event_count = 0usize;

    for (index, line) in output.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let event: serde_json::Value = serde_json::from_str(line)
            .with_context(|| format!("parsing Codex JSONL event on line {}", index + 1))?;
        let event_type = event.get("type").and_then(|value| value.as_str()).unwrap_or("");
        match event_type {
            "item.completed" => {
                let item = &event["item"];
                match item.get("type").and_then(|value| value.as_str()).unwrap_or("") {
                    "agent_message" => {
                        if let Some(text) = item.get("text").and_then(|value| value.as_str()) {
                            rendered.push_str(r#"<section class="codex-message">"#);
                            rendered.push_str(&render_markdown_fragment(text));
                            rendered.push_str("</section>");
                            event_count += 1;
                        }
                    }
                    "command_execution" => {
                        let command = item.get("command").and_then(|value| value.as_str()).unwrap_or("");
                        let status = item.get("status").and_then(|value| value.as_str()).unwrap_or("completed");
                        let command_output = item
                            .get("aggregated_output")
                            .and_then(|value| value.as_str())
                            .unwrap_or("");
                        rendered.push_str(&format!(
                            r#"<details class="codex-command codex-status-{}"><summary><code>{}</code> <span>{}</span></summary>"#,
                            escape_attr(status),
                            escape_html(command),
                            escape_html(status),
                        ));
                        if !command_output.is_empty() {
                            rendered.push_str(&format!("<pre>{}</pre>", escape_html(command_output)));
                        }
                        rendered.push_str("</details>");
                        event_count += 1;
                    }
                    "error" => {
                        let message = item.get("message").and_then(|value| value.as_str()).unwrap_or("Unknown Codex error");
                        rendered.push_str(&format!(
                            r#"<div class="codex-error">{}</div>"#,
                            escape_html(message),
                        ));
                        event_count += 1;
                    }
                    _ => {}
                }
            }
            "turn.failed" | "error" => {
                let message = event
                    .pointer("/error/message")
                    .or_else(|| event.get("message"))
                    .and_then(|value| value.as_str())
                    .unwrap_or("Codex execution failed");
                rendered.push_str(&format!(
                    r#"<div class="codex-error">{}</div>"#,
                    escape_html(message),
                ));
                event_count += 1;
            }
            "turn.completed" => {
                if let Some(usage) = event.get("usage") {
                    let input = usage.get("input_tokens").and_then(|value| value.as_u64()).unwrap_or(0);
                    let cached = usage
                        .get("cached_input_tokens")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0);
                    let uncached = input.saturating_sub(cached);
                    let output = usage.get("output_tokens").and_then(|value| value.as_u64()).unwrap_or(0);
                    rendered.push_str(&format!(
                        r#"<footer class="codex-usage">{} uncached input · {} cached input · {} output tokens</footer>"#,
                        uncached, cached, output,
                    ));
                }
            }
            _ => {}
        }
    }

    let stderr = String::from_utf8_lossy(stderr);
    if !stderr.trim().is_empty() {
        rendered.push_str(&format!(
            r#"<details class="codex-stderr"><summary>Codex diagnostics</summary><pre>{}</pre></details>"#,
            escape_html(stderr.trim()),
        ));
    }
    if event_count == 0 {
        return Err(anyhow!("Codex returned no renderable events"));
    }
    rendered.push_str("</div>");
    Ok(rendered)
}

// removed legacy convert_org_to_html (replaced by convert_org_to_html_with_ctx)

fn convert_org_to_html_with_ctx<'a>(input: &str, theme: &ThemeConfig, ctx: Option<&'a PageLangCtx>, assets: &mut AssetConfig) -> Result<String> {
    let org = Org::parse(input);
    let mut bytes: Vec<u8> = Vec::new();
    let _ = org.write_html(&mut bytes);
    let mut body = String::from_utf8(bytes).unwrap_or_default();
    let title = extract_title_from_org(input);
    body = highlight_code_blocks_in_html(&body);
    if let Some(c) = ctx { if let Some(cur) = c.current_lang.as_ref() { if Some(cur) != c.default_lang.as_ref() {
        body = rewrite_internal_links(&body, &format!("/{}/", cur));
    }}}
    let page_dir = ctx.map(|c| c.page_dir.as_str()).unwrap_or("");
    body = rewrite_asset_urls_maybe_collect(&body, assets, page_dir)?;
    Ok(wrap_html_page_with_ctx(body, title, theme, ctx))
}

fn rewrite_internal_links(body_html: &str, prefix: &str) -> String {
    // naive replacement of href="/..." and href='/...'
    let re_d = Regex::new(r#"href=\"/([^"]*)\""#).unwrap();
    let re_s = Regex::new(r#"href='/([^']*)'"#).unwrap();
    let tmp = re_d.replace_all(body_html, |caps: &regex::Captures| {
        format!("href=\"{}{}\"", prefix, &caps[1])
    });
    let tmp = re_s.replace_all(&tmp, |caps: &regex::Captures| {
        format!("href='{}{}'", prefix, &caps[1])
    });
    tmp.into_owned()
}

fn rewrite_asset_urls(body_html: &str, assets: &mut AssetConfig, page_dir: &str) -> Result<String> {
    rewrite_asset_urls_maybe_collect(body_html, assets, page_dir)
}

fn rewrite_asset_urls_collect(body_html: &str, assets: &mut AssetConfig, page_dir: &str) -> Result<String> {
    rewrite_asset_urls_maybe_collect(body_html, assets, page_dir)
}

fn rewrite_asset_urls_maybe_collect(body_html: &str, assets: &mut AssetConfig, page_dir: &str) -> Result<String> {
    let Some(prefix) = assets.prefix.as_deref() else {
        return Ok(body_html.to_string());
    };
    let prefix = prefix.to_string();

    static SRCSET_ATTR_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)\b(srcset)=(["'])([^"']*)["']"#).unwrap());
    static URL_ATTR_RE: Lazy<Regex> =
        Lazy::new(|| Regex::new(r#"(?is)\b(src|poster|href)=(["'])([^"']*)["']"#).unwrap());

    let mut rewritten = String::new();
    let mut last = 0;
    for caps in SRCSET_ATTR_RE.captures_iter(body_html) {
        let matched = caps.get(0).unwrap();
        rewritten.push_str(&body_html[last..matched.start()]);
        let attr = caps.get(1).map(|m| m.as_str()).unwrap_or("srcset");
        let quote = caps.get(2).map(|m| m.as_str()).unwrap_or("\"");
        let value = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        rewritten.push_str(&format!("{attr}={quote}{}{quote}", rewrite_srcset(value, &prefix, page_dir, assets)?));
        last = matched.end();
    }
    rewritten.push_str(&body_html[last..]);

    let with_srcset = rewritten;
    let mut rewritten = String::new();
    let mut last = 0;
    for caps in URL_ATTR_RE.captures_iter(&with_srcset) {
        let matched = caps.get(0).unwrap();
        rewritten.push_str(&with_srcset[last..matched.start()]);
        let attr = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let quote = caps.get(2).map(|m| m.as_str()).unwrap_or("\"");
        let value = caps.get(3).map(|m| m.as_str()).unwrap_or("");
        let rewritten_value = rewrite_asset_url_value(attr, value, &prefix, page_dir, assets)?
            .unwrap_or_else(|| value.to_string());
        rewritten.push_str(&format!("{attr}={quote}{rewritten_value}{quote}"));
        last = matched.end();
    }
    rewritten.push_str(&with_srcset[last..]);
    Ok(rewritten)
}

fn rewrite_srcset(value: &str, prefix: &str, page_dir: &str, assets: &mut AssetConfig) -> Result<String> {
    let mut rewritten_candidates = Vec::new();
    for candidate in value.split(',') {
        let leading = candidate.len() - candidate.trim_start().len();
        let trimmed_start = &candidate[leading..];
        let trailing = trimmed_start.len() - trimmed_start.trim_end().len();
        let core = &trimmed_start[..trimmed_start.len() - trailing];
        let mut parts = core.splitn(2, char::is_whitespace);
        let url = parts.next().unwrap_or("");
        let descriptor = parts.next().unwrap_or("");
        let rewritten = prefix_asset_url(url, prefix, page_dir, assets)?.unwrap_or_else(|| url.to_string());
        rewritten_candidates.push(format!(
            "{}{}{}{}{}",
            &candidate[..leading],
            rewritten,
            if descriptor.is_empty() { "" } else { " " },
            descriptor,
            &trimmed_start[trimmed_start.len() - trailing..],
        ));
    }
    Ok(rewritten_candidates.join(","))
}

fn rewrite_asset_url_value(attr: &str, value: &str, prefix: &str, page_dir: &str, assets: &mut AssetConfig) -> Result<Option<String>> {
    if attr.eq_ignore_ascii_case("href") && !is_asset_like_url(value) {
        return Ok(None);
    }
    prefix_asset_url(value, prefix, page_dir, assets)
}

fn prefix_asset_url(value: &str, prefix: &str, page_dir: &str, assets: &mut AssetConfig) -> Result<Option<String>> {
    if is_external_or_special_url(value) || value == "/" {
        return Ok(None);
    }
    let (path_part, suffix) = split_url_path_suffix(value);
    let site_path = if let Some(path) = path_part.strip_prefix('/') {
        normalize_site_path("", path)
    } else {
        normalize_site_path(page_dir, path_part)
    };
    let Some(site_path) = site_path else { return Ok(None); };
    if site_path.is_empty() {
        return Ok(None);
    }
    let Some(asset) = resolve_static_asset(assets, &site_path)? else {
        return Ok(None);
    };
    Ok(Some(format!("{prefix}/{}{}", asset.key, suffix)))
}

fn is_external_or_special_url(value: &str) -> bool {
    value.is_empty()
        || value.starts_with("//")
        || value.starts_with('#')
        || value.starts_with("data:")
        || value.starts_with("mailto:")
        || value.starts_with("tel:")
        || value.contains("://")
}

fn split_url_path_suffix(value: &str) -> (&str, &str) {
    match value.find(['?', '#']) {
        Some(index) => (&value[..index], &value[index..]),
        None => (value, ""),
    }
}

fn normalize_site_path(page_dir: &str, value: &str) -> Option<String> {
    let mut parts = Vec::new();
    for segment in page_dir.split('/').chain(value.split('/')) {
        match segment {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            segment => parts.push(segment),
        }
    }
    Some(parts.join("/"))
}

fn resolve_static_asset(assets: &mut AssetConfig, site_path: &str) -> Result<Option<StaticAsset>> {
    if let Some(asset) = assets.generated.get(site_path) {
        return Ok(Some(asset.clone()));
    }

    let source_path = assets.source_dir.join(site_path);
    let asset = if source_path.is_file() {
        let bytes = fs::read(&source_path)
            .with_context(|| format!("reading asset {}", source_path.display()))?;
        let hash = content_hash(&bytes);
        StaticAsset {
            source: site_path.to_string(),
            key: hashed_asset_key(site_path, &hash),
            hash,
        }
    } else if let Some(asset) = assets.manifest.get(site_path) {
        asset.clone()
    } else {
        eprintln!(
            "warning: asset {} is referenced but {} does not exist and no manifest entry was found; leaving URL unchanged",
            site_path,
            source_path.display(),
        );
        return Ok(None);
    };

    assets.generated.insert(site_path.to_string(), asset.clone());
    Ok(Some(asset))
}

fn content_hash(bytes: &[u8]) -> String {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}").chars().take(12).collect()
}

fn hashed_asset_key(site_path: &str, hash: &str) -> String {
    let path = Path::new(site_path);
    let Some(file_name) = path.file_name().and_then(|name| name.to_str()) else {
        return format!("{site_path}.{hash}");
    };
    let hashed_name = match file_name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => {
            format!("{stem}.{hash}.{ext}")
        }
        _ => format!("{file_name}.{hash}"),
    };
    match path.parent().and_then(|parent| parent.to_str()) {
        Some(parent) if !parent.is_empty() => format!("{}/{}", parent.replace('\\', "/"), hashed_name),
        _ => hashed_name,
    }
}

fn is_asset_like_url(value: &str) -> bool {
    if is_external_or_special_url(value) || value == "/" {
        return false;
    }
    let path = value.trim_start_matches('/');
    if path.is_empty() {
        return false;
    }
    let path = path.split(['?', '#']).next().unwrap_or("");
    let Some(ext) = Path::new(path).extension().and_then(|value| value.to_str()) else {
        return false;
    };
    !matches!(ext.to_ascii_lowercase().as_str(), "html" | "htm" | "md" | "org")
}

fn read_static_file_list(path: &Path) -> Result<BTreeMap<String, StaticAsset>> {
    let mut assets = BTreeMap::new();
    if !path.exists() {
        return Ok(assets);
    }

    let contents = fs::read_to_string(path)
        .with_context(|| format!("reading static file list {}", path.display()))?;
    for (index, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split('\t');
        let Some(source) = parts.next() else { continue; };
        let Some(key) = parts.next() else {
            return Err(anyhow!("{}:{} is missing an asset key", path.display(), index + 1));
        };
        let Some(hash) = parts.next() else {
            return Err(anyhow!("{}:{} is missing an asset hash", path.display(), index + 1));
        };
        if parts.next().is_some() {
            return Err(anyhow!("{}:{} has too many columns", path.display(), index + 1));
        }
        assets.insert(source.to_string(), StaticAsset {
            source: source.to_string(),
            key: key.to_string(),
            hash: hash.to_string(),
        });
    }
    Ok(assets)
}

fn write_static_file_list(path: &Path, files: &BTreeMap<String, StaticAsset>) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    }

    let mut contents = files
        .values()
        .map(|asset| format!("{}\t{}\t{}", asset.source, asset.key, asset.hash))
        .collect::<Vec<_>>()
        .join("\n");
    if !contents.is_empty() {
        contents.push('\n');
    }
    fs::write(path, contents).with_context(|| format!("writing static file list {}", path.display()))?;
    println!("Wrote static file list -> {}", path.display());
    Ok(())
}

fn render_index_for_paths(paths: &[std::path::PathBuf], theme: &ThemeConfig, title: &str, href_prefix: &str, lang_nav: Option<&str>) -> String {
    // Build a nested directory tree and render as grouped lists
    #[derive(Default)]
    struct DirNode {
        subdirs: BTreeMap<String, DirNode>,
        files: Vec<std::path::PathBuf>, // full relative paths
    }

    fn insert_path(node: &mut DirNode, head: &std::path::Path, full: &std::path::Path) {
        let mut it = head.iter();
        let first = match it.next() { Some(s) => s, None => return };
        let rest: std::path::PathBuf = it.collect();
        if rest.as_os_str().is_empty() {
            // At leaf; store the full relative path including directories
            node.files.push(full.to_path_buf());
        } else {
            let key = first.to_string_lossy().to_string();
            let entry = node.subdirs.entry(key).or_default();
            insert_path(entry, &rest, full);
        }
    }

    fn render_dir(name: Option<&str>, node: &DirNode, href_prefix: &str) -> String {
        let mut s = String::new();
        if let Some(n) = name {
            s.push_str(&format!("<li class=\"dir\"><strong>{}/</strong>\n<ul>\n", escape_html(n)));
        } else {
            s.push_str("<ul class=\"index-tree\">\n");
        }
        // Files first, sorted by case-insensitive path
        let mut files = node.files.clone();
        files.sort_by(|a, b| a.to_string_lossy().to_ascii_lowercase().cmp(&b.to_string_lossy().to_ascii_lowercase()));
        for rel in files {
            let html_rel = rel.with_extension("html");
            let href = html_rel.to_string_lossy().replace('\\', "/");
            let label = rel
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| rel.to_string_lossy().to_string());
            let data_name = label.to_ascii_lowercase();
            s.push_str(&format!(r#"  <li class="file" data-name="{}"><a href="/{}{}">{}</a></li>
"#, escape_html(&data_name), escape_attr(href_prefix), href, escape_html(&label)));
        }
        // Subdirectories
        for (subname, subnode) in &node.subdirs {
            s.push_str(&render_dir(Some(subname.as_str()), subnode, href_prefix));
        }
        s.push_str("</ul>\n");
        if name.is_some() { s.push_str("</li>\n"); }
        s
    }

    let mut root = DirNode::default();
    for rel in paths {
        insert_path(&mut root, rel, rel);
    }

    let mut body = String::new();
    body.push_str(&format!("<h1>{}</h1>\n", escape_html(title)));
    if let Some(nav) = lang_nav { body.push_str(nav); }
    body.push_str("<div class=\"index-search\"><input type=\"search\" id=\"idxSearch\" placeholder=\"Search...\" aria-label=\"Search documents\"></div>\n");
    body.push_str(&render_dir(None, &root, href_prefix));
    body.push_str(r#"
<script>(function(){
  var input = document.getElementById('idxSearch');
  var tree = document.querySelector('.index-tree');
  if(!input || !tree) return;
  function filter(){
    var q = (input.value || '').trim().toLowerCase();
    var files = tree.querySelectorAll('li.file');
    files.forEach(function(li){
      var name = li.getAttribute('data-name') || li.textContent || '';
      var hide = !!q && name.toLowerCase().indexOf(q) === -1;
      if(hide) li.classList.add('hidden'); else li.classList.remove('hidden');
    });
    var dirs = tree.querySelectorAll('li.dir');
    dirs.forEach(function(li){
      var hasVisible = li.querySelector('li.file:not(.hidden), li.dir:not(.hidden)');
      if(hasVisible) li.classList.remove('hidden'); else li.classList.add('hidden');
    });
  }
  input.addEventListener('input', filter);
  filter();
})();</script>
"#);
    wrap_html_page(body, Some(title.to_string()), theme)
}

fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[derive(Debug, Clone, Default)]
struct PageLangCtx {
    current_lang: Option<String>,
    default_lang: Option<String>,
    supported_langs: Vec<String>,
    page_tail_html: Option<String>,
    page_dir: String,
    available_langs: Vec<String>,
    exec_source: Option<String>,
    exec_languages: Vec<String>,
}

fn wrap_html_page(body: String, title: Option<String>, theme: &ThemeConfig) -> String {
    wrap_html_page_with_ctx(body, title, theme, None)
}

fn wrap_html_page_with_ctx(body: String, title: Option<String>, theme: &ThemeConfig, ctx: Option<&PageLangCtx>) -> String {
    let css = default_css();
    let (syn_css_light, syn_css_dark) = syntax_css(theme.light.as_deref(), theme.dark.as_deref());
    let page_title = title.as_deref().unwrap_or("haystack");
    let html_lang = ctx
        .and_then(|c| c.current_lang.clone().or_else(|| c.default_lang.clone()))
        .unwrap_or_else(|| "en".to_string());
    let theme_bootstrap = r#"(function(){
  try {
    // Ensure initial theme attribute BEFORE CSS
    document.documentElement.setAttribute('data-theme', localStorage.getItem('haystack-theme') || 'auto');
    var ua = navigator.userAgent || '';
    if (/micromessenger/i.test(ua)) {
      document.documentElement.setAttribute('data-hide-share', '1');
    }
    // MathJax v4: config and lazy loader
    window.haystackTypesetMath = function(root) {
      var content = root ? (root.textContent || '') : '';
      var maybeHasMath = /\\$[^\\$]+\\$|\\\\\(|\\\\\)|\\\\\[|\\\\\]|\\$\\$/m.test(content);
      if (!maybeHasMath) return;
      if (window.MathJax && typeof window.MathJax.typesetPromise === 'function') {
        window.MathJax.typesetPromise(root ? [root] : undefined).catch(function(error){
          console.error('MathJax typesetting failed', error);
        });
        return;
      }
      if (document.getElementById('MathJax-script')) return;
      window.MathJax = window.MathJax || {
        tex: {
          inlineMath: [['$', '$'], ['\\(', '\\)']],
          displayMath: [['$$','$$'], ['\\[','\\]']],
          processEscapes: true,
          processEnvironments: true
        },
        startup: {
          typeset: false,
          ready: function() {
            MathJax.startup.defaultReady();
            window.haystackTypesetMath(document.body);
          }
        },
        options: { skipHtmlTags: ['script','noscript','style','textarea','pre','code'] }
      };
      var mj = document.createElement('script');
      mj.id = 'MathJax-script';
      mj.async = true;
      mj.src = 'https://cdn.jsdelivr.net/npm/mathjax@4/es5/tex-chtml.js';
      document.head.appendChild(mj);
    };
    document.addEventListener('DOMContentLoaded', function(){
      window.haystackTypesetMath(document.body);
    });
  } catch(e) {}
})();"#;
    let share_script = r#"(function(){
  function loadHtml2Canvas(){
    return new Promise(function(resolve, reject){
      if(window.html2canvas){ resolve(window.html2canvas); return; }
      var s = document.createElement('script');
      s.src = 'https://unpkg.com/html2canvas@1.4.1/dist/html2canvas.min.js';
      s.onload = function(){ resolve(window.html2canvas); };
      s.onerror = function(){ reject(new Error('Failed to load html2canvas')); };
      document.head.appendChild(s);
    });
  }
  function filenameFromTitle(){
    var t = document.title || 'page';
    return t.toLowerCase().replace(/[^a-z0-9]+/g,'-').replace(/^-+|-+$/g,'') || 'page';
  }
  function notify(btn, msg){
    if(!btn) { try{ alert(msg); }catch(e){} return; }
    var orig = btn.textContent;
    btn.textContent = msg;
    btn.disabled = true;
    setTimeout(function(){ btn.textContent = orig; btn.disabled = false; }, 1400);
  }
  async function shareOrDownload(canvas, btn){
    return new Promise(function(resolve){ canvas.toBlob(async function(blob){
      if(!blob){ notify(btn,'Failed'); resolve(); return; }
      var file = new File([blob], filenameFromTitle()+'.png', { type: 'image/png' });
      try {
        if(navigator.canShare && navigator.canShare({ files: [file] }) && navigator.share){
          await navigator.share({ files: [file], title: document.title, text: window.location.href });
          notify(btn, 'Shared'); resolve(); return;
        }
      } catch(e){ /* ignore and fallback */ }
      try {
        if(navigator.clipboard && window.ClipboardItem){
          await navigator.clipboard.write([ new ClipboardItem({ 'image/png': blob }) ]);
          notify(btn, 'Copied'); resolve(); return;
        }
      } catch(e){ /* ignore and fallback */ }
      // Fallback: trigger download
      var a = document.createElement('a');
      a.href = URL.createObjectURL(blob);
      a.download = filenameFromTitle()+'.png';
      document.body.appendChild(a); a.click(); a.remove();
      setTimeout(function(){ URL.revokeObjectURL(a.href); }, 1500);
      notify(btn, 'Saved'); resolve();
    }, 'image/png'); });
  }
  async function onShare(){
    var btn = document.getElementById('shareBtn');
    if(!btn) return;
    btn.disabled = true; var prev = btn.textContent; btn.textContent = 'Rendering…';
    try{
      var h2c = await loadHtml2Canvas();
      var target = document.querySelector('main.container') || document.querySelector('.container') || document.body;
      var bg = getComputedStyle(document.body).backgroundColor || '#ffffff';
      var canvas = await h2c(target, { backgroundColor: bg, scale: Math.min(window.devicePixelRatio||1, 2) });
      await shareOrDownload(canvas, btn);
    } catch(e){
      console.error(e); try{ alert('Screenshot failed: '+(e && e.message ? e.message : e)); }catch(_){}
    } finally {
      btn.disabled = false; btn.textContent = prev;
    }
  }
  var btn = document.getElementById('shareBtn'); if(btn){ btn.addEventListener('click', onShare); }
})();"#;
    let controls_html = {
        let mut html = String::from(r#"<div class="theme-controls">"#);
        html.push_str(r#"<button id="shareBtn" aria-label="Share or save screenshot" title="Share or save screenshot">⇪ Share</button>"#);
        if let Some(c) = ctx {
            if !c.supported_langs.is_empty() {
                let tail = c.page_tail_html.clone().unwrap_or_default();
                let default_attr = c.default_lang.clone().unwrap_or_default();
                let cur = c.current_lang.as_deref().or(c.default_lang.as_deref());
                // Determine if there is any other available language besides the current
                let mut has_alt = false;
                for code in &c.available_langs {
                    if Some(code.as_str()) != cur { has_alt = true; break; }
                }
                let select_state = if has_alt { String::new() } else { String::from(" disabled aria-disabled=\"true\" title=\"No other languages available\"") };
                html.push_str(&format!(r#"<select id="langSelect" aria-label="Language" data-tail="{}" data-default="{}"{}>"#, escape_attr(&tail), escape_attr(&default_attr), select_state));
                for code in &c.supported_langs {
                    let mut attrs = String::new();
                    if Some(code.as_str()) == cur { attrs.push_str(" selected"); }
                    let available = c.available_langs.iter().any(|x| x == code);
                    if !available { attrs.push_str(" disabled"); }
                    html.push_str(&format!(r#"<option value="{}"{}>{}</option>"#, escape_attr(code), attrs, escape_html(code)));
                }
                html.push_str("</select>");
            }
        }
        html.push_str(r#"<button id="themeToggle" aria-label="Toggle theme">🌓</button>"#);
        html.push_str("</div>");
        html
    };
    let toggle_script = r#"(function(){
  function setTheme(t){ document.documentElement.setAttribute('data-theme', t); try{ localStorage.setItem('haystack-theme', t); }catch(e){} }
  const btn = document.getElementById('themeToggle');
  if(btn){ btn.addEventListener('click', function(){
    const cur = document.documentElement.getAttribute('data-theme')||'auto';
    const next = (cur==='light') ? 'dark' : (cur==='dark' ? 'auto' : 'light');
    setTheme(next);
    try { if(window.haystackTypesetMath){ setTimeout(function(){ window.haystackTypesetMath(document.body); }, 0); } } catch(e){}
  }); }
})();"#;
    // Prepare syntect CSS for light/dark and auto (media-driven)
    let syn_light_scoped = scope_syntect_css(&syn_css_light, r#"html[data-theme='light']"#);
    let syn_dark_scoped = scope_syntect_css(&syn_css_dark, r#"html[data-theme='dark']"#);
    let syn_auto_light = format!("@media (prefers-color-scheme: light) {{\n{}\n}}", scope_syntect_css(&syn_css_light, r#"html[data-theme='auto']"#));
    let syn_auto_dark = format!("@media (prefers-color-scheme: dark) {{\n{}\n}}", scope_syntect_css(&syn_css_dark, r#"html[data-theme='auto']"#));

    let wrap_overrides = "\n/* Force code wrapping */\n.container pre, .container pre code, .container code.hl, .container pre .hl {\n  white-space: pre-wrap;\n  overflow-wrap: anywhere;\n  word-break: break-word;\n}\n/* Controls spacing */\n.theme-controls > * + * { margin-left: 8px; }\n/* Hide share button for WeChat in-app browser */\nhtml[data-hide-share='1'] #shareBtn { display: none !important; }\n";
    let fonts_head = r#"
<link rel="preconnect" href="https://fonts.googleapis.com" crossorigin>
<link rel="preconnect" href="https://fonts.gstatic.com" crossorigin>
<link href="https://fonts.googleapis.com/css2?family=Source+Sans+3:ital,wght@0,200..900;1,200..900&display=swap" rel="stylesheet">
<link href="https://fonts.googleapis.com/css2?family=Source+Serif+4:ital,wght@0,200..900;1,200..900&display=swap" rel="stylesheet">
"#;
    let head_extra = read_head_snippet().unwrap_or_default();
    let indicator_script = r#"(function(){
  function render(){
    var btn = document.getElementById('themeToggle'); if(!btn) return;
    var mode = document.documentElement.getAttribute('data-theme')||'auto';
    btn.setAttribute('data-mode', mode);
    var label = (mode==='light'?'Light':(mode==='dark'?'Dark':'Auto'));
    btn.setAttribute('aria-label', 'Toggle theme (current: '+label+')');
    btn.title = 'Theme: '+label+' (click to switch)';
    btn.textContent = (mode==='light'?'\u2600':(mode==='dark'?'\u263D':'A'));
  }
  render();
  var btn = document.getElementById('themeToggle'); if(btn){ btn.addEventListener('click', function(){ setTimeout(render,0); }); }
  var obs = new MutationObserver(render); obs.observe(document.documentElement, { attributes:true, attributeFilter:['data-theme']});
})();"#;
    let lang_switch_script = r#"(function(){
  var sel = document.getElementById('langSelect');
  if(!sel) return;
  sel.addEventListener('change', function(){
    var lang = sel.value || '';
    var tail = sel.getAttribute('data-tail') || '';
    var def = sel.getAttribute('data-default') || '';
    var url = '/';
    if(lang && def && lang !== def) { url += lang + '/'; }
    if(tail) { url += tail; }
    // Navigate
    window.location.href = url;
  });
})();"#;
    let graphviz_script = r#"(function(){
  var graphvizPromise = null;
  function loadGraphviz(){
    if(graphvizPromise) return graphvizPromise;
    graphvizPromise = import('https://cdn.jsdelivr.net/npm/@hpcc-js/wasm-graphviz@1/dist/index.min.js')
      .then(function(module){ return module.Graphviz.load(); });
    return graphvizPromise;
  }
  function sourceFor(block){
    var source = block.querySelector('.graphviz-dot-source');
    if(!source) return '';
    if(source.content) return source.content.textContent || '';
    return source.textContent || '';
  }
  async function renderBlock(block){
    if(block.getAttribute('data-graphviz-rendered') === '1') return;
    block.setAttribute('data-graphviz-rendered', '1');
    try {
      var dot = sourceFor(block);
      var graphviz = await loadGraphviz();
      var svg = graphviz.dot(dot);
      block.removeAttribute('data-graphviz-pending');
      block.innerHTML = '<div class="graphviz-rendered">' + svg + '</div>';
    } catch(error) {
      block.removeAttribute('data-graphviz-pending');
      block.classList.add('graphviz-error');
      block.innerHTML = '<pre>' + String(error && error.message ? error.message : error)
        .replace(/[&<>]/g, function(ch){ return ({'&':'&amp;','<':'&lt;','>':'&gt;'})[ch]; }) + '</pre>';
    }
  }
  function renderAll(root){
    var blocks = (root || document).querySelectorAll('.graphviz-dot');
    if(!blocks.length) return;
    blocks.forEach(function(block){ renderBlock(block); });
  }
  window.haystackRenderGraphviz = renderAll;
  if(document.readyState === 'loading') {
    document.addEventListener('DOMContentLoaded', function(){ renderAll(document); });
  } else {
    renderAll(document);
  }
})();"#;
    let execution_script = r#"(function(){
  document.addEventListener('click', async function(event){
    var button = event.target.closest('.run-code');
    if(!button) return;
    var box = button.closest('.executable-code');
    var output = box.querySelector('.code-output');
    var source = box.getAttribute('data-source');
    var id = box.getAttribute('data-block-id');
    button.disabled = true;
    button.textContent = 'Running…';
    output.hidden = false;
    output.textContent = '';
    output.classList.remove('markdown-output');
    output.classList.add('text-output');
    try {
      var url = '/__haystack/run?source=' + encodeURIComponent(source) + '&id=' + encodeURIComponent(id);
      var response = await fetch(url, {
        method: 'POST',
        headers: {'X-Haystack-Run': '1'}
      });
      if(!response.ok) throw new Error((await response.text()) || ('HTTP ' + response.status));
      if((response.headers.get('content-type') || '').indexOf('text/html') !== -1) {
        output.classList.remove('text-output');
        output.classList.add('markdown-output');
        output.innerHTML = await response.text();
        if(window.haystackTypesetMath) window.haystackTypesetMath(output);
        if(window.haystackRenderGraphviz) window.haystackRenderGraphviz(output);
        return;
      }
      if(!response.body) { output.textContent = await response.text(); return; }
      var reader = response.body.getReader();
      var decoder = new TextDecoder();
      while(true) {
        var chunk = await reader.read();
        if(chunk.done) break;
        output.textContent += decoder.decode(chunk.value, {stream:true});
      }
      output.textContent += decoder.decode();
    } catch(error) {
      output.textContent += 'Error: ' + (error.message || error);
    } finally {
      button.disabled = false;
      button.textContent = 'Run';
    }
  });
})();"#;
    format!(
        "<!DOCTYPE html>\n<html lang=\"{}\" data-theme=\"auto\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n{}\n<script>{}</script>\n<style>\n{}\n{}\n{}\n{}\n{}\n{}\n</style>\n{}\n</head>\n<body>\n{}\n<main class=\"container\">\n{}\n</main>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n</body>\n</html>",
        escape_attr(&html_lang), page_title, fonts_head, theme_bootstrap, css, syn_light_scoped, syn_dark_scoped, syn_auto_light, syn_auto_dark, wrap_overrides, head_extra, controls_html, body, toggle_script, indicator_script, share_script, lang_switch_script, graphviz_script, execution_script
    )
}

fn read_head_snippet() -> Option<String> {
    let path = Path::new("theme").join("head.html");
    match fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(_) => None,
    }
}

fn escape_attr(s: &str) -> String {
    s.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;")
}

fn collect_docs_under(root: &Path, _ignore: Option<()>, exclude_first_level: &Vec<String>) -> Vec<std::path::PathBuf> {
    let mut docs = Vec::new();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        let p = entry.path();
        if p.is_file() {
            if let Some(ext) = p.extension().and_then(|s| s.to_str()) {
                if ext == "md" || ext == "org" || ext == "html" {
                    if let Ok(rel) = p.strip_prefix(root) {
                        // Exclude top-level language directories when scanning default root
                        if let Some(first) = rel.iter().next().and_then(|s| s.to_str()) {
                            if exclude_first_level.iter().any(|x| x == first) { continue; }
                        }
                        docs.push(rel.to_path_buf());
                    }
                }
            }
        }
    }
    docs.sort_by(|a,b| a.to_string_lossy().to_ascii_lowercase().cmp(&b.to_string_lossy().to_ascii_lowercase()));
    docs
}

fn page_available_langs(src_dir: &Path, base_tail: &str, cfg: &LangConfig) -> Vec<String> {
    let mut v = Vec::new();
    // default
    let def_exists = src_dir.join(format!("{}.md", base_tail)).exists() || src_dir.join(format!("{}.org", base_tail)).exists();
    if def_exists {
        if let Some(d) = &cfg.default { v.push(d.clone()); }
    }
    for l in &cfg.others {
        let base = src_dir.join(l);
        if base.join(format!("{}.md", base_tail)).exists() || base.join(format!("{}.org", base_tail)).exists() {
            v.push(l.clone());
        }
    }
    v
}

fn build_page_ctx(base_src: &Path, rel_root: &Path, rel_file: &Path, cfg: &LangConfig) -> PageLangCtx {
    let current = if rel_root.as_os_str().is_empty() {
        cfg.default.clone()
    } else {
        rel_root.iter().next().and_then(|s| s.to_str()).map(|s| s.to_string())
    };
    let tail_html = rel_file.with_extension("html").to_string_lossy().replace('\\', "/");
    let tail_no_ext = rel_file.with_extension("").to_string_lossy().replace('\\', "/");
    let page_dir = rel_root
        .join(rel_file.parent().unwrap_or_else(|| Path::new("")))
        .to_string_lossy()
        .replace('\\', "/");
    let available = page_available_langs(base_src, &tail_no_ext, cfg);
    PageLangCtx {
        current_lang: current,
        default_lang: cfg.default.clone(),
        supported_langs: cfg.all_langs(),
        page_tail_html: Some(tail_html),
        page_dir,
        available_langs: available,
        exec_source: None,
        exec_languages: Vec::new(),
    }
}

fn build_runtime_page_ctx(src_dir: &Path, current_lang: &Option<String>, base_tail_no_ext: &str, cfg: &LangConfig) -> Option<PageLangCtx> {
    if !cfg.has_langs() { return None; }
    let available = page_available_langs(src_dir, base_tail_no_ext, cfg);
    let tail_html = format!("{}.html", base_tail_no_ext);
    let page_dir = Path::new(&tail_html)
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_string_lossy()
        .replace('\\', "/");
    Some(PageLangCtx {
        current_lang: current_lang.clone().or_else(|| cfg.default.clone()),
        default_lang: cfg.default.clone(),
        supported_langs: cfg.all_langs(),
        page_tail_html: Some(tail_html),
        page_dir,
        available_langs: available,
        exec_source: None,
        exec_languages: Vec::new(),
    })
}

fn build_lang_index_nav(cfg: &LangConfig, current_prefix: &str) -> Option<String> {
    if !cfg.has_langs() { return None; }
    let mut html = String::new();
    html.push_str(r#"<nav class="lang-index-nav" aria-label="Languages">"#);
    let mut first = true;
    if let Some(d) = &cfg.default {
        let active = if current_prefix.is_empty() { " class=\"active\"" } else { "" };
        let name = lang_display_name(d);
        html.push_str(&format!(r#"<a href="/"{}>{}</a>"#, active, escape_html(&name)));
        first = false;
    }
    for l in &cfg.others {
        if !first { html.push_str(" · "); }
        let pref = format!("{}/", l);
        let active = if current_prefix == pref { " class=\"active\"" } else { "" };
        let name = lang_display_name(l);
        html.push_str(&format!(r#"<a href="/{}"{}>{}</a>"#, pref, active, escape_html(&name)));
        first = false;
    }
    html.push_str("</nav>\n");
    Some(html)
}

fn lang_display_name(code: &str) -> String {
    let c = code.trim();
    let lc = c.to_ascii_lowercase();
    let name = match lc.as_str() {
        // Common languages
        "en" => "English",
        "zh" | "zh-cn" | "zh-hans" => "简体中文",
        "zh-tw" | "zh-hant" => "繁體中文",
        "fr" => "Français",
        "de" => "Deutsch",
        "es" => "Español",
        "ja" => "日本語",
        "ko" => "한국어",
        "ru" => "Русский",
        "pt" => "Português",
        "pt-br" => "Português (Brasil)",
        "it" => "Italiano",
        "ar" => "العربية",
        "hi" => "हिन्दी",
        "tr" => "Türkçe",
        "vi" => "Tiếng Việt",
        "id" | "in" => "Bahasa Indonesia",
        "th" => "ไทย",
        "nl" => "Nederlands",
        "sv" => "Svenska",
        "pl" => "Polski",
        "uk" => "Українська",
        "fa" | "prs" | "pes" => "فارسی",
        "he" | "iw" => "עברית",
        "cs" => "Čeština",
        "el" => "Ελληνικά",
        "ro" => "Română",
        "hu" => "Magyar",
        "sk" => "Slovenčina",
        "sl" => "Slovenščina",
        "fi" => "Suomi",
        "no" | "nb" | "nn" => "Norsk",
        "da" => "Dansk",
        "bg" => "Български",
        "hr" => "Hrvatski",
        "sr" => "Српски",
        "et" => "Eesti",
        "lv" => "Latviešu",
        "lt" => "Lietuvių",
        "ms" => "Bahasa Melayu",
        "fil" | "tl" => "Filipino",
        "ur" => "اردو",
        "bn" => "বাংলা",
        "ta" => "தமிழ்",
        "te" => "తెలుగు",
        "ml" => "മലയാളം",
        "mr" => "मराठी",
        "gu" => "ગુજરાતી",
        "pa" => "ਪੰਜਾਬੀ",
        "kn" => "ಕನ್ನಡ",
        "si" => "සිංහල",
        "km" => "ភាសាខ្មែរ",
        "my" => "မြန်မာ",
        "am" => "አማርኛ",
        "sw" => "Kiswahili",
        "af" => "Afrikaans",
        "is" => "Íslenska",
        "ga" => "Gaeilge",
        _ => c,
    };
    name.to_string()
}

fn extract_title_from_markdown(input: &str) -> Option<String> {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = MdParser::new_ext(input, options);
    let mut in_heading = false;
    let mut title = String::new();
    for ev in parser {
        match ev {
            Event::Start(Tag::Heading { .. }) => {
                in_heading = true;
            }
            Event::End(TagEnd::Heading(..)) => {
                if !title.trim().is_empty() {
                    return Some(title.trim().to_string());
                } else {
                    in_heading = false;
                }
            }
            Event::Text(t) | Event::Code(t) if in_heading => {
                if !title.is_empty() {
                    title.push(' ');
                }
                title.push_str(&t);
            }
            _ => {}
        }
    }
    None
}

fn extract_title_from_org(input: &str) -> Option<String> {
    for line in input.lines() {
        let l = line.trim();
        if l.is_empty() { continue; }
        // #+TITLE: My Title (case-insensitive)
        if let Some(rest) = l.strip_prefix("#+") {
            let mut parts = rest.splitn(2, ':');
            if let (Some(key), Some(val)) = (parts.next(), parts.next()) {
                if key.eq_ignore_ascii_case("title") {
                    let v = val.trim();
                    if !v.is_empty() { return Some(v.to_string()); }
                }
            }
        }
        // First headline: * Heading
        if let Some(stripped) = l.strip_prefix('*') {
            // count additional stars then require a space
            let mut i = 0;
            for ch in stripped.chars() { if ch == '*' { i += 1; } else { break; } }
            let after = &stripped[i..];
            if let Some(title) = after.strip_prefix(' ') {
                let t = title.trim();
                if !t.is_empty() { return Some(t.to_string()); }
            }
        }
    }
    None
}

fn default_css() -> &'static str {
    r#":root {
  --fg: #222222;
  --bg: #f7f4e9; /* retro paper */
  --muted: #6b665e;
  --link: #2f6f6f; /* teal-ish retro */
  --border: #d9d4c7;
  --code-bg: #efe9d6;
  --shadow: rgba(0,0,0,0.04);
}
[data-theme='dark'] {
  --fg: #e6e1cf;
  --bg: #0e0f13;
  --muted: #9a968a;
  --link: #7fd1b9;
  --border: #2a2c33;
  --code-bg: #151821;
  --shadow: rgba(0,0,0,0.25);
}
@media (prefers-color-scheme: dark) {
  [data-theme='auto'] {
    --fg: #e6e1cf;
    --bg: #0e0f13;
    --muted: #9a968a;
    --link: #7fd1b9;
    --border: #2a2c33;
    --code-bg: #151821;
    --shadow: rgba(0,0,0,0.25);
  }
}
html, body { padding: 0; margin: 0; background: var(--bg); color: var(--fg); }
body {
  font-family: "Source Serif Pro Variable", "Source Serif 4", "Times New Roman", Times, serif;
  font-size: 18px;
  line-height: 1.6;
  text-rendering: optimizeLegibility;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
}
.container { max-width: 90ch; margin: 0 auto; padding: 28px 18px 48px; }

.theme-controls { position: absolute; top: 0; right: 0; display: flex; justify-content: flex-end; padding: 10px 18px 0; }
.theme-controls button {
  border: 1px solid var(--fg);
  background: transparent;
  color: var(--fg);
  border-radius: 999px;
  padding: 4px 10px;
  cursor: pointer;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
  font-size: 0.9rem;
}
.theme-controls button[data-mode='auto'] {
  border-style: dashed;
  letter-spacing: 0.06em;
}
.theme-controls button:hover { background: var(--code-bg); }

/* Language selector: styled to match buttons */
.theme-controls select {
  border: 1px solid var(--fg);
  background: transparent;
  color: var(--fg);
  border-radius: 999px;
  padding: 4px 28px 4px 10px; /* right padding for arrow */
  cursor: pointer;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace;
  font-size: 0.9rem;
  -webkit-appearance: none; -moz-appearance: none; appearance: none;
  background-image: url('data:image/svg+xml;utf8,<svg xmlns="http://www.w3.org/2000/svg" width="10" height="10" viewBox="0 0 10 10"><path fill="%23aaa" d="M1 3l4 4 4-4z"/></svg>');
  background-repeat: no-repeat; background-position: right 10px center; background-size: 10px 10px;
}
.theme-controls select:hover { background: var(--code-bg); }
.theme-controls select:disabled { opacity: 0.6; cursor: not-allowed; }

h1, h2, h3, h4, h5, h6 { line-height: 1.2; margin: 1.4em 0 0.7em; font-weight: 700; letter-spacing: 0.02em; font-family: "Source Sans Variable", "Source Sans 3", Arial, sans-serif; }
h1 { font-size: 2.2rem; }
h2 { font-size: 1.6rem; }
h3 { font-size: 1.25rem; }
h4 { font-size: 1.1rem; }
p { margin: 1em 0; text-align: justify; }
a { color: var(--link); text-decoration: underline; text-decoration-thickness: 1px; text-underline-offset: 2px; text-decoration-skip-ink: auto; }
a:hover { opacity: 0.9; }
::selection { background: color-mix(in srgb, var(--link) 25%, transparent); }
img, video { max-width: 100%; height: auto; border-radius: 2px; box-shadow: 0 1px 0 var(--shadow); }
hr { border: 0; border-top: 1px dashed var(--border); margin: 2.2rem 0; }
ul, ol { padding-left: 1.2rem; }
li { margin: 0.35rem 0; }
/* Index page */
.index-search { margin: 0.8rem 0 1rem; }
.index-search input[type='search'] {
  width: 100%; padding: 0.5rem 0.75rem; border: 1px solid var(--border); border-radius: 6px; background: var(--code-bg); color: var(--fg);
}
ul.index-tree { list-style: none; padding-left: 0.2rem; column-gap: 2rem; }
/* Multi-column layout for the top-level index to save vertical space */
@media (min-width: 800px) { ul.index-tree { column-count: 2; } }
@media (min-width: 1200px) { ul.index-tree { column-count: 3; } }
ul.index-tree > li { break-inside: avoid; -webkit-column-break-inside: avoid; page-break-inside: avoid; }
ul.index-tree > li.dir > strong { display: inline-block; margin-top: 0.5rem; }
li.hidden { display: none; }
/* Language nav on index */
.lang-index-nav { margin: 0.4rem 0 0.8rem; font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, "Liberation Mono", "Courier New", monospace; font-size: 0.9rem; color: var(--muted); }
.lang-index-nav a { color: var(--link); text-decoration: none; padding: 2px 6px; border-radius: 999px; border: 1px solid transparent; }
.lang-index-nav a:hover { background: var(--code-bg); }
.lang-index-nav a.active { border-color: var(--fg); }
blockquote {
  margin: 1.2rem 0; padding: 0.75rem 1rem; border-left: 3px solid var(--border);
  color: var(--muted); background: color-mix(in srgb, var(--code-bg) 65%, transparent);
  font-style: italic;
}
code, pre {
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, \"Liberation Mono\", \"Courier New\", monospace;
  font-size: 0.95em;
}
pre {
  background: var(--code-bg);
  padding: 0.9rem; border-radius: 6px; overflow: auto; border: 1px solid var(--border);
}
.executable-code { margin: 1em 0; }
.executable-code > pre { margin-top: 0; }
.code-actions {
  display: flex; align-items: center; justify-content: space-between;
  padding: 0.35rem 0.55rem; border: 1px solid var(--border); border-bottom: 0;
  border-radius: 6px 6px 0 0; background: var(--code-bg);
  color: var(--muted); font: 0.8rem ui-monospace, SFMono-Regular, Menlo, monospace;
}
.code-actions + pre { border-radius: 0 0 6px 6px; }
.run-code {
  border: 1px solid var(--border); border-radius: 4px; padding: 0.2rem 0.65rem;
  background: var(--bg); color: var(--fg); cursor: pointer; font: inherit;
}
.run-code:disabled { opacity: 0.6; cursor: wait; }
.code-output {
  min-height: 1.5em; color: var(--fg); background: var(--code-bg);
  padding: 0.9rem; border: 1px dashed var(--border);
  border-style: dashed; border-radius: 6px !important;
}
.code-output.text-output {
  white-space: pre-wrap;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
  font-size: 0.95em;
}
.code-output.markdown-output { white-space: normal; }
.code-output.markdown-output > :first-child { margin-top: 0; }
.code-output.markdown-output > :last-child { margin-bottom: 0; }
.codex-output > :first-child { margin-top: 0; }
.codex-output > :last-child { margin-bottom: 0; }
.codex-message + .codex-message { border-top: 1px solid var(--border); margin-top: 1rem; }
.codex-command { margin: 0.75rem 0; font-size: 0.9em; }
.codex-command summary { font-family: ui-monospace, SFMono-Regular, Menlo, monospace; }
.codex-command summary span, .codex-usage { color: var(--muted); }
.codex-command pre, .codex-stderr pre { margin-bottom: 0; }
.codex-error {
  margin: 0.75rem 0; padding: 0.6rem 0.8rem; border-left: 3px solid #b94a48;
  background: color-mix(in srgb, #b94a48 12%, var(--code-bg));
}
.codex-usage { margin-top: 1rem; font-size: 0.8rem; }
.codex-stderr { margin-top: 1rem; }
.saved-code-result {
  margin: 1rem 0; padding: 0.9rem; border: 1px dashed var(--border);
  border-radius: 6px; background: var(--code-bg);
}
.saved-code-result > :first-child { margin-top: 0; }
.saved-code-result > :last-child { margin-bottom: 0; }
.graphviz-dot {
  margin: 1rem 0;
  padding: 0.9rem;
  border: 1px solid var(--border);
  border-radius: 6px;
  background: color-mix(in srgb, var(--code-bg) 60%, transparent);
  overflow-x: auto;
}
.graphviz-dot[data-graphviz-pending='1'] {
  color: var(--muted);
  font: 0.9rem ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, monospace;
}
.graphviz-rendered {
  min-width: min-content;
}
.graphviz-rendered svg {
  display: block;
  max-width: 100%;
  height: auto;
  margin: 0 auto;
}
.graphviz-error {
  border-style: dashed;
  background: color-mix(in srgb, #b94a48 12%, var(--code-bg));
}
.graphviz-error pre {
  margin: 0;
  white-space: pre-wrap;
}
 code { background: var(--code-bg); padding: 0.1rem 0.35rem; border-radius: 4px; }
 pre code { padding: 0; background: transparent; }
 table { width: 100%; border-collapse: collapse; margin: 1.2rem 0; }
 th, td { padding: 0.5rem 0.6rem; border: 1px solid var(--border); text-align: left; }
 thead th { background: color-mix(in srgb, var(--code-bg) 85%, transparent); }
 details { border: 1px solid var(--border); border-radius: 6px; padding: 0.6rem 0.9rem; background: color-mix(in srgb, var(--code-bg) 75%, transparent); }
 summary { cursor: pointer; font-weight: 600; }
 kbd { font-family: inherit; background: var(--code-bg); border: 1px solid var(--border); border-bottom-width: 2px; padding: 0 0.35rem; border-radius: 4px; }

 /* Footnotes — subtle, compact, aligned */
 .footnote-definition {
   display: grid;
   grid-template-columns: min-content 1fr;
   align-items: baseline; /* keep number and first line aligned */
   column-gap: 0.5rem;
   margin: 0.6rem 0;
   padding-left: 0.5rem;
   border-left: 1px dotted var(--border);
   background: transparent;
   font-size: 0.9em; /* slightly smaller */
   scroll-margin-top: 72px; /* nicer anchor jump */
 }
 .footnote-definition > .footnote-definition-label {
   grid-column: 1;
   display: inline-block;
   min-width: 2ch; /* space for two-digit notes */
   text-align: right;
   vertical-align: baseline; /* override <sup> default */
   align-self: baseline;
   font-size: 0.85em;
   line-height: 1.2;
   color: var(--muted);
   text-decoration: none;
   margin: 0; /* avoid shifting baseline */
 }
 .footnote-definition > :not(.footnote-definition-label) {
   grid-column: 2; /* everything else flows in content column */
   margin: 0.15rem 0; /* tight vertical rhythm */
 }
 /* Superscript reference styling (if present) */
 a.footnote-reference > sup,
 .footnote-reference-label {
   font-size: 0.75em;
   vertical-align: super;
   line-height: 1;
   color: var(--muted);
 }
 a.footnote-reference:hover > sup { color: var(--fg); }
 a.footnote-backref { margin-left: 0.3rem; text-decoration: none; font-size: 0.75em; color: var(--muted); }
 a.footnote-backref:hover { color: var(--fg); }
 @media (max-width: 600px) { body { font-size: 20px; } .container { padding: 0 22px 56px; } }
 @media (min-width: 900px) { body { font-size: 19px; } .container { padding: 28px 22px 56px; } }
"#
}

static SYNTAX_SET: Lazy<SyntaxSet> = Lazy::new(|| SyntaxSet::load_defaults_newlines());
static THEME_SET: Lazy<ThemeSet> = Lazy::new(ThemeSet::load_defaults);

fn syntax_css(light_name: Option<&str>, dark_name: Option<&str>) -> (String, String) {
    let light_theme = resolve_theme(light_name).unwrap_or_else(|| {
        if !light_name.is_none() {
            eprintln!("[haystack] theme-light not found, using InspiredGitHub/base16-ocean.light fallback");
        }
        THEME_SET
            .themes
            .get("InspiredGitHub")
            .or_else(|| THEME_SET.themes.get("base16-ocean.light"))
            .expect("InspiredGitHub or base16-ocean.light theme present")
    });

    let dark_theme = resolve_theme(dark_name).unwrap_or_else(|| {
        if !dark_name.is_none() {
            eprintln!("[haystack] theme-dark not found, using base16-ocean.dark/Solarized (dark) fallback");
        }
        THEME_SET
            .themes
            .get("base16-ocean.dark")
            .or_else(|| THEME_SET.themes.get("Solarized (dark)"))
            .expect("base16-ocean.dark or Solarized (dark) theme present")
    });
    let light = css_for_theme_with_class_style(light_theme, ClassStyle::Spaced).unwrap_or_default();
    let dark = css_for_theme_with_class_style(dark_theme, ClassStyle::Spaced).unwrap_or_default();
    (light, dark)
}

fn scope_syntect_css(css: &str, scope: &str) -> String {
    // Naively prefix each CSS rule's selectors with the scope.
    // This avoids selector collisions between light/dark theme rules.
    let mut out = String::new();
    for chunk in css.split('}') {
        if let Some((selectors, body)) = chunk.split_once('{') {
            let scoped_selectors = selectors
                .split(',')
                .map(|s| s.trim())
                .filter(|s| !s.is_empty())
                .map(|s| format!("{} {}", scope, s))
                .collect::<Vec<_>>()
                .join(", ");
            out.push_str(&scoped_selectors);
            out.push_str("{\n");
            out.push_str(body);
            out.push_str("}\n");
        }
    }
    out
}

fn resolve_theme(name: Option<&str>) -> Option<&'static Theme> {
    let name = name?.trim();
    if name.is_empty() {
        return None;
    }
    // 1) Exact match
    if let Some(t) = THEME_SET.themes.get(name) {
        return Some(t);
    }
    // 2) Case-insensitive exact
    let lower = name.to_ascii_lowercase();
    if let Some((_, t)) = THEME_SET
        .themes
        .iter()
        .find(|(k, _)| k.to_ascii_lowercase() == lower)
    {
        return Some(t);
    }
    // 3) Normalized (remove non-alnum)
    let norm = normalize_name(name);
    if let Some((_, t)) = THEME_SET
        .themes
        .iter()
        .find(|(k, _)| normalize_name(k) == norm)
    {
        return Some(t);
    }
    // 4) Aliases
    let alias = match lower.as_str() {
        "github" | "inspiredgithub" => Some("InspiredGitHub"),
        "solarized-dark" | "solarized(dark)" => Some("Solarized (dark)"),
        "solarized-light" | "solarized(light)" => Some("Solarized (light)"),
        "ocean-dark" | "base16-ocean-dark" => Some("base16-ocean.dark"),
        "ocean-light" | "base16-ocean-light" => Some("base16-ocean.light"),
        _ => None,
    };
    alias.and_then(|a| THEME_SET.themes.get(a))
}

fn normalize_name(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| c.to_ascii_lowercase())
        .collect()
}

fn list_themes() {
    let mut names: Vec<&str> = THEME_SET.themes.keys().map(|s| s.as_str()).collect();
    names.sort_unstable_by(|a, b| a.to_ascii_lowercase().cmp(&b.to_ascii_lowercase()));
    println!("Available themes ({}):", names.len());
    for n in names {
        println!("- {}", n);
    }
}

fn highlight_code(code: &str, lang: Option<&str>) -> String {
    let ss: &SyntaxSet = &SYNTAX_SET;
    let syntax: &SyntaxReference = match lang {
        Some(l) => ss.find_syntax_by_token(l).unwrap_or_else(|| ss.find_syntax_plain_text()),
        None => ss.find_syntax_plain_text(),
    };
    let mut generator = ClassedHTMLGenerator::new_with_class_style(syntax, ss, ClassStyle::Spaced);
    for line in LinesWithEndings::from(code) {
        let _ = generator.parse_html_for_line_which_includes_newline(line);
    }
    let highlighted = generator.finalize();
    let class_lang = lang.unwrap_or("text");
    format!(r#"<pre><code class="hl language-{}">{}</code></pre>"#, class_lang, highlighted)
}

fn highlight_code_blocks_in_html(input_html: &str) -> String {
    static RE_MD: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)<pre><code class="language-([A-Za-z0-9_+\-.#]+)">(.*?)</code></pre>"#).unwrap()
    });
    static RE_ORG: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)<pre class="src src-([A-Za-z0-9_+\-.#]+)">(.*?)</pre>"#).unwrap()
    });

    let unescape = |s: &str| -> String {
        s.replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&amp;", "&")
            .replace("&quot;", "\"")
            .replace("&#39;", "'")
    };

    let tmp = RE_MD.replace_all(input_html, |caps: &regex::Captures| {
        let lang = caps.get(1).map(|m| m.as_str()).unwrap_or("text");
        let code_escaped = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let code = unescape(code_escaped);
        highlight_code(&code, Some(lang))
    });

    let tmp = RE_ORG.replace_all(&tmp, |caps: &regex::Captures| {
        let lang = caps.get(1).map(|m| m.as_str()).unwrap_or("text");
        let code_escaped = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let code = unescape(code_escaped);
        highlight_code(&code, Some(lang))
    });

    tmp.into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_assets(prefix: &str, entries: &[(&str, &str, &str)]) -> AssetConfig {
        let mut manifest = BTreeMap::new();
        for (source, key, hash) in entries {
            manifest.insert((*source).to_string(), StaticAsset {
                source: (*source).to_string(),
                key: (*key).to_string(),
                hash: (*hash).to_string(),
            });
        }
        AssetConfig {
            prefix: Some(prefix.trim_end_matches('/').to_string()),
            source_dir: PathBuf::from("src"),
            manifest,
            generated: BTreeMap::new(),
        }
    }

    #[test]
    fn parses_code_block_language_code_and_settings_by_id() {
        let markdown = "```text\nskip\n```\n\n```python cwd=examples env.MODE=test flag\nprint('ok')\n```\n";
        let id = format!("{}-0", block_fingerprint("python", "print('ok')\n"));
        let block = markdown_code_block(markdown, &id).unwrap();

        assert_eq!(block.language, "python");
        assert_eq!(block.code, "print('ok')\n");
        assert_eq!(block.settings.get("cwd").map(String::as_str), Some("examples"));
        assert_eq!(block.settings.get("env.MODE").map(String::as_str), Some("test"));
        assert_eq!(block.settings.get("flag").map(String::as_str), Some("true"));
    }

    #[test]
    fn rejects_malformed_block_ids() {
        assert!(markdown_code_block("```sh\ntrue\n```", "../code-1").is_none());
        assert!(markdown_code_block("```sh\ntrue\n```", "code-zero").is_none());
    }

    #[test]
    fn parses_configured_codex_runner() {
        let config: UserConfig = toml::from_str(
            "[code_blocks.codex]\ncommand = \"codex\"\nargs = [\"exec\", \"--json\", \"-\"]\noutput_format = \"codex\"\n",
        ).unwrap();
        let runner = config.code_blocks.get("codex").unwrap();

        assert_eq!(runner.command, "codex");
        assert_eq!(runner.args, ["exec", "--json", "-"]);
        assert!(!runner.args.iter().any(|arg| arg.contains("{code}")));
        assert!(matches!(runner.output_format, OutputFormat::Codex));
    }

    #[test]
    fn built_in_runners_are_default_toml_configuration() {
        let config: UserConfig = toml::from_str(DEFAULT_EXECUTION_CONFIG).unwrap();

        assert_eq!(config.code_blocks.len(), 8);
        assert_eq!(config.code_blocks["sh"].command, "sh");
        assert_eq!(config.code_blocks["python"].command, "uv");
        assert_eq!(config.code_blocks["python"].args, ["run", "-"]);
        assert_eq!(config.code_blocks["py"].command, "uv");
        assert_eq!(config.code_blocks["py"].args, ["run", "-"]);
        assert_eq!(config.code_blocks["codex"].args, ["exec", "--json", "-"]);
        assert!(matches!(
            config.code_blocks["codex"].output_format,
            OutputFormat::Codex
        ));
        assert_eq!(config.code_blocks["javascript"].command, "node");
    }

    #[test]
    fn renders_run_button_only_when_execution_context_is_present() {
        let theme = ThemeConfig::default();
        let enabled = PageLangCtx {
            exec_source: Some("guide.md".to_string()),
            exec_languages: vec!["python".to_string()],
            ..PageLangCtx::default()
        };
        let mut assets = AssetConfig::default();
        let with_execution =
            convert_markdown_to_html_with_ctx("```python\nprint(1)\n```", &theme, Some(&enabled), &mut assets).unwrap();
        let mut assets = AssetConfig::default();
        let without_execution =
            convert_markdown_to_html_with_ctx("```python\nprint(1)\n```", &theme, None, &mut assets).unwrap();

        assert!(with_execution.contains(r#"data-block-id="temp-"#));
        assert!(with_execution.contains(r#"class="run-code""#));
        assert!(with_execution.contains("haystackTypesetMath(output)"));
        assert!(!without_execution.contains(r#"class="run-code""#));
    }

    #[test]
    fn renders_markdown_output_as_html_fragment() {
        let fragment = render_markdown_fragment("# Result\n\n- one\n- two\n\n<img src=x onerror=alert(1)>");

        assert!(fragment.contains("<h1>Result</h1>"));
        assert!(fragment.contains("<li>one</li>"));
        assert!(fragment.contains("&lt;img"));
        assert!(!fragment.contains("<html"));
    }

    #[test]
    fn renders_dot_code_blocks_as_graphviz_containers() {
        let theme = ThemeConfig::default();
        let mut assets = AssetConfig::default();
        let rendered = convert_markdown_to_html_with_ctx(
            "```dot\ndigraph { A -> B }\n```",
            &theme,
            None,
            &mut assets,
        ).unwrap();

        assert!(rendered.contains(r#"class="graphviz-dot""#));
        assert!(rendered.contains(r#"class="graphviz-dot-source""#));
        assert!(rendered.contains("digraph { A -&gt; B }"));
        assert!(rendered.contains("@hpcc-js/wasm-graphviz"));
        assert!(!rendered.contains(r#"language-dot"#));
    }

    #[test]
    fn renders_dot_code_blocks_in_markdown_fragments() {
        let fragment = render_markdown_fragment("```graphviz\ngraph { A -- B }\n```");

        assert!(fragment.contains(r#"class="graphviz-dot""#));
        assert!(fragment.contains("graph { A -- B }"));
        assert!(!fragment.contains(r#"language-graphviz"#));
    }

    #[test]
    fn renders_codex_jsonl_events() {
        let jsonl = concat!(
            "{\"type\":\"thread.started\",\"thread_id\":\"thread_1\"}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_1\",\"type\":\"command_execution\",\"command\":\"cargo test\",\"aggregated_output\":\"ok\\n\",\"exit_code\":0,\"status\":\"completed\"}}\n",
            "{\"type\":\"item.completed\",\"item\":{\"id\":\"item_2\",\"type\":\"agent_message\",\"text\":\"## Done\\n\\nTests pass.\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":100,\"cached_input_tokens\":75,\"output_tokens\":20}}\n",
        );
        let html = render_codex_output(jsonl.as_bytes(), b"diagnostic").unwrap();

        assert!(html.contains("<code>cargo test</code>"));
        assert!(html.contains("<h2>Done</h2>"));
        assert!(html.contains("25 uncached input · 75 cached input · 20 output tokens"));
        assert!(html.contains("Codex diagnostics"));
    }

    #[test]
    fn rewrites_root_relative_asset_urls_only() {
        let mut assets = test_assets("https://assets.example.com/", &[
            ("images/a.png", "images/a.111111111111.png", "111111111111"),
            ("audio/theme.mp3", "audio/theme.222222222222.mp3", "222222222222"),
            ("posters/hero.jpg", "posters/hero.333333333333.jpg", "333333333333"),
            ("small.png", "small.444444444444.png", "444444444444"),
            ("large.png", "large.555555555555.png", "555555555555"),
            ("style.css", "style.666666666666.css", "666666666666"),
            ("files/book.pdf", "files/book.777777777777.pdf", "777777777777"),
        ]);
        let html = concat!(
            r#"<img src="/images/a.png">"#,
            r#"<audio src='/audio/theme.mp3'></audio>"#,
            r#"<video poster="/posters/hero.jpg"></video>"#,
            r#"<img srcset="/small.png 1x, /large.png 2x, https://cdn.example.com/x.png 3x">"#,
            r#"<link href="/style.css" rel="stylesheet">"#,
            r#"<a href="/guide.html">Guide</a>"#,
            r#"<a href="/files/book.pdf">PDF</a>"#,
            r#"<img src="//cdn.example.com/already.png">"#,
        );

        let rewritten = rewrite_asset_urls(html, &mut assets, "posts/2026").unwrap();

        assert!(rewritten.contains(r#"src="https://assets.example.com/images/a.111111111111.png""#));
        assert!(rewritten.contains(r#"src='https://assets.example.com/audio/theme.222222222222.mp3'"#));
        assert!(rewritten.contains(r#"poster="https://assets.example.com/posters/hero.333333333333.jpg""#));
        assert!(rewritten.contains(r#"srcset="https://assets.example.com/small.444444444444.png 1x, https://assets.example.com/large.555555555555.png 2x, https://cdn.example.com/x.png 3x""#));
        assert!(rewritten.contains(r#"href="https://assets.example.com/style.666666666666.css""#));
        assert!(rewritten.contains(r#"<a href="/guide.html">Guide</a>"#));
        assert!(rewritten.contains(r#"<a href="https://assets.example.com/files/book.777777777777.pdf">PDF</a>"#));
        assert!(rewritten.contains(r#"src="//cdn.example.com/already.png""#));
    }

    #[test]
    fn rewrites_relative_asset_urls_from_page_directory() {
        let mut assets = test_assets("https://assets.example.com/", &[
            ("lessons/audio/n2-aging-society.mp3", "lessons/audio/n2-aging-society.aaaaaaaaaaaa.mp3", "aaaaaaaaaaaa"),
            ("lessons/n2/images/chart.png", "lessons/n2/images/chart.bbbbbbbbbbbb.png", "bbbbbbbbbbbb"),
            ("lessons/audio/transcript.pdf", "lessons/audio/transcript.cccccccccccc.pdf", "cccccccccccc"),
        ]);
        let html = concat!(
            r#"<audio src="../audio/n2-aging-society.mp3?version=1"></audio>"#,
            r#"<img src="./images/chart.png#v2">"#,
            r#"<a href="../audio/transcript.pdf">Transcript</a>"#,
        );
        let rewritten = rewrite_asset_urls_collect(html, &mut assets, "lessons/n2").unwrap();

        assert!(rewritten.contains(r#"src="https://assets.example.com/lessons/audio/n2-aging-society.aaaaaaaaaaaa.mp3?version=1""#));
        assert!(rewritten.contains(r#"src="https://assets.example.com/lessons/n2/images/chart.bbbbbbbbbbbb.png#v2""#));
        assert!(rewritten.contains(r#"href="https://assets.example.com/lessons/audio/transcript.cccccccccccc.pdf""#));
        assert!(assets.generated.contains_key("lessons/audio/n2-aging-society.mp3"));
        assert!(assets.generated.contains_key("lessons/n2/images/chart.png"));
        assert!(assets.generated.contains_key("lessons/audio/transcript.pdf"));
    }

    #[test]
    fn build_writes_static_file_list() {
        let root = std::env::temp_dir().join(format!(
            "haystack-static-list-test-{}-{}",
            std::process::id(),
            generate_block_id(),
        ));
        let src = root.join("src");
        let out = root.join("output");
        fs::create_dir_all(src.join("audio")).unwrap();
        fs::create_dir_all(src.join("posts")).unwrap();
        fs::write(src.join("posts").join("index.md"), "# Home\n\n<audio src=\"../audio/theme.mp3\"></audio>\n").unwrap();
        fs::write(src.join("audio").join("theme.mp3"), b"audio").unwrap();
        fs::write(src.join("robots.txt"), "User-agent: *\n").unwrap();
        let manifest = out.join("static-files.txt");
        let hash = content_hash(b"audio");
        let expected_key = format!("audio/theme.{hash}.mp3");
        let mut assets = AssetConfig::new(
            Some("https://assets.example.com".to_string()),
            &src,
            &manifest,
        ).unwrap();

        build_all(
            &src,
            &out,
            &ThemeConfig::default(),
            &LangConfig::default(),
            false,
            &mut assets,
            &manifest,
        ).unwrap();

        let page = fs::read_to_string(out.join("posts").join("index.html")).unwrap();
        let list = fs::read_to_string(&manifest).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(page.contains(&format!(r#"src="https://assets.example.com/{expected_key}""#)));
        assert_eq!(list, format!("audio/theme.mp3\t{expected_key}\t{hash}\n"));
    }

    #[test]
    fn build_uses_manifest_when_asset_file_is_missing() {
        let root = std::env::temp_dir().join(format!(
            "haystack-static-manifest-test-{}-{}",
            std::process::id(),
            generate_block_id(),
        ));
        let src = root.join("src");
        let out = root.join("output");
        fs::create_dir_all(src.join("posts")).unwrap();
        fs::write(src.join("posts").join("index.md"), "# Home\n\n<audio src=\"../audio/theme.mp3\"></audio>\n").unwrap();
        let manifest = root.join("static-assets.txt");
        fs::write(&manifest, "audio/theme.mp3\taudio/theme.abc123.mp3\tabc123\n").unwrap();
        let mut assets = AssetConfig::new(
            Some("https://assets.example.com".to_string()),
            &src,
            &manifest,
        ).unwrap();

        build_all(
            &src,
            &out,
            &ThemeConfig::default(),
            &LangConfig::default(),
            false,
            &mut assets,
            &manifest,
        ).unwrap();

        let page = fs::read_to_string(out.join("posts").join("index.html")).unwrap();
        let list = fs::read_to_string(&manifest).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(page.contains(r#"src="https://assets.example.com/audio/theme.abc123.mp3""#));
        assert_eq!(list, "audio/theme.mp3\taudio/theme.abc123.mp3\tabc123\n");
    }

    #[test]
    fn build_leaves_missing_asset_without_manifest_unchanged() {
        let root = std::env::temp_dir().join(format!(
            "haystack-static-missing-test-{}-{}",
            std::process::id(),
            generate_block_id(),
        ));
        let src = root.join("src");
        let out = root.join("output");
        fs::create_dir_all(src.join("posts")).unwrap();
        fs::write(src.join("posts").join("index.md"), "# Home\n\n<audio src=\"../audio/missing.mp3\"></audio>\n").unwrap();
        let manifest = root.join("static-assets.txt");
        let mut assets = AssetConfig::new(
            Some("https://assets.example.com".to_string()),
            &src,
            &manifest,
        ).unwrap();

        build_all(
            &src,
            &out,
            &ThemeConfig::default(),
            &LangConfig::default(),
            false,
            &mut assets,
            &manifest,
        ).unwrap();

        let page = fs::read_to_string(out.join("posts").join("index.html")).unwrap();
        let list = fs::read_to_string(&manifest).unwrap();
        let _ = fs::remove_dir_all(&root);

        assert!(page.contains(r#"src="../audio/missing.mp3""#));
        assert_eq!(list, "");
    }

    #[test]
    fn assigns_stable_id_and_replaces_saved_text_result() {
        let path = std::env::temp_dir().join(format!(
            "haystack-result-test-{}-{}.md",
            std::process::id(),
            generate_block_id(),
        ));
        let original = "# Example\n\n```python\nprint(1)\n```\n\nAfter.\n";
        fs::write(&path, original).unwrap();
        let temporary_id = format!("{}-0", block_fingerprint("python", "print(1)\n"));
        let block = markdown_code_block(original, &temporary_id).unwrap();
        let stable_id = ensure_block_id(&path, original, &block).unwrap();
        let save = ResultSave {
            path: path.clone(),
            block_id: stable_id.clone(),
            executed_code: "print(1)\n".to_string(),
            output_format: OutputFormat::Text,
        };

        save_result(&save, b"first\n").unwrap();
        save_result(&save, b"second with ``` inside\n").unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let _ = fs::remove_file(&path);

        assert!(updated.contains(&format!("```python id={stable_id}")));
        assert_eq!(updated.matches("<!-- haystack-result:").count(), 1);
        assert!(!updated.contains("first"));
        assert!(updated.contains("second with ``` inside"));
        assert!(updated.contains("````text haystack-result="));
        assert!(updated.contains("\nAfter.\n"));
    }

    #[test]
    fn generated_block_ids_are_short() {
        let id = generate_block_id();

        assert_eq!(id.len(), 10);
        assert!(id.starts_with("b-"));
        assert!(id[2..].bytes().all(|byte| byte.is_ascii_hexdigit()));
    }

    #[test]
    fn saves_and_renders_markdown_results_inline() {
        let path = std::env::temp_dir().join(format!(
            "haystack-markdown-result-test-{}-{}.md",
            std::process::id(),
            generate_block_id(),
        ));
        let original = "```sh id=example\nprintf result\n```\n";
        fs::write(&path, original).unwrap();
        let save = ResultSave {
            path: path.clone(),
            block_id: "example".to_string(),
            executed_code: "printf result\n".to_string(),
            output_format: OutputFormat::Markdown,
        };

        save_result(&save, b"## Result\n\nValue: $x^2$\n").unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let mut assets = AssetConfig::default();
        let rendered =
            convert_markdown_to_html_with_ctx(&updated, &ThemeConfig::default(), None, &mut assets).unwrap();
        let _ = fs::remove_file(&path);

        assert!(updated.contains("```markdown haystack-result=example"));
        assert!(rendered.contains(r#"<div class="saved-code-result"><h2>Result</h2>"#));
        assert!(rendered.contains("Value: $x^2$"), "{rendered}");
    }

    #[test]
    fn saves_and_renders_codex_results_inline() {
        let path = std::env::temp_dir().join(format!(
            "haystack-codex-result-test-{}-{}.md",
            std::process::id(),
            generate_block_id(),
        ));
        let original = "```codex id=agent\nAnswer briefly.\n```\n";
        fs::write(&path, original).unwrap();
        let save = ResultSave {
            path: path.clone(),
            block_id: "agent".to_string(),
            executed_code: "Answer briefly.\n".to_string(),
            output_format: OutputFormat::Codex,
        };
        let jsonl = concat!(
            "{\"type\":\"item.completed\",\"item\":{\"type\":\"agent_message\",\"text\":\"## Answer\\n\\nDone.\"}}\n",
            "{\"type\":\"turn.completed\",\"usage\":{\"input_tokens\":40,\"cached_input_tokens\":30,\"output_tokens\":5}}\n",
        );

        save_result(&save, jsonl.as_bytes()).unwrap();
        let updated = fs::read_to_string(&path).unwrap();
        let mut assets = AssetConfig::default();
        let rendered =
            convert_markdown_to_html_with_ctx(&updated, &ThemeConfig::default(), None, &mut assets).unwrap();
        let _ = fs::remove_file(&path);

        assert!(updated.contains("```codex haystack-result=agent"));
        assert!(rendered.contains("<h2>Answer</h2>"));
        assert!(rendered.contains("10 uncached input · 30 cached input · 5 output tokens"));
    }
}
