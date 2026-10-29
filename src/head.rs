use regex::Regex;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::OnceLock;

#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct Head {
    pub title: Option<String>,
    pub description: Option<String>,
    pub path: Option<String>,
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub image: Option<String>,
    pub published: Option<String>,
    pub modified: Option<String>,
    pub noindex: Option<bool>,
    pub json_ld: Option<Value>,
    pub raw: Option<String>,
}

pub struct Site<'a> {
    pub name: &'a str,
    pub description: &'a str,
    pub default_image: &'a str,
    pub origin: &'a str,
}

pub fn escape_html(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;")
}

pub fn script_json(v: &Value) -> String {
    v.to_string().replace('<', "\\u003c").replace('>', "\\u003e").replace('&', "\\u0026").replace('\u{2028}', "\\u2028").replace('\u{2029}', "\\u2029")
}

pub fn absolute(origin: &str, path: &str) -> String {
    if path.starts_with("http://") || path.starts_with("https://") {
        path.to_string()
    } else {
        format!("{}{}", origin.trim_end_matches('/'), if path.starts_with('/') { path.to_string() } else { format!("/{path}") })
    }
}

fn absolutize_json(origin: &str, v: &mut Value) {
    match v {
        Value::Object(map) => {
            for (k, val) in map.iter_mut() {
                if matches!(k.as_str(), "url" | "item" | "thumbnailUrl" | "contentUrl" | "image" | "mainEntityOfPage") {
                    if let Value::String(s) = val {
                        if s.starts_with('/') {
                            *s = absolute(origin, s);
                        }
                    }
                }
                absolutize_json(origin, val);
            }
        }
        Value::Array(items) => items.iter_mut().for_each(|i| absolutize_json(origin, i)),
        _ => {}
    }
}

fn meta(out: &mut Vec<String>, kind: &str, name: &str, content: &str) {
    out.push(format!("<meta {kind}=\"{name}\" content=\"{}\" />", escape_html(content)));
}

pub fn head_tags(head: &Head, site: &Site, status: u16) -> String {
    if let Some(raw) = &head.raw {
        return raw.clone();
    }
    let title = match &head.title {
        Some(t) if !t.is_empty() => format!("{t} | {}", site.name),
        _ => site.name.to_string(),
    };
    let description = head.description.as_deref().map(str::trim).filter(|d| !d.is_empty()).unwrap_or(site.description);
    let og_title = head.title.clone().filter(|t| !t.is_empty()).unwrap_or_else(|| site.name.to_string());
    let noindex = head.noindex.unwrap_or(false) || status >= 400;
    let mut out = vec![format!("<title>{}</title>", escape_html(&title))];
    meta(&mut out, "name", "description", description);
    meta(&mut out, "name", "robots", if noindex { "noindex, follow" } else { "index, follow, max-image-preview:large" });
    let kind = head.kind.clone().unwrap_or_else(|| "website".into());
    meta(&mut out, "property", "og:type", &kind);
    meta(&mut out, "property", "og:site_name", site.name);
    meta(&mut out, "property", "og:title", &og_title);
    meta(&mut out, "property", "og:description", description);
    let image = head.image.as_deref().filter(|i| !i.is_empty()).map(|i| absolute(site.origin, i)).unwrap_or_else(|| absolute(site.origin, site.default_image));
    meta(&mut out, "property", "og:image", &image);
    meta(&mut out, "name", "twitter:card", "summary_large_image");
    meta(&mut out, "name", "twitter:title", &og_title);
    meta(&mut out, "name", "twitter:description", description);
    meta(&mut out, "name", "twitter:image", &image);
    let canonical = head.path.as_deref().filter(|_| !noindex).map(|p| absolute(site.origin, p));
    if let Some(url) = &canonical {
        meta(&mut out, "property", "og:url", url);
        out.push(format!("<link rel=\"canonical\" href=\"{}\" />", escape_html(url)));
    }
    if kind == "article" {
        if let Some(p) = &head.published {
            meta(&mut out, "property", "article:published_time", p);
        }
        if let Some(m) = &head.modified {
            meta(&mut out, "property", "article:modified_time", m);
        }
    }
    if !noindex {
        if let Some(ld) = json_ld(head, site, &og_title, description, &kind) {
            out.push(format!("<script type=\"application/ld+json\">{}</script>", script_json(&ld)));
        }
    }
    out.join("\n    ")
}

