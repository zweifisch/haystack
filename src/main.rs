use std::fs;
use std::io::Read;
use std::io::Write;
use std::path::Path;
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

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

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { theme_light, theme_dark, index, langs } => {
            let src = Path::new("src");
            let out = Path::new("output");
            let theme = ThemeConfig { light: theme_light, dark: theme_dark };
            let lang = LangConfig::new(langs);
            build_all(src, out, &theme, &lang, index)?;
        }
        Commands::Serve { port, theme_light, theme_dark, langs, allow_exec } => {
            let src = Path::new("src");
            let theme = ThemeConfig { light: theme_light, dark: theme_dark };
            let lang = LangConfig::new(langs);
            let execution = ExecutionConfig::load()?;
            serve(port, src, &theme, &lang, allow_exec, &execution)?;
        }
        Commands::Themes => {
            list_themes();
        }
    }

    Ok(())
}

fn build_all(src_dir: &Path, out_dir: &Path, theme: &ThemeConfig, lang: &LangConfig, generate_index: bool) -> Result<()> {
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
                        let html = convert_file_with_lang(path, theme, &build_page_ctx(&base_src, &rel_root, rel, cfg))?;
                        fs::write(&out_path, html).with_context(|| format!("writing output file {}", out_path.display()))?;
                        println!("Built {} -> {}", path.display(), out_path.display());
                    }
                    Some("html") => {
                        let rel = path.strip_prefix(&src_root).unwrap();
                        doc_paths.push(rel.to_path_buf());
                        let out_path = out_root.join(rel);
                        if let Some(parent) = out_path.parent() { fs::create_dir_all(parent)?; }
                        fs::copy(path, &out_path).with_context(|| format!("copying html {} -> {}", path.display(), out_path.display()))?;
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
    process_lang(src_dir, out_dir, theme, Path::new(""), "", generate_index, lang)?;

    // Build each non-default language under its prefix if configured
    for lang_code in &lang.others {
        let rel_root = Path::new(lang_code);
        if src_dir.join(rel_root).exists() {
            process_lang(src_dir, out_dir, theme, rel_root, &format!("{}/", lang_code), generate_index, lang)?;
        }
    }
    Ok(())
}

