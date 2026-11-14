//! First-party transformer for links between documents.
//!
//! The collect pass records each document's title, the anchors it defines
//! (heading and section ids) and the documents it links to. The apply pass
//! then, for every document:
//!
//! - **Checks links and images.** A relative link to a file that does not
//!   exist under the source directory is reported as a `broken-link`
//!   warning; a `#fragment` naming no heading of the target document is a
//!   `broken-anchor` warning. External URLs are not checked.
//! - **Fills in cross-references.** A link with no text, such as
//!   `[](guide.md#setup)`, gets the title of the heading it points to, or of
//!   the page if it has no fragment.
//! - **Adds backlinks** (if enabled): a `custom:backlinks` node at the end of
//!   the document listing the pages that link to it.
//!
//! Options:
//!
//! | Option | Default | Meaning |
//! |---|---|---|
//! | `check` | `true` | Report broken links and anchors. |
//! | `backlinks` | `false` | Add the list of pages linking here. |
//! | `backlinks_title` | `"Pages linking here"` | Text shown above that list. |
//!
//! Links are resolved like the HTML renderer resolves them: relative to the
//! linking document, or to the source directory if they start with `/`.

use std::collections::{BTreeMap, BTreeSet};

use lsdocs_plugin_sdk::doctree::{
    Custom, Link, List, ListItem, Node, Paragraph, Strong, plain_text, text,
};
use lsdocs_plugin_sdk::protocol::{
    Diagnostic, Severity, TransformApplyParams, TransformApplyResult, TransformCollectParams,
    TransformCollectResult,
};
use lsdocs_plugin_sdk::{Context, Plugin, PluginResult, Transformer, relative_path};
use serde_json::{Value, json};

/// The plugin, as registered with the host.
pub fn plugin() -> Plugin {
    Plugin::new("transform-links", env!("CARGO_PKG_VERSION")).transformer(Links)
}

/// The links transformer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Links;

impl Transformer for Links {
    const COLLECTS: bool = true;

    async fn collect(
        &self,
        params: TransformCollectParams,
        _ctx: &Context,
    ) -> PluginResult<TransformCollectResult> {
        let doc = &params.document;
        let mut anchors = serde_json::Map::new();
        let mut links = BTreeSet::new();
        for node in doc.descendants() {
            match node {
                Node::Heading(h) => {
                    if let Some(id) = &h.id {
                        anchors.insert(id.clone(), json!(plain_text(&h.children)));
                    }
                }
                Node::Section(s) => {
                    if let Some(id) = &s.id {
                        anchors.entry(id.clone()).or_insert(json!(""));
                    }
                }
                Node::Link(l) => {
                    if let Target::Internal {
                        path: Some(path), ..
                    } = resolve(&l.url, &doc.source)
                        && path != doc.source
                    {
                        links.insert(path);
                    }
                }
                _ => {}
            }
        }
        Ok(TransformCollectResult {
            data: json!({ "title": doc.title().unwrap_or_else(|| doc.source.clone()), "anchors": anchors, "links": links }),
            diagnostics: vec![],
        })
    }

    async fn apply(
        &self,
        params: TransformApplyParams,
        ctx: &Context,
    ) -> PluginResult<TransformApplyResult> {
        let options = ctx.options();
        let flag = |name, default| {
            options
                .get(name)
                .and_then(Value::as_bool)
                .unwrap_or(default)
        };
        let site = Site {
            files: ctx.site().files.iter().map(String::as_str).collect(),
            pages: params.collected.as_object(),
        };
        let mut document = params.document;
        let mut diagnostics = Vec::new();
        let source = document.source.clone();
        visit(&mut document.children, &mut |node| {
            site.link(node, &source, flag("check", true), &mut diagnostics)
        });
        if flag("backlinks", false) {
            let heading = options
                .get("backlinks_title")
                .and_then(Value::as_str)
                .unwrap_or("Pages linking here");
            if let Some(node) = site.backlinks(&source, heading) {
                document.children.push(node);
            }
        }
        Ok(TransformApplyResult {
            document,
            diagnostics,
        })
    }
}

