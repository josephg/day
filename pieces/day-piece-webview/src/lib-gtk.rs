// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

// ---------------------------------------------------------------------------
// GTK: WebKitGTK 6.0 via the `webkit6` crate — a `WebView` widget (a `gtk4::Widget`). Written blind
// (WebKitGTK isn't installed on the reference host); it builds+runs where `webkitgtk-6.0` is present
// (the CI gtk jobs install it). The `uri` property notify reports navigation back via
// `Event::custom("webview:url", …)`, matching the AppKit/Qt renderers. JavaScript evaluation
// rides `evaluate_javascript` and answers on the same channel keyed by request id
// (docs/webview-eval.md).
// ---------------------------------------------------------------------------

use super::*;
use day_gtk::Gtk;
use day_spec::NodeId;
use gtk4::prelude::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use webkit6::prelude::*;

/// Extract an inline site's tree from the GResource blob to the user cache dir, once per
/// process per root (docs/webview.md): WebKitGTK cannot browse a GResource, so the site becomes
/// loose files and the view loads a `file://` URL. Returns the extracted site root.
///
/// Called from `prepare_site()` (the checked, pre-warming route) and lazily from `make` (the
/// direct route). Synchronous — it runs inside a day task poll or at realize, not on a render
/// path; moving large-site extraction to a thread is the noted upgrade.
pub(crate) fn extract_site(root: &str) -> Result<std::path::PathBuf, String> {
    use std::cell::RefCell;
    day_core::tls_group! {
    static DONE: RefCell<std::collections::HashMap<String, std::path::PathBuf>> =
        RefCell::new(std::collections::HashMap::new());
    }
    if let Some(dir) = DONE.with(|m| m.borrow().get(root).cloned()) {
        return Ok(dir);
    }
    let app = gtk4::glib::prgname().unwrap_or_else(|| "day-app".into());
    let dest = gtk4::glib::user_cache_dir()
        .join("day-web")
        .join(app.as_str())
        .join(root);
    // Overwrite-extract once per process: stale caches from an older app build must not linger,
    // and the cost is one pass over a bundled site's files.
    let _ = std::fs::remove_dir_all(&dest);
    extract_tree(&format!("/day/assets/{root}"), &dest)?;
    DONE.with(|m| m.borrow_mut().insert(root.to_string(), dest.clone()));
    Ok(dest)
}

/// Recursively copy one GResource directory (`res_dir`, absolute resource path) into `dest`.
fn extract_tree(res_dir: &str, dest: &std::path::Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("mkdir {}: {e}", dest.display()))?;
    let children =
        gtk4::gio::resources_enumerate_children(res_dir, gtk4::gio::ResourceLookupFlags::NONE)
            .map_err(|e| format!("enumerate {res_dir}: {e}"))?;
    for child in children {
        let name = child.as_str();
        if let Some(dir_name) = name.strip_suffix('/') {
            extract_tree(&format!("{res_dir}/{dir_name}"), &dest.join(dir_name))?;
        } else {
            let bytes = gtk4::gio::resources_lookup_data(
                &format!("{res_dir}/{name}"),
                gtk4::gio::ResourceLookupFlags::NONE,
            )
            .map_err(|e| format!("read {res_dir}/{name}: {e}"))?;
            std::fs::write(dest.join(name), bytes.as_ref())
                .map_err(|e| format!("write {}: {e}", dest.join(name).display()))?;
        }
    }
    Ok(())
}

