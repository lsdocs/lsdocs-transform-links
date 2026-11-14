//! First-party transformer for links between documents.
//!
//! The collect pass records each document's title, the anchors it defines
//! (ids of sections, headings, figures and `anchor` nodes) and the
//! documents it links to. The apply pass then, for every document:
//!
//! - **Resolves labels.** A link whose `target` is a `label` gets the
//!   document defining that anchor (`broken-label` if none does,
//!   `ambiguous-label` if several do; the first in source order wins).
//! - **Checks links and images.** A link to a file that does not exist under
//!   the source directory is reported as a `broken-link` warning; an anchor
//!   the target document does not define is a `broken-anchor` warning.
//!   External URLs are not checked.
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
//! A link's `target`, set by the host or the parser, says where it points;
//! links without one (and images) are resolved from their URL, relative to
//! the linking document or, if they start with `/`, to the source
//! directory.

use std::collections::{BTreeMap, BTreeSet};

use lsdocs_plugin_sdk::doctree::{
    Custom, Link, LinkTarget, List, ListItem, Node, Paragraph, Strong, plain_text, text,
};
use lsdocs_plugin_sdk::links::{UrlTarget as Target, resolve_url as resolve};
use lsdocs_plugin_sdk::protocol::{
    Diagnostic, Severity, TransformApplyParams, TransformApplyResult, TransformCollectParams,
    TransformCollectResult,
};
use lsdocs_plugin_sdk::{Context, Plugin, PluginResult, Transformer, relative_path};
use serde_json::{Value, json};