/// What the apply pass knows about the whole site.
struct Site<'a> {
    /// Every source file.
    files: BTreeSet<&'a str>,
    /// Collected data of every document, by source path.
    pages: Option<&'a serde_json::Map<String, Value>>,
}

impl Site<'_> {
    fn page(&self, source: &str) -> Option<&Value> {
        self.pages?.get(source)
    }

    fn exists(&self, path: &str) -> bool {
        if self.page(path).is_some() || self.files.contains(path) {
            return true;
        }
        // A directory link such as `guide/` is fine if anything is inside.
        path.ends_with('/') && self.files.iter().any(|f| f.starts_with(path))
    }

    /// Checks one link or image, and fills in an empty link's text.
    fn link(&self, node: &mut Node, source: &str, check: bool, out: &mut Vec<Diagnostic>) {
        let (url, span, is_image) = match node {
            Node::Link(l) => (l.url.as_str(), l.span, false),
            Node::Image(i) => (i.url.as_str(), i.span, true),
            _ => return,
        };
        let warn = |out: &mut Vec<Diagnostic>, code: &str, message: String| {
            out.push(Diagnostic {
                severity: Severity::Warning,
                message,
                span,
                code: Some(code.into()),
                ..Diagnostic::default()
            });
        };
        let (path, fragment) = match resolve(url, source) {
            Target::External => return,
            Target::Outside => {
                if check {
                    warn(
                        out,
                        "broken-link",
                        format!("`{url}` points outside the site"),
                    );
                }
                return;
            }
            Target::Internal { path, fragment } => {
                (path.unwrap_or_else(|| source.to_owned()), fragment)
            }
        };
        // Without a file list (an older host), only documents are known.
        let checkable = !self.files.is_empty() || self.pages.is_some();
        if check && checkable && !self.exists(&path) {
            let what = if is_image { "image" } else { "link target" };
            warn(
                out,
                "broken-link",
                format!("{what} `{path}` does not exist"),
            );
            return;
        }
        let page = self.page(&path);
        let anchor_title = match (&fragment, page) {
            (Some(fragment), Some(page)) => match page["anchors"].get(fragment) {
                Some(title) => title.as_str(),
                None => {
                    if check {
                        warn(
                            out,
                            "broken-anchor",
                            format!("`{path}` has no heading `#{fragment}`"),
                        );
                    }
                    return;
                }
            },
            _ => None,
        };
        // Cross-reference: an empty link takes its target's title.
        if let Node::Link(link) = node
            && link.children.is_empty()
        {
            let title = match &fragment {
                Some(_) => anchor_title,
                None => page.and_then(|p| p["title"].as_str()),
            };
            if let Some(title) = title.filter(|t| !t.is_empty()) {
                link.children.push(text(title));
            }
        }
    }

    /// A `custom:backlinks` node listing the documents that link to
    /// `source`, or `None` if none do.
    fn backlinks(&self, source: &str, heading: &str) -> Option<Node> {
        let items: Vec<Node> = self
            .pages?
            .iter()
            .filter(|(from, page)| {
                *from != source
                    && page["links"]
                        .as_array()
                        .is_some_and(|links| links.iter().any(|l| l == source))
            })
            .map(|(from, page)| {
                let title = page["title"].as_str().unwrap_or(from);
                ListItem {
                    children: vec![Node::Paragraph(Paragraph {
                        children: vec![Node::Link(Link {
                            url: relative_path(source, from),
                            children: vec![text(title)],
                            ..Link::default()
                        })],
                        ..Paragraph::default()
                    })],
                    ..ListItem::default()
                }
                .into()
            })
            .collect();
        if items.is_empty() {
            return None;
        }
        Some(Node::Custom(Custom {
            kind: "backlinks".into(),
            attrs: BTreeMap::new(),
            children: vec![
                Node::Paragraph(Paragraph {
                    children: vec![Node::Strong(Strong {
                        children: vec![text(heading)],
                        ..Strong::default()
                    })],
                    ..Paragraph::default()
                }),
                Node::List(List {
                    children: items,
                    ..List::default()
                }),
            ],
            span: None,
        }))
    }
}

