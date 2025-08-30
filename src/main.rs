use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pulldown_cmark::{html, CodeBlockKind, CowStr, Event, Options, Parser as MdParser, Tag, TagEnd};
use tiny_http::{Header, Response, Server};
use walkdir::WalkDir;
use orgize::Org;
use once_cell::sync::Lazy;
use regex::Regex;
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
    },
    /// List available syntax highlighting themes
    Themes,
}

#[derive(Debug, Clone, Default)]
struct ThemeConfig {
    light: Option<String>,
    dark: Option<String>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build { theme_light, theme_dark } => {
            let src = Path::new("src");
            let out = Path::new("output");
            let theme = ThemeConfig { light: theme_light, dark: theme_dark };
            build_all(src, out, &theme)?;
        }
        Commands::Serve { port, theme_light, theme_dark } => {
            let src = Path::new("src");
            let theme = ThemeConfig { light: theme_light, dark: theme_dark };
            serve(port, src, &theme)?;
        }
        Commands::Themes => {
            list_themes();
        }
    }

    Ok(())
}

fn build_all(src_dir: &Path, out_dir: &Path, theme: &ThemeConfig) -> Result<()> {
    if !src_dir.exists() {
        return Err(anyhow!("src folder not found: {}", src_dir.display()));
    }
    fs::create_dir_all(out_dir).with_context(|| format!("creating {}", out_dir.display()))?;

    for entry in WalkDir::new(src_dir).into_iter().filter_map(|e| e.ok()) {
        let path = entry.path();
        if path.is_file() {
            match path.extension().and_then(|s| s.to_str()) {
                Some("md") | Some("org") => {
                    let rel = path.strip_prefix(src_dir).unwrap();
                    let mut out_path = out_dir.to_path_buf();
                    let file_stem = rel.with_extension("");
                    // Keep subdirectories structure
                    out_path.push(file_stem);
                    out_path.set_extension("html");

                    if let Some(parent) = out_path.parent() {
                        fs::create_dir_all(parent)?;
                    }

                    let html = convert_file(path, theme)?;
                    fs::write(&out_path, html).with_context(|| format!(
                        "writing output file {}",
                        out_path.display()
                    ))?;
                    println!(
                        "Built {} -> {}",
                        path.display(),
                        out_path.display()
                    );
                }
                _ => {
                    // Copy static files as-is
                    let rel = path.strip_prefix(src_dir).unwrap();
                    let mut out_path = out_dir.to_path_buf();
                    out_path.push(rel);
                    if let Some(parent) = out_path.parent() {
                        fs::create_dir_all(parent)?;
                    }
                    fs::copy(path, &out_path).with_context(|| format!(
                        "copying static {} -> {}",
                        path.display(),
                        out_path.display()
                    ))?;
                    println!("Copied {} -> {}", path.display(), out_path.display());
                }
            }
        }
    }
    Ok(())
}

