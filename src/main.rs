use std::fs;
use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, Context, Result};
use clap::{Parser, Subcommand};
use pulldown_cmark::{html, Event, Options, Parser as MdParser, Tag, TagEnd};
use tiny_http::{Header, Response, Server};
use walkdir::WalkDir;
use orgize::Org;

#[derive(Parser, Debug)]
#[command(name = "haystack", version, about = "Build and serve markdown/org to HTML")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Compile src/*.md and src/*.org to output/*.html
    Build,
    /// Serve on-demand HTML from src/*.md and src/*.org
    Serve {
        /// Port to listen on
        #[arg(long, default_value_t = 4000)]
        port: u16,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Build => {
            let src = Path::new("src");
            let out = Path::new("output");
            build_all(src, out)?;
        }
        Commands::Serve { port } => {
            let src = Path::new("src");
            serve(port, src)?;
        }
    }

    Ok(())
}

fn build_all(src_dir: &Path, out_dir: &Path) -> Result<()> {
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

                    let html = convert_file(path)?;
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
                _ => {}
            }
        }
    }
    Ok(())
}

fn serve(port: u16, src_dir: &Path) -> Result<()> {
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

        // Only handle .html requests
        if !path.ends_with(".html") {
            let resp = Response::from_string("Not Found").with_status_code(404);
            let _ = request.respond(resp);
            continue;
        }

        let base = &path[..path.len() - ".html".len()];
        let md_path = src_dir.join(format!("{}.md", base));
        let org_path = src_dir.join(format!("{}.org", base));

        let resp = if md_path.exists() {
            match fs::read_to_string(&md_path).map(|s| convert_markdown_to_html(&s)) {
                Ok(html) => Response::from_string(html)
                    .with_status_code(200)
                    .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                Err(e) => Response::from_string(format!("Error reading {}: {}", md_path.display(), e))
                    .with_status_code(500),
            }
        } else if org_path.exists() {
            match fs::read_to_string(&org_path).map(|s| convert_org_to_html(&s)) {
                Ok(html) => Response::from_string(html)
                    .with_status_code(200)
                    .with_header(Header::from_bytes(&b"Content-Type"[..], &b"text/html; charset=utf-8"[..]).unwrap()),
                Err(e) => Response::from_string(format!("Error reading {}: {}", org_path.display(), e))
                    .with_status_code(500),
            }
        } else {
            Response::from_string("Not Found").with_status_code(404)
        };

        let _ = request.respond(resp);
    }

    Ok(())
}

fn convert_file(path: &Path) -> Result<String> {
    let mut file = fs::File::open(path)
        .with_context(|| format!("opening input file {}", path.display()))?;
    let mut buf = String::new();
    file.read_to_string(&mut buf)
        .with_context(|| format!("reading input file {}", path.display()))?;

    match path.extension().and_then(|s| s.to_str()) {
        Some("md") => Ok(convert_markdown_to_html(&buf)),
        Some("org") => Ok(convert_org_to_html(&buf)),
        other => Err(anyhow!("unsupported extension {:?} for {}", other, path.display())),
    }
}

fn convert_markdown_to_html(input: &str) -> String {
    let mut options = Options::empty();
    options.insert(Options::ENABLE_TABLES);
    options.insert(Options::ENABLE_FOOTNOTES);
    options.insert(Options::ENABLE_STRIKETHROUGH);
    options.insert(Options::ENABLE_TASKLISTS);
    let parser = MdParser::new_ext(input, options);
    let mut out = String::new();
    html::push_html(&mut out, parser);
    let title = extract_title_from_markdown(input);
    wrap_html_page(out, title)
}

// Minimal Org-mode to HTML converter: supports headings, lists, paragraphs.
fn convert_org_to_html(input: &str) -> String {
    let org = Org::parse(input);
    let mut bytes: Vec<u8> = Vec::new();
    let _ = org.write_html(&mut bytes);
    let body = String::from_utf8(bytes).unwrap_or_default();
    let title = extract_title_from_org(input);
    wrap_html_page(body, title)
}

fn wrap_html_page(body: String, title: Option<String>) -> String {
    let css = default_css();
    let page_title = title.as_deref().unwrap_or("haystack");
    format!(
        "<!DOCTYPE html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n<meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{page_title}</title>\n<style>\n{css}\n</style>\n</head>\n<body>\n<main class=\"container\">\n{body}\n</main>\n</body>\n</html>",
    )
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
    r#":root { --fg: #1f2328; --bg: #ffffff; --muted: #667085; --link: #0a66c2; --border: #e5e7eb; --code-bg: #f6f8fa; }
@media (prefers-color-scheme: dark) {
  :root { --fg: #e6edf3; --bg: #0d1117; --muted: #9aa4b2; --link: #79b8ff; --border: #30363d; --code-bg: #161b22; }
}
html, body { padding: 0; margin: 0; background: var(--bg); color: var(--fg); }
body { font: 16px/1.65 system-ui, -apple-system, Segoe UI, Roboto, Ubuntu, Cantarell, Noto Sans, Helvetica, Arial, \"Apple Color Emoji\", \"Segoe UI Emoji\"; }
.container { max-width: 760px; margin: 0 auto; padding: 24px 16px; }

h1, h2, h3, h4, h5, h6 { line-height: 1.25; margin: 1.5em 0 0.6em; }
h1 { font-size: 2rem; }
h2 { font-size: 1.6rem; }
h3 { font-size: 1.25rem; }
h4 { font-size: 1.1rem; }
p { margin: 1em 0; }
a { color: var(--link); text-decoration: none; }
a:hover { text-decoration: underline; }
img, video { max-width: 100%; height: auto; }
hr { border: 0; border-top: 1px solid var(--border); margin: 2rem 0; }
ul, ol { padding-left: 1.25rem; }
li { margin: 0.3rem 0; }
blockquote { margin: 1rem 0; padding: 0.75rem 1rem; border-left: 3px solid var(--border); color: var(--muted); background: color-mix(in srgb, var(--code-bg) 60%, transparent); }
code, pre { font-family: ui-monospace, SFMono-Regular, Menlo, Monaco, Consolas, \"Liberation Mono\", \"Courier New\", monospace; font-size: 0.95em; }
pre { background: var(--code-bg); padding: 0.9rem; border-radius: 8px; overflow: auto; border: 1px solid var(--border); }
code { background: var(--code-bg); padding: 0.1rem 0.35rem; border-radius: 6px; }
pre code { padding: 0; background: transparent; }
table { width: 100%; border-collapse: collapse; margin: 1rem 0; }
th, td { padding: 0.5rem 0.6rem; border: 1px solid var(--border); text-align: left; }
thead th { background: color-mix(in srgb, var(--code-bg) 85%, transparent); }
details { border: 1px solid var(--border); border-radius: 8px; padding: 0.6rem 0.9rem; background: color-mix(in srgb, var(--code-bg) 75%, transparent); }
summary { cursor: pointer; font-weight: 600; }
@media (min-width: 900px) { body { font-size: 17px; } .container { padding: 32px 20px; } }
"#
}