/// Calls `f` on every node, parents before children.
fn visit(nodes: &mut [Node], f: &mut impl FnMut(&mut Node)) {
    for node in nodes {
        f(node);
        if let Some(children) = node.children_mut() {
            visit(children, f);
        }
    }
}

/// Where a link points.
#[derive(Debug, PartialEq, Eq)]
enum Target {
    /// Another site, or a non-file scheme such as `mailto:`.
    External,
    /// Above the source directory.
    Outside,
    /// A file in the site, by its path relative to the source directory
    /// (`None` for the linking document itself), and an optional fragment.
    Internal {
        path: Option<String>,
        fragment: Option<String>,
    },
}

/// Resolves `url`, found in the document `from`.
fn resolve(url: &str, from: &str) -> Target {
    let has_scheme = url.split_once(':').is_some_and(|(scheme, _)| {
        !scheme.is_empty()
            && scheme
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if has_scheme || url.starts_with("//") {
        return Target::External;
    }
    let (rest, fragment) = match url.split_once('#') {
        Some((rest, fragment)) => (rest, Some(percent_decode(fragment))),
        None => (url, None),
    };
    let fragment = fragment.filter(|f| !f.is_empty());
    let path = rest.split_once('?').map_or(rest, |(path, _)| path);
    if path.is_empty() {
        return Target::Internal {
            path: None,
            fragment,
        };
    }
    let path = percent_decode(path);
    let (base, path) = match path.strip_prefix('/') {
        Some(rooted) => ("", rooted),
        None => (
            from.rsplit_once('/').map_or("", |(dir, _)| dir),
            path.as_str(),
        ),
    };
    let mut parts: Vec<&str> = base.split('/').filter(|p| !p.is_empty()).collect();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.pop().is_none() {
                    return Target::Outside;
                }
            }
            part => parts.push(part),
        }
    }
    let mut joined = parts.join("/");
    if path.ends_with('/') && !joined.is_empty() {
        joined.push('/');
    }
    Target::Internal {
        path: Some(joined),
        fragment,
    }
}