fn serve(port: u16, src_dir: &Path, theme: &ThemeConfig, lang_cfg: &LangConfig, allow_exec: bool, execution: &ExecutionConfig) -> Result<()> {
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
                    Ok(s) => Response::from_string(s)
                        .with_status_code(200)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
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
                match fs::read_to_string(&md_path).map(|s| convert_markdown_to_html_with_ctx(&s, theme, ctx.as_ref())) {
                    Ok(html) => Response::from_string(html)
                        .with_status_code(200)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                    Err(e) => Response::from_string(format!("Error reading {}: {}", md_path.display(), e))
                        .with_status_code(500),
                }
            } else if org_path.exists() {
                let ctx = build_runtime_page_ctx(src_dir, &current_lang, base_in, lang_cfg);
                match fs::read_to_string(&org_path).map(|s| convert_org_to_html_with_ctx(&s, theme, ctx.as_ref())) {
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
    let wanted = wanted_id.strip_prefix("code-")?.parse::<usize>().ok()?;
    let parser = MdParser::new_ext(input, Options::all());
    let mut index = 0usize;
    let mut current: Option<(String, String)> = None;
    for event in parser {
        match event {
            Event::Start(Tag::CodeBlock(kind)) => {
                index += 1;
                let info = match kind {
                    CodeBlockKind::Fenced(info) => info.to_string(),
                    CodeBlockKind::Indented => String::new(),
                };
                current = Some((info, String::new()));
            }
            Event::Text(text) if current.is_some() => {
                current.as_mut().unwrap().1.push_str(&text);
            }
            Event::End(TagEnd::CodeBlock) => {
                if index == wanted {
                    let (info, code) = current.take()?;
                    let (language, settings) = parse_fence_info(&info);
                    return Some(MarkdownCodeBlock { language, code, settings });
                }
                current = None;
            }
            _ => {}
        }
    }
    None
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
        let reader = ChannelExecutionOutput { child, receiver: rx, pending: Vec::new() };
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
            let response = match render_codex_output(&stdout, &stderr) {
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
                    self.pending = bytes
                }
                Err(_) => {
                    let _ = self.child.wait();
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

// removed legacy convert_file (replaced by convert_file_with_lang)

fn convert_file_with_lang(path: &Path, theme: &ThemeConfig, ctx: &PageLangCtx) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening input file {}", path.display()))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("reading input file {}", path.display()))?;

    let html = match path.extension().and_then(|s| s.to_str()) {
        Some("md") => convert_markdown_to_html_with_ctx(&buf, theme, Some(ctx)),
        Some("org") => convert_org_to_html_with_ctx(&buf, theme, Some(ctx)),
        other => return Err(anyhow!("unsupported extension {:?} for {}", other, path.display())),
    };
    Ok(html)
}

// removed legacy convert_markdown_to_html (replaced by convert_markdown_to_html_with_ctx)

fn convert_markdown_to_html_with_ctx<'a>(input: &str, theme: &ThemeConfig, ctx: Option<&'a PageLangCtx>) -> String {
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
    let mut code_index = 0usize;
    for ev in parser {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_index += 1;
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
                let mut html_snippet = highlight_code(&code_buf, code_lang.as_deref());
                if let (Some(source), Some(lang)) =
                    (ctx.and_then(|c| c.exec_source.as_deref()), code_lang.as_deref())
                {
                    let is_configured = ctx
                        .map(|c| c.exec_languages.iter().any(|configured| configured.eq_ignore_ascii_case(lang)))
                        .unwrap_or(false);
                    if is_configured {
                        html_snippet = executable_code_html(
                            &html_snippet,
                            source,
                            &format!("code-{}", code_index),
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
    let title = extract_title_from_markdown(input);
    wrap_html_page_with_ctx(body, title, theme, ctx)
}

fn render_markdown_fragment(input: &str) -> String {
    let parser = MdParser::new_ext(input, Options::all());
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
                events.push(Event::Html(CowStr::from(highlight_code(
                    &code_buf,
                    code_lang.as_deref(),
                ))));
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

fn convert_org_to_html_with_ctx<'a>(input: &str, theme: &ThemeConfig, ctx: Option<&'a PageLangCtx>) -> String {
    let org = Org::parse(input);
    let mut bytes: Vec<u8> = Vec::new();
    let _ = org.write_html(&mut bytes);
    let mut body = String::from_utf8(bytes).unwrap_or_default();
    let title = extract_title_from_org(input);
    body = highlight_code_blocks_in_html(&body);
    if let Some(c) = ctx { if let Some(cur) = c.current_lang.as_ref() { if Some(cur) != c.default_lang.as_ref() {
        body = rewrite_internal_links(&body, &format!("/{}/", cur));
    }}}
    wrap_html_page_with_ctx(body, title, theme, ctx)
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
    // MathJax v3: config and loader
    window.MathJax = window.MathJax || {
      tex: {
        inlineMath: [['$', '$'], ['\\(', '\\)']],
        displayMath: [['$$','$$'], ['\\[','\\]']],
        processEscapes: true,
        processEnvironments: true
      },
      options: { skipHtmlTags: ['script','noscript','style','textarea','pre','code'] }
    };
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
      var mj = document.createElement('script');
      mj.id = 'MathJax-script';
      mj.async = true;
      mj.src = 'https://cdn.jsdelivr.net/npm/mathjax@3/es5/tex-chtml.js';
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
    try { if(window.MathJax && window.MathJax.typeset){ setTimeout(function(){ MathJax.typeset(); }, 0); } } catch(e){}
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
        "<!DOCTYPE html>\n<html lang=\"{}\" data-theme=\"auto\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n{}\n<script>{}</script>\n<style>\n{}\n{}\n{}\n{}\n{}\n{}\n</style>\n{}\n</head>\n<body>\n{}\n<main class=\"container\">\n{}\n</main>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n<script>{}</script>\n</body>\n</html>",
        escape_attr(&html_lang), page_title, fonts_head, theme_bootstrap, css, syn_light_scoped, syn_dark_scoped, syn_auto_light, syn_auto_dark, wrap_overrides, head_extra, controls_html, body, toggle_script, indicator_script, share_script, lang_switch_script, execution_script
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
    let available = page_available_langs(base_src, &tail_no_ext, cfg);
    PageLangCtx {
        current_lang: current,
        default_lang: cfg.default.clone(),
        supported_langs: cfg.all_langs(),
        page_tail_html: Some(tail_html),
        available_langs: available,
        exec_source: None,
        exec_languages: Vec::new(),
    }
}

fn build_runtime_page_ctx(src_dir: &Path, current_lang: &Option<String>, base_tail_no_ext: &str, cfg: &LangConfig) -> Option<PageLangCtx> {
    if !cfg.has_langs() { return None; }
    let available = page_available_langs(src_dir, base_tail_no_ext, cfg);
    let tail_html = format!("{}.html", base_tail_no_ext);
    Some(PageLangCtx {
        current_lang: current_lang.clone().or_else(|| cfg.default.clone()),
        default_lang: cfg.default.clone(),
        supported_langs: cfg.all_langs(),
        page_tail_html: Some(tail_html),
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

    #[test]
    fn parses_code_block_language_code_and_settings_by_id() {
        let markdown = "```text\nskip\n```\n\n```python cwd=examples env.MODE=test flag\nprint('ok')\n```\n";
        let block = markdown_code_block(markdown, "code-2").unwrap();

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
        let with_execution =
            convert_markdown_to_html_with_ctx("```python\nprint(1)\n```", &theme, Some(&enabled));
        let without_execution =
            convert_markdown_to_html_with_ctx("```python\nprint(1)\n```", &theme, None);

        assert!(with_execution.contains(r#"data-block-id="code-1""#));
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
}