fn make(_backend: &mut Gtk, p: &WebProps, id: NodeId) -> gtk4::Widget {
    let wv = webkit6::WebView::new();
    let state = Rc::new(ViewState {
        node: id,
        base: Rc::new(RefCell::new(p.base_url.clone())),
    });
    VIEWS.with(|m| m.borrow_mut().insert(widget_key(&wv), state.clone()));
    // `try_with`: at process exit the tree (and so this widget) is dropped by day-core's
    // thread-local destructor, which can run after VIEWS is already gone — `with` would
    // panic inside a non-unwinding GTK trampoline and abort the process.
    wv.connect_destroy(|w| {
        let _ = VIEWS.try_with(|m| m.borrow_mut().remove(&widget_key(w)));
    });
    // Report the current URL back on every navigation so a bound text field follows.
    wv.connect_uri_notify(move |wv| {
        if let Some(uri) = wv.uri() {
            day_gtk::emit(id, Event::custom("webview:url", uri.to_string()));
        }
    });
    if !p.inline_root.is_empty() {
        // Inline mode (docs/webview.md): extract-to-cache (above), then a file URL — WebKit
        // resolves the site's relative references natively. The policy handler polices by the
        // canonical file-URL prefix; navigations leaving the site are IGNORED here and
        // reported, and the Rust front-end runs the app's LinkPolicy (events are enqueue-only,
        // so the verdict cannot come back through this signal).
        match extract_site(&p.inline_root) {
            Ok(dir) => {
                let dir = dir.canonicalize().unwrap_or(dir);
                let base = format!("file://{}/", dir.display());
                let start = format!("{base}{}", p.inline_start);
                let policed = base.clone();
                wv.connect_decide_policy(move |_wv, decision, dtype| {
                    use webkit6::PolicyDecisionType;
                    let uri = match dtype {
                        PolicyDecisionType::NavigationAction => decision
                            .downcast_ref::<webkit6::NavigationPolicyDecision>()
                            .and_then(|d| d.navigation_action())
                            .and_then(|a| a.request())
                            .and_then(|r| r.uri())
                            .map(|u| u.to_string()),
                        // target=_blank / window.open: no new window exists in day's tree —
                        // external by definition.
                        PolicyDecisionType::NewWindowAction => decision
                            .downcast_ref::<webkit6::NavigationPolicyDecision>()
                            .and_then(|d| d.navigation_action())
                            .and_then(|a| a.request())
                            .and_then(|r| r.uri())
                            .map(|u| u.to_string()),
                        _ => None,
                    };
                    let Some(uri) = uri else { return false };
                    let inside = uri.starts_with(&policed) || uri == "about:blank";
                    if inside && dtype == PolicyDecisionType::NavigationAction {
                        return false; // let WebKit proceed
                    }
                    decision.ignore();
                    day_gtk::emit(
                        id,
                        Event::Custom {
                            tag: "webview:link",
                            num: super::LINK_REPORT,
                            text: uri,
                        },
                    );
                    true
                });
                wv.load_uri(&start);
            }
            Err(e) => log::warn!("day-piece-webview: inline site {:?}: {e}", p.inline_root),
        }
    } else if p.doc_mode {
        // Document mode (docs/webview.md): the HTML is the page. Every main-frame navigation
        // the document starts is cancelled and reported — so a rendered email can never
        // navigate the reading pane away, not even to a sibling file under the base. Only
        // the document's own load (the base itself, or about:blank without one) and
        // fragment jumps within it proceed. The base lives in a cell the LoadHtml patch
        // updates, keyed by the widget and dropped with it.
        let base = state.base.clone();
        wv.connect_decide_policy(move |_wv, decision, dtype| {
            use webkit6::PolicyDecisionType;
            let uri = match dtype {
                PolicyDecisionType::NavigationAction | PolicyDecisionType::NewWindowAction => {
                    decision
                        .downcast_ref::<webkit6::NavigationPolicyDecision>()
                        .and_then(|d| d.navigation_action())
                        .and_then(|a| a.request())
                        .and_then(|r| r.uri())
                        .map(|u| u.to_string())
                }
                _ => None,
            };
            let Some(uri) = uri else { return false };
            // The borrow ends before `emit`: the app may answer the event synchronously
            // with a new LoadHtml patch, whose `update` writes this same cell.
            let inside = {
                let b = base.borrow();
                uri == "about:blank"
                    || (!b.is_empty()
                        && (uri == *b
                            || uri
                                .strip_prefix(b.as_str())
                                .is_some_and(|rest| rest.starts_with('#'))))
            };
            if inside && dtype == PolicyDecisionType::NavigationAction {
                return false;
            }
            decision.ignore();
            day_gtk::emit(
                id,
                Event::Custom {
                    tag: "webview:link",
                    num: super::LINK_REPORT,
                    text: uri,
                },
            );
            true
        });
        let base = p.base_url.clone();
        if !p.html.is_empty() {
            wv.load_html(
                &p.html,
                if base.is_empty() {
                    None
                } else {
                    Some(base.as_str())
                },
            );
        }
    } else if !p.url.is_empty() {
        wv.load_uri(&p.url);
    }
    wv.upcast()
}