fn json_ld(head: &Head, site: &Site, title: &str, description: &str, kind: &str) -> Option<Value> {
    if let Some(custom) = &head.json_ld {
        let mut ld = custom.clone();
        absolutize_json(site.origin, &mut ld);
        return Some(ld);
    }
    let path = head.path.as_deref()?;
    let url = absolute(site.origin, path);
    if path == "/" {
        return Some(json!({
            "@context": "https://schema.org",
            "@type": "WebSite",
            "name": site.name,
            "url": url,
            "description": description,
            "potentialAction": {
                "@type": "SearchAction",
                "target": format!("{}/search?q={{search_term_string}}", site.origin.trim_end_matches('/')),
                "query-input": "required name=search_term_string"
            }
        }));
    }
    if kind == "article" {
        let mut ld = json!({
            "@context": "https://schema.org",
            "@type": "CreativeWork",
            "name": title,
            "description": description,
            "url": url,
            "publisher": { "@type": "Organization", "name": site.name }
        });
        if let Some(p) = &head.published {
            ld["datePublished"] = json!(p);
        }
        return Some(ld);
    }
    Some(json!({ "@context": "https://schema.org", "@type": "WebPage", "name": title, "description": description, "url": url }))
}

fn stripped_tags() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r#"[ \t]*(?:<title>[^<]*</title>|"#,
            r#"<meta\s+(?:name|property)="(?:description|robots|og:[a-z:_]+|article:[a-z_]+|twitter:[a-z:_]+)"[^>]*/?>|"#,
            r#"<link\s+rel="canonical"[^>]*/?>)[ \t]*\r?\n?"#
        ))
        .unwrap()
    })
}

fn overridden(head: &str) -> Vec<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| Regex::new(r#"<meta\s[^>]*?(?:name|property)="([^"]+)"|<(title)[\s>]|<link\s[^>]*?rel="(canonical)""#).unwrap());
    re.captures_iter(head).filter_map(|c| (1..=3).find_map(|i| c.get(i)).map(|m| m.as_str().to_string())).collect()
}

fn strip_overridden(shell: &str, head: &str) -> String {
    let names = overridden(head);
    let re = stripped_tags();
    re.replace_all(shell, |c: &regex::Captures| {
        let tag = c.get(0).unwrap().as_str();
        let hit = if tag.contains("<title>") {
            names.iter().any(|n| n == "title")
        } else if tag.contains("canonical") {
            names.iter().any(|n| n == "canonical")
        } else {
            names.iter().any(|n| tag.contains(&format!("=\"{n}\"")))
        };
        if hit { String::new() } else { tag.to_string() }
    })
    .into_owned()
}

pub fn assemble_raw(shell: &str, body: &str, head: &str, data: &Value) -> String {
    assemble_with(strip_overridden(shell, head), body, head, data)
}

pub fn assemble(shell: &str, body: &str, head: &str, data: &Value) -> String {
    assemble_with(stripped_tags().replace_all(shell, "").into_owned(), body, head, data)
}