/// The plugin, as registered with the host.
pub fn plugin() -> Plugin {
    Plugin::new("transform-links", env!("CARGO_PKG_VERSION"))
        .transformer(Links)
        .cacheable()
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
            // Anchors, with the title a cross-reference to them shows.
            let anchor = match node {
                Node::Section(s) => s.id.as_ref().map(|id| {
                    let title = match s.children.first() {
                        Some(Node::Heading(h)) => plain_text(&h.children),
                        _ => String::new(),
                    };
                    (id, title)
                }),
                Node::Heading(h) => h.id.as_ref().map(|id| (id, plain_text(&h.children))),
                Node::Figure(f) => f.id.as_ref().map(|id| (id, String::new())),
                Node::Anchor(a) => Some((&a.id, String::new())),
                _ => None,
            };
            if let Some((id, title)) = anchor {
                anchors.entry(id.clone()).or_insert(json!(title));
            }
            if let Node::Link(l) = node {
                let path = match &l.target {
                    Some(target) => target.document.clone(),
                    None => match resolve(&l.url, &doc.source) {
                        Target::Internal { path, .. } => path,
                        _ => None,
                    },
                };
                if let Some(path) = path.filter(|p| *p != doc.source) {
                    links.insert(path);
                }
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

    /// The documents defining the anchor `label`, in source order.
    fn label(&self, label: &str) -> Vec<&str> {
        self.pages
            .into_iter()
            .flatten()
            .filter(|(_, page)| page["anchors"].get(label).is_some())
            .map(|(source, _)| source.as_str())
            .collect()
    }

    /// Checks one link or image, resolves a link's label, and fills in an
    /// empty link's text.
    fn link(&self, node: &mut Node, source: &str, check: bool, out: &mut Vec<Diagnostic>) {
        let span = node.span();
        let warn = |out: &mut Vec<Diagnostic>, code: &str, message: String| {
            out.push(Diagnostic {
                severity: Severity::Warning,
                message,
                span,
                code: Some(code.into()),
                ..Diagnostic::default()
            });
        };
        // A label names an anchor anywhere in the site.
        if let Node::Link(link) = node
            && let Some(target) = &mut link.target
            && let Some(label) = target.label.clone()
        {
            let found = self.label(&label);
            match found.as_slice() {
                [] => {
                    if check {
                        warn(out, "broken-label", format!("no anchor is named `{label}`"));
                    }
                    return;
                }
                [_, ..] => {
                    if found.len() > 1 && check {
                        warn(
                            out,
                            "ambiguous-label",
                            format!(
                                "`{label}` is defined in {}; using `{}`",
                                found.join(", "),
                                found[0]
                            ),
                        );
                    }
                    target.document = Some(found[0].to_owned());
                    target.anchor = Some(label);
                    target.label = None;
                }
            }
        }
        let (url, target, is_image) = match node {
            Node::Link(l) => (l.url.as_str(), l.target.as_ref(), false),
            Node::Image(i) => (i.url.as_str(), None, true),
            _ => return,
        };
        let (path, fragment) = match target {
            Some(target) => (
                target.document.clone().unwrap_or_else(|| source.to_owned()),
                target.anchor.clone(),
            ),
            None => match resolve(url, source) {
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
            },
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
                            format!("`{path}` has no anchor `#{fragment}`"),
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
                            target: Some(LinkTarget {
                                document: Some(from.clone()),
                                ..LinkTarget::default()
                            }),
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

#[cfg(test)]
mod tests {
    use super::*;
    use lsdocs_plugin_sdk::doctree::{Document, Heading, Image, Span};
    use lsdocs_plugin_sdk::protocol::{InitializeParams, SiteInfo};
    use lsdocs_plugin_sdk::testing;

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
                "broken-anchor@4: `guide/setup.md` has no anchor `#nope`",
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
    async fn targets_and_labels() {
        use lsdocs_plugin_sdk::doctree::{Anchor, LinkTarget, Section};
        let target_link = |at: u32, target: LinkTarget| {
            Node::Paragraph(Paragraph {
                children: vec![Node::Link(Link {
                    span: Some(Span::new(at, at + 1)),
                    // The URL is not used when there is a target.
                    url: "ignored.md".into(),
                    target: Some(target),
                    ..Link::default()
                })],
                ..Paragraph::default()
            })
        };
        let mut a = Document::new("a.rst");
        a.children = vec![
            target_link(
                1,
                LinkTarget {
                    label: Some("intro".into()),
                    ..LinkTarget::default()
                },
            ),
            target_link(
                2,
                LinkTarget {
                    document: Some("b.rst".into()),
                    anchor: Some("intro".into()),
                    label: None,
                },
            ),
            target_link(
                3,
                LinkTarget {
                    label: Some("nowhere".into()),
                    ..LinkTarget::default()
                },
            ),
            target_link(
                4,
                LinkTarget {
                    label: Some("dup".into()),
                    ..LinkTarget::default()
                },
            ),
            Node::Anchor(Anchor {
                id: "dup".into(),
                ..Anchor::default()
            }),
        ];
        let mut b = Document::new("b.rst");
        b.children = vec![
            Node::Section(Section {
                id: Some("intro".into()),
                children: vec![heading(1, "", "Introduction")],
                ..Section::default()
            }),
            Node::Anchor(Anchor {
                id: "dup".into(),
                ..Anchor::default()
            }),
        ];
        let init = InitializeParams {
            site: SiteInfo {
                files: vec!["a.rst".into(), "b.rst".into()],
                ..SiteInfo::default()
            },
            ..InitializeParams::default()
        };
        let mut out = testing::run_transform(&plugin(), &init, vec![a, b])
            .await
            .unwrap();
        let (a, diagnostics) = out.remove(0);
        let links: Vec<_> = a
            .descendants()
            .filter_map(|n| match n {
                Node::Link(l) => Some((l.target.clone().unwrap(), plain_text(&l.children))),
                _ => None,
            })
            .collect();
        let resolved = LinkTarget {
            document: Some("b.rst".into()),
            anchor: Some("intro".into()),
            label: None,
        };
        // The label is resolved, and the section's title fills in both.
        assert_eq!(links[0], (resolved.clone(), "Introduction".into()));
        assert_eq!(links[1], (resolved, "Introduction".into()));
        assert_eq!(links[3].0.document.as_deref(), Some("a.rst"));
        let codes: Vec<_> = diagnostics
            .iter()
            .map(|d| format!("{}@{}", d.code.as_deref().unwrap(), d.span.unwrap().start))
            .collect();
        assert_eq!(codes, ["broken-label@3", "ambiguous-label@4"]);
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