fn serve(port: u16, src_dir: &Path, theme: &ThemeConfig) -> Result<()> {
    if !src_dir.exists() {
        return Err(anyhow!("src folder not found: {}", src_dir.display()));
    }
    let addr = format!("0.0.0.0:{}", port);
    println!("Serving {} on http://{}/", src_dir.display(), addr);
    let server = Server::http(addr).map_err(|e| anyhow!("server error: {e}"))?;

    for request in server.incoming_requests() {
        let url_path = request.url(); // includes leading '/'
        let mut path = url_path.split('?').next().unwrap_or("").trim_start_matches('/');
        if path.is_empty() {
            path = "index.html";
        }

        // Basic path traversal guard
        if path.split('/').any(|seg| seg == ".." || seg.contains('\\')) {
            let resp = Response::from_string("Bad Request").with_status_code(400);
            let _ = request.respond(resp);
            continue;
        }

        let resp = if path.ends_with(".html") {
            let base = &path[..path.len() - ".html".len()];
            let md_path = src_dir.join(format!("{}.md", base));
            let org_path = src_dir.join(format!("{}.org", base));

            if md_path.exists() {
                match fs::read_to_string(&md_path).map(|s| convert_markdown_to_html(&s, theme)) {
                    Ok(html) => Response::from_string(html)
                        .with_status_code(200)
                        .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                    Err(e) => Response::from_string(format!("Error reading {}: {}", md_path.display(), e))
                        .with_status_code(500),
                }
            } else if org_path.exists() {
                match fs::read_to_string(&org_path).map(|s| convert_org_to_html(&s, theme)) {
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
            let static_path = src_dir.join(path);
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

fn convert_file(path: &Path, theme: &ThemeConfig) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening input file {}", path.display()))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("reading input file {}", path.display()))?;

    match path.extension().and_then(|s| s.to_str()) {
        Some("md") => Ok(convert_markdown_to_html(&buf, theme)),
        Some("org") => Ok(convert_org_to_html(&buf, theme)),
        other => Err(anyhow!("unsupported extension {:?} for {}", other, path.display())),
    }
}

fn convert_markdown_to_html(input: &str, theme: &ThemeConfig) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = MdParser::new_ext(input, options);

    // Transform code blocks into syntect-highlighted HTML
    let mut events = Vec::new();
    let mut in_code = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();

    for ev in parser {
        match ev {
            Event::Start(Tag::CodeBlock(kind)) => {
                in_code = true;
                code_buf.clear();
                code_lang = match kind {
                    CodeBlockKind::Fenced(info) => {
                        let first = info.split_whitespace().next().unwrap_or("");
                        if first.is_empty() { None } else { Some(first.to_string()) }
                    }
                    CodeBlockKind::Indented => None,
                };
            }
            Event::Text(t) if in_code => {
                code_buf.push_str(&t);
            }
            Event::End(TagEnd::CodeBlock) => {
                let html_snippet = highlight_code(&code_buf, code_lang.as_deref());
                events.push(Event::Html(CowStr::from(html_snippet)));
                in_code = false;
                code_lang = None;
            }
            other => {
                if !in_code {
                    events.push(other);
                }
            }
        }
    }

    let mut out = String::new();
    html::push_html(&mut out, events.into_iter());
    let title = extract_title_from_markdown(input);
    wrap_html_page(out, title, theme)
}

// Minimal Org-mode to HTML converter: supports headings, lists, paragraphs.
fn convert_org_to_html(input: &str, theme: &ThemeConfig) -> String {
    let org = Org::parse(input);
    let mut bytes: Vec<u8> = Vec::new();
    let _ = org.write_html(&mut bytes);
    let body = String::from_utf8(bytes).unwrap_or_default();
    let title = extract_title_from_org(input);
    let body = highlight_code_blocks_in_html(&body);
    wrap_html_page(body, title, theme)
}

fn wrap_html_page(body: String, title: Option<String>, theme: &ThemeConfig) -> String {
    let css = default_css();
    let (syn_css_light, syn_css_dark) = syntax_css(theme.light.as_deref(), theme.dark.as_deref());
    let page_title = title.as_deref().unwrap_or("haystack");
    let theme_bootstrap = r#"(function(){
  try {
    document.documentElement.setAttribute('data-theme', localStorage.getItem('haystack-theme') || 'auto');
  } catch(e) {}
})();"#;
    let controls_html = r#"<div class="theme-controls"><button id="themeToggle" aria-label="Toggle theme">🌓</button></div>"#;
    let toggle_script = r#"(function(){
  function setTheme(t){ document.documentElement.setAttribute('data-theme', t); try{ localStorage.setItem('haystack-theme', t); }catch(e){} }
  const btn = document.getElementById('themeToggle');
  if(btn){ btn.addEventListener('click', function(){
    const cur = document.documentElement.getAttribute('data-theme')||'auto';
    const next = (cur==='light') ? 'dark' : (cur==='dark' ? 'auto' : 'light');
    setTheme(next);
  }); }
})();"#;
    // Prepare syntect CSS for light/dark and auto (media-driven)
    let syn_light_scoped = scope_syntect_css(&syn_css_light, r#"html[data-theme='light']"#);
    let syn_dark_scoped = scope_syntect_css(&syn_css_dark, r#"html[data-theme='dark']"#);
    let syn_auto_light = format!("@media (prefers-color-scheme: light) {{\n{}\n}}", scope_syntect_css(&syn_css_light, r#"html[data-theme='auto']"#));
    let syn_auto_dark = format!("@media (prefers-color-scheme: dark) {{\n{}\n}}", scope_syntect_css(&syn_css_dark, r#"html[data-theme='auto']"#));

    let wrap_overrides = "\n/* Force code wrapping */\n.container pre, .container pre code, .container code.hl, .container pre .hl {\n  white-space: pre-wrap;\n  overflow-wrap: anywhere;\n  word-break: break-word;\n}\n";
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
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title>\n<script>{}</script>\n<style>\n{}\n{}\n{}\n{}\n{}\n{}\n</style>\n{}\n</head>\n<body>\n{}\n<main class=\"container\">\n{}\n</main>\n<script>{}</script>\n<script>{}</script>\n</body>\n</html>",
        page_title, theme_bootstrap, css, syn_light_scoped, syn_dark_scoped, syn_auto_light, syn_auto_dark, wrap_overrides, head_extra, controls_html, body, toggle_script, indicator_script
    )
}

fn read_head_snippet() -> Option<String> {
    let path = Path::new("theme").join("head.html");
    match fs::read_to_string(&path) {
        Ok(s) => Some(s),
        Err(_) => None,
    }
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
  font-family: ui-serif, Georgia, Times, \"Noto Serif\", serif;
  font-size: 17px;
  line-height: 1.7;
  text-rendering: optimizeLegibility;
  -webkit-font-smoothing: antialiased;
  -moz-osx-font-smoothing: grayscale;
}
.container { max-width: 70ch; margin: 0 auto; padding: 28px 18px 48px; }

.theme-controls { position: sticky; top: 0; display: flex; justify-content: flex-end; padding: 10px 18px 0; }
.theme-controls button {
  border: 1px solid var(--fg);
  background: transparent;
  color: var(--fg);
  border-radius: 999px;
  padding: 4px 10px;
  cursor: pointer;
  font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, \"Liberation Mono\", \"Courier New\", monospace;
  font-size: 0.9rem;
}
.theme-controls button[data-mode='auto'] {
  border-style: dashed;
  letter-spacing: 0.06em;
}
.theme-controls button:hover { background: var(--code-bg); }

h1, h2, h3, h4, h5, h6 { line-height: 1.2; margin: 1.6em 0 0.7em; font-weight: 700; letter-spacing: 0.02em; }
h1 { font-size: 2.1rem; }
h2 { font-size: 1.6rem; }
h3 { font-size: 1.25rem; }
h4 { font-size: 1.1rem; }
p { margin: 1em 0; }
a { color: var(--link); text-decoration: underline; text-decoration-thickness: 1px; text-underline-offset: 2px; text-decoration-skip-ink: auto; }
a:hover { opacity: 0.9; }
::selection { background: color-mix(in srgb, var(--link) 25%, transparent); }
img, video { max-width: 100%; height: auto; border-radius: 2px; box-shadow: 0 1px 0 var(--shadow); }
hr { border: 0; border-top: 1px dashed var(--border); margin: 2.2rem 0; }
ul, ol { padding-left: 1.2rem; }
li { margin: 0.35rem 0; }
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
code { background: var(--code-bg); padding: 0.1rem 0.35rem; border-radius: 4px; }
pre code { padding: 0; background: transparent; }
table { width: 100%; border-collapse: collapse; margin: 1.2rem 0; }
th, td { padding: 0.5rem 0.6rem; border: 1px solid var(--border); text-align: left; }
thead th { background: color-mix(in srgb, var(--code-bg) 85%, transparent); }
details { border: 1px solid var(--border); border-radius: 6px; padding: 0.6rem 0.9rem; background: color-mix(in srgb, var(--code-bg) 75%, transparent); }
summary { cursor: pointer; font-weight: 600; }
kbd { font-family: inherit; background: var(--code-bg); border: 1px solid var(--border); border-bottom-width: 2px; padding: 0 0.35rem; border-radius: 4px; }
@media (min-width: 900px) { body { font-size: 18px; } .container { padding: 36px 22px 56px; } }
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
    format!("<pre><code class=\"hl language-{}\">{}</code></pre>", class_lang, highlighted)
}

fn highlight_code_blocks_in_html(input_html: &str) -> String {
    static RE_MD: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)<pre><code class=\"language-([A-Za-z0-9_+\-.#]+)\">(.*?)</code></pre>"#).unwrap()
    });
    static RE_ORG: Lazy<Regex> = Lazy::new(|| {
        Regex::new(r#"(?s)<pre class=\"src src-([A-Za-z0-9_+\-.#]+)\">(.*?)</pre>"#).unwrap()
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