fn assemble_with(shell: String, body: &str, head: &str, data: &Value) -> String {
    let mut page = shell;
    if let Some(i) = page.find("</head>") {
        page.insert_str(i, &format!("    {head}\n  "));
    }
    let marker = "<!--app-html-->";
    page = if page.contains(marker) { page.replacen(marker, body, 1) } else { page.replacen("<div id=\"root\"></div>", &format!("<div id=\"root\">{body}</div>"), 1) };
    let data_tag = format!("<script id=\"ssr-data\" type=\"application/json\">{}</script>\n  ", script_json(data));
    match page.find("</body>") {
        Some(i) => {
            page.insert_str(i, &data_tag);
            page
        }
        None => page + &data_tag,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHELL: &str = "<!doctype html>\n<html>\n  <head>\n    <meta charset=\"utf-8\" />\n    <title>Old</title>\n    <meta name=\"description\" content=\"old\" />\n    <meta property=\"og:url\" content=\"old\" />\n    <meta name=\"og:title\" content=\"old\" />\n    <meta property=\"og:image:width\" content=\"1200\" />\n    <meta name=\"twitter:card\" content=\"x\" />\n    <link rel=\"canonical\" href=\"https://old/\" />\n  </head>\n  <body>\n    <div id=\"root\"><!--app-html--></div>\n    <script type=\"module\" src=\"/assets/a.js\"></script>\n  </body>\n</html>";

    fn site() -> Site<'static> {
        Site { name: "DTAN.XYZ", description: "Torrents on Nostr", default_image: "/banner.jpg", origin: "https://dtan.xyz" }
    }

    #[test]
    fn the_shell_gets_the_rendered_body_head_tags_and_data_once() {
        let head = Head { title: Some("Ubuntu".into()), description: Some("A \"q\" & <t>".into()), path: Some("/e/nevent1x".into()), kind: Some("article".into()), published: Some("2026-01-02T00:00:00Z".into()), ..Default::default() };
        let tags = head_tags(&head, &site(), 200);
        let html = assemble(SHELL, "<main>hi</main>", &tags, &json!({ "q": "</script><b>" }));
        assert_eq!(html.matches("<title>").count(), 1);
        assert!(html.contains("<title>Ubuntu | DTAN.XYZ</title>"));
        assert!(!html.contains("old"));
        assert!(!html.contains("og:image:width"));
        assert_eq!(html.matches("twitter:card").count(), 1);
        assert!(html.contains("content=\"A &quot;q&quot; &amp; &lt;t&gt;\""));
        assert!(html.contains("<link rel=\"canonical\" href=\"https://dtan.xyz/e/nevent1x\" />"));
        assert!(html.contains("<meta property=\"article:published_time\" content=\"2026-01-02T00:00:00Z\" />"));
        assert!(html.contains("<div id=\"root\"><main>hi</main></div>"));
        assert_eq!(html.matches("id=\"ssr-data\"").count(), 1);
        assert!(!html.contains("</script><b>"));
        assert!(html.contains("<script type=\"module\" src=\"/assets/a.js\"></script>"));
    }

    #[test]
    fn a_plain_root_div_is_filled_when_there_is_no_marker() {
        let shell = "<html><head></head><body><div id=\"root\"></div></body></html>";
        let html = assemble(shell, "<p>x</p>", "", &json!({}));
        assert!(html.contains("<div id=\"root\"><p>x</p></div>"));
    }

    #[test]
    fn indexable_pages_carry_json_ld_and_noindex_pages_do_not() {
        let home = Head { path: Some("/".into()), ..Default::default() };
        let tags = head_tags(&home, &site(), 200);
        assert!(tags.contains("\"@type\":\"WebSite\"") && tags.contains("SearchAction"));
        let login = Head { path: Some("/login".into()), noindex: Some(true), ..Default::default() };
        assert!(!head_tags(&login, &site(), 200).contains("ld+json"));
    }

    #[test]
    fn custom_json_ld_is_made_absolute_and_cannot_close_the_script() {
        let head = Head { path: Some("/e/a".into()), json_ld: Some(json!({"@type": "VideoObject", "url": "/e/a", "name": "</script>"})), ..Default::default() };
        let tags = head_tags(&head, &site(), 200);
        let ld = tags.split("application/ld+json\">").nth(1).unwrap().split("</script>").next().unwrap();
        let parsed: Value = serde_json::from_str(ld).unwrap();
        assert_eq!(parsed["url"], "https://dtan.xyz/e/a");
        assert_eq!(parsed["name"], "</script>");
    }

    #[test]
    fn raw_head_html_from_the_bundle_replaces_the_shell_tags_untouched() {
        let head = Head { raw: Some("<title>Stream - zap.stream</title>\n    <meta property=\"og:type\" content=\"video\" />".into()), ..Default::default() };
        let tags = head_tags(&head, &site(), 200);
        let html = assemble_raw(SHELL, "", &tags, &json!({}));
        assert_eq!(html.matches("<title>").count(), 1);
        assert!(html.contains("<title>Stream - zap.stream</title>"));
        assert!(html.contains("og:type\" content=\"video\""));
        assert!(html.contains("rel=\"canonical\" href=\"https://old/\""));
        assert!(html.contains("name=\"description\" content=\"old\""));
        assert!(!html.contains("<title>Old"));
        assert!(html.contains("og:url\" content=\"old\""));
    }

    #[test]
    fn helmet_tags_with_data_attributes_still_replace_the_shell_tags() {
        let raw = "<title data-rh=\"true\">Stream - zap.stream</title>\n<meta data-rh=\"true\" property=\"og:title\" content=\"Stream\"/>\n<link data-rh=\"true\" rel=\"canonical\" href=\"https://x/s\"/>";
        let head = Head { raw: Some(raw.into()), ..Default::default() };
        let html = assemble_raw(SHELL, "", &head_tags(&head, &site(), 200), &json!({}));
        assert_eq!(html.matches("og:title").count(), 1);
        assert_eq!(html.matches("<title").count(), 1);
        assert_eq!(html.matches("rel=\"canonical\"").count(), 1);
        assert!(html.contains("content=\"Stream\""));
        assert!(html.contains("og:url\" content=\"old\""));
    }

    #[test]
    fn error_pages_are_noindex_without_canonical() {
        let tags = head_tags(&Head { path: Some("/x".into()), ..Default::default() }, &site(), 404);
        assert!(tags.contains("noindex") && !tags.contains("canonical") && !tags.contains("og:url"));
    }
}