/// Decodes `%XX` escapes; malformed escapes are kept as they are.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && let Some(hex) = s.get(i + 1..i + 3)
            && let Ok(byte) = u8::from_str_radix(hex, 16)
        {
            out.push(byte);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsdocs_plugin_sdk::doctree::{Document, Heading, Image, Span};
    use lsdocs_plugin_sdk::protocol::{InitializeParams, SiteInfo};
    use lsdocs_plugin_sdk::testing;

    fn internal(path: Option<&str>, fragment: Option<&str>) -> Target {
        Target::Internal {
            path: path.map(Into::into),
            fragment: fragment.map(Into::into),
        }
    }

    #[test]
    fn resolving() {
        assert_eq!(resolve("https://x.org/a.md", "a.md"), Target::External);
        assert_eq!(resolve("//cdn/x", "a.md"), Target::External);
        assert_eq!(resolve("mailto:me@x.org", "a.md"), Target::External);
        assert_eq!(resolve("#top", "g/a.md"), internal(None, Some("top")));
        assert_eq!(resolve("?q=1", "g/a.md"), internal(None, None));
        assert_eq!(resolve("b.md", "g/a.md"), internal(Some("g/b.md"), None));
        assert_eq!(
            resolve("../c/d.md?x#Sec%20One", "g/a.md"),
            internal(Some("c/d.md"), Some("Sec One"))
        );
        assert_eq!(
            resolve("/img/x.svg", "g/h/a.md"),
            internal(Some("img/x.svg"), None)
        );
        assert_eq!(
            resolve("./my%20file.md", "a.md"),
            internal(Some("my file.md"), None)
        );
        assert_eq!(resolve("sub/", "a.md"), internal(Some("sub/"), None));
        assert_eq!(resolve("../x.md", "a.md"), Target::Outside);
    }

    fn heading(level: u8, id: &str, title: &str) -> Node {
        Node::Heading(Heading {
            level,
            id: Some(id.into()),
            children: vec![text(title)],
            ..Heading::default()
        })
    }

    fn link(url: &str, label: &str, at: u32) -> Node {
        Node::Paragraph(Paragraph {
            children: vec![Node::Link(Link {
                span: Some(Span::new(at, at + 1)),
                url: url.into(),
                children: if label.is_empty() {
                    vec![]
                } else {
                    vec![text(label)]
                },
                ..Link::default()
            })],
            ..Paragraph::default()
        })
    }

    fn site() -> Vec<Document> {
        let mut index = Document::new("index.md");
        index.children = vec![
            heading(1, "home", "Home"),
            link("guide/setup.md", "setup", 1),
            link("guide/setup.md#install", "", 2),
            link("missing.md", "gone", 3),
            link("guide/setup.md#nope", "bad anchor", 4),
            link("#home", "", 5),
            link("https://example.com/missing.md", "ext", 6),
            Node::Image(Image {
                span: Some(Span::new(7, 8)),
                url: "img/none.png".into(),
                ..Image::default()
            }),
        ];
        let mut setup = Document::new("guide/setup.md");
        setup.metadata.insert("title".into(), json!("Setup"));
        setup.children = vec![
            heading(1, "setup", "Set up"),
            heading(2, "install", "Installing"),
            link("../index.md", "", 1),
            link("../img/logo.svg", "logo", 2),
        ];
        vec![index, setup]
    }

    async fn build(options: Value) -> Vec<(Document, Vec<Diagnostic>)> {
        let plugin = plugin();
        let init = InitializeParams {
            site: SiteInfo {
                files: ["guide/setup.md", "img/logo.svg", "index.md"]
                    .map(String::from)
                    .to_vec(),
                ..SiteInfo::default()
            },
            options,
            ..InitializeParams::default()
        };
        testing::run_transform(&plugin, &init, site())
            .await
            .unwrap()
    }

    fn texts(doc: &Document) -> Vec<String> {
        doc.descendants()
            .filter_map(|n| match n {
                Node::Link(l) => Some(format!("{}={}", l.url, plain_text(&l.children))),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn checks_links_and_fills_cross_references() {
        let out = build(Value::Null).await;
        let (index, diagnostics) = &out[0];
        assert_eq!(
            texts(index),
            [
                "guide/setup.md=setup",
                "guide/setup.md#install=Installing",
                "missing.md=gone",
                "guide/setup.md#nope=bad anchor",
                "#home=Home",
                "https://example.com/missing.md=ext",
            ]
        );
        let found: Vec<_> = diagnostics
            .iter()
            .map(|d| {
                format!(
                    "{}@{}: {}",
                    d.code.as_deref().unwrap(),
                    d.span.unwrap().start,
                    d.message
                )
            })
            .collect();
        assert_eq!(
            found,
            [
                "broken-link@3: link target `missing.md` does not exist",
                "broken-anchor@4: `guide/setup.md` has no heading `#nope`",
                "broken-link@7: image `img/none.png` does not exist",
            ]
        );
        let (setup, diagnostics) = &out[1];
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(texts(setup), ["../index.md=Home", "../img/logo.svg=logo"]);
        assert!(
            !setup
                .children
                .iter()
                .any(|n| n.kind() == "custom:backlinks")
        );
    }

    #[tokio::test]
    async fn backlinks_list_linking_pages() {
        let out = build(json!({"backlinks": true, "check": false})).await;
        let (index, diagnostics) = &out[0];
        assert!(diagnostics.is_empty());
        let (setup, _) = &out[1];
        let Some(Node::Custom(backlinks)) = setup.children.last() else {
            panic!("{setup:?}")
        };
        assert_eq!(backlinks.kind, "backlinks");
        assert_eq!(plain_text(&backlinks.children[..1]), "Pages linking here");
        assert_eq!(texts(setup).last().unwrap(), "../index.md=Home");
        // The home page is linked from setup too.
        let Some(Node::Custom(back)) = index.children.last() else {
            panic!()
        };
        assert_eq!(plain_text(&back.children[1..]), "Setup");
    }
}