/// What the arm keeps per live web view, keyed by widget address and dropped on `destroy`.
struct ViewState {
    /// The node this view reports to — `update` is handed only the widget, and an eval
    /// reply has to name the node it answers on.
    node: NodeId,
    /// Document mode's live base URL, so a LoadHtml patch can move it and the policy
    /// closure sees the move.
    base: Rc<RefCell<String>>,
}

thread_local! {
    static VIEWS: RefCell<HashMap<usize, Rc<ViewState>>> = RefCell::new(HashMap::new());
}

fn state_of(wv: &webkit6::WebView) -> Option<Rc<ViewState>> {
    VIEWS.with(|m| m.borrow().get(&widget_key(wv)).cloned())
}

/// Run `script` (already wrapped by the front-end, so it evaluates to a JS string) and
/// report `1␟<json>` / `0␟<name>␟<message>` back on `node`, keyed by `req`.
///
/// Errors here are WebKit's own: a thrown exception surfaces as `WebKitJavascriptError`
/// (`SCRIPT_FAILED`, message pre-formatted with `source_uri:line:col`) only when the
/// wrapper itself failed to run — a page whose CSP refuses `eval`, a dead web process. A
/// non-string value cannot come from the wrapper, so it is reported as an engine error too.
/// The callback always arrives (WebKit answers with `CANCELLED` if the view is destroyed
/// first), so nothing is left pending. Everything runs in the page's main world: the
/// wrapper uses `eval`, and an isolated world could not read page globals anyway.
fn eval(wv: &webkit6::WebView, node: NodeId, req: u64, script: &str) {
    wv.evaluate_javascript(
        script,
        None,
        Some("day-eval"),
        gtk4::gio::Cancellable::NONE,
        move |result| {
            let payload = match result {
                Ok(v) if v.is_string() => v.to_str().to_string(),
                Ok(_) => engine_error("WebKitError", "non-string reply"),
                Err(e) => engine_error("WebKitError", &e.to_string()),
            };
            day_gtk::emit(
                node,
                Event::Custom {
                    tag: "webview:eval",
                    num: req as f64,
                    text: payload,
                },
            );
        },
    );
}

fn widget_key(wv: &webkit6::WebView) -> usize {
    use gtk4::glib::prelude::ObjectType;
    wv.as_ptr() as usize
}

fn update(_backend: &mut Gtk, h: &gtk4::Widget, patch: &WebPatch) {
    let Some(wv) = h.downcast_ref::<webkit6::WebView>() else {
        return;
    };
    match patch {
        WebPatch::Eval { req, script } => {
            if let Some(state) = state_of(wv) {
                eval(wv, state.node, *req, script);
            }
        }
        WebPatch::LoadHtml { html, base } => {
            if let Some(state) = state_of(wv) {
                *state.base.borrow_mut() = base.clone();
            }
            wv.load_html(
                html,
                if base.is_empty() {
                    None
                } else {
                    Some(base.as_str())
                },
            );
        }
        WebPatch::Load(url) => wv.load_uri(url),
        WebPatch::Back => {
            if wv.can_go_back() {
                wv.go_back();
            }
        }
        WebPatch::Forward => {
            if wv.can_go_forward() {
                wv.go_forward();
            }
        }
        WebPatch::Stop => wv.stop_loading(),
        WebPatch::Reload => wv.reload(),
    }
}

day_pieces::renderer!(day_gtk::RENDERERS, Gtk,
    kind: KIND, props: WebProps, patch: WebPatch,
    make: make, update: update, measure: day_pieces::fill_measure);
