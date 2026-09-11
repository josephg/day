// Copyright © The Daybrite Project
// SPDX-License-Identifier: MPL-2.0

// ---------------------------------------------------------------------------
// UIKit: WKWebView (WebKit) — the same control as AppKit, but a UIView subclass on iOS. objc2-web-kit
// 0.3 only generates the macOS (NSView) WKWebView binding, so here we hand-roll the iOS class via
// `extern_class!` + `msg_send!`. A navigation delegate reports the committed URL back through
// `Event::custom("webview:url", …)`; retained in a thread_local (WKWebView keeps the delegate weakly).
// ---------------------------------------------------------------------------

use super::*;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;

use block2::RcBlock;
use day_spec::NodeId;
use day_uikit::Uikit;
use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol};
use objc2::{AllocAnyThread, DefinedClass, MainThreadMarker, MainThreadOnly, class, define_class, extern_class, msg_send};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSError, NSString, NSURL, NSURLRequest};
use objc2_ui_kit::{UIResponder, UIView};

// WKWebView lives in WebKit.framework. objc2-web-kit force-links it on macOS but only binds the
// AppKit variant, so on iOS we hand-roll the class below. WebKit must be LINKED or
// `objc_getClass("WKWebView")` returns nil and `alloc` aborts (SIGABRT) — declared via this crate's
// `[package.metadata.day.ios].frameworks = ["WebKit"]`, which the generated DayPieces SwiftPM package
// links into the app (no runtime `dlopen`, no xcodeproj edit — the framework-contribution seam).

// The iOS WKWebView (a UIView subclass). We only need a handful of methods, called via msg_send!.
extern_class!(
    #[unsafe(super(UIView, UIResponder, NSObject))]
    #[thread_kind = MainThreadOnly]
    struct WKWebView;
);

// The configuration objects behind a view, hand-rolled for the same reason: the script-message
// channel and the fit-content reporter hang off the configuration's user content controller
// (docs/webview.md, docs/webview-eval.md § Script messages).
extern_class!(
    #[unsafe(super(NSObject))]
    struct WKWebViewConfiguration;
);
extern_class!(
    #[unsafe(super(NSObject))]
    struct WKUserContentController;
);
extern_class!(
    #[unsafe(super(NSObject))]
    struct WKUserScript;
);
extern_class!(
    #[unsafe(super(NSObject))]
    struct WKContentWorld;
);
extern_class!(
    #[unsafe(super(NSObject))]
    struct WKProcessPool;
);

/// The script-message handler name: `window.webkit.messageHandlers.day.postMessage(v)`.
const MESSAGE_HANDLER: &str = "day";

/// The fit-content height channel — a second handler so the piece's own traffic never
/// reaches the app's `on_message`, registered in [`FIT_WORLD`] rather than the page's world
/// (`addScriptMessageHandler:contentWorld:name:`, iOS 14+).
const FIT_HANDLER: &str = "dayFit";

/// The isolated content world the fit-content reporter runs in (`WKContentWorld
/// worldWithName:`): it shares the document but not the page's JavaScript globals, so the
/// page can neither see `webkit.messageHandlers.dayFit` nor post a height of its own.
const FIT_WORLD: &str = "day-fit";

/// Injected at document end into a fit-content view, in the isolated world: pins the page's
/// scrolling off (WebKit has no user style sheet API; the `<style>` lands in the shared DOM
/// from the isolated world), then reports the document's height — the root element's border
/// box, NOT `scrollHeight`, which is clamped to the viewport and would never let the view
/// shrink — whenever it changes: after the load, as images land, when a width change reflows
/// the text. The same reporter as the GTK arm's.
const FIT_SCRIPT: &str = r#"(function(){
if (window.__dayFit) return; window.__dayFit = true;
var st = document.createElement('style');
st.textContent = 'html{overflow:hidden!important;height:auto!important}';
(document.head || document.documentElement).appendChild(st);
var last = -1;
function post() {
  var d = document.documentElement, b = document.body;
  var h = Math.ceil(d.getBoundingClientRect().height);
  if (b) h = Math.max(h, Math.ceil(b.getBoundingClientRect().height + b.offsetTop));
  if (h !== last) { last = h; window.webkit.messageHandlers.dayFit.postMessage(h); }
}
var ro = new ResizeObserver(post);
ro.observe(document.documentElement);
if (document.body) ro.observe(document.body);
window.addEventListener('load', post);
post();
})();"#;

/// `WKUserScriptInjectionTimeAtDocumentEnd`.
const INJECT_AT_DOCUMENT_END: isize = 1;

/// The height a fit-content view has before its document has reported one (unless the app
/// estimated better), and the least it can report: one line.
const FIT_MIN: f64 = 20.0;

/// The most a document can make its view: a broken measure is capped rather than allowed to
/// give layout a mile-high leaf.
const FIT_MAX: f64 = 100_000.0;

struct NavIvars {
    /// Mutable: a session-retained view outlives the node that first realized it, and must report
    /// to whichever node is currently showing it.
    node: Cell<NodeId>,
    /// Inline mode (docs/webview.md): the `file://` URL prefix of the bundled site's root.
    /// A main-frame navigation outside it is cancelled and reported (`Report::Link`); `None`
    /// (remote mode) polices nothing.
    inline_base: RefCell<Option<String>>,
    /// Fit-content mode: the document's last reported height in points (`None` = a filling
    /// view). What `measure` answers with; the app's estimate (floored at [`FIT_MIN`]) until
    /// the first report.
    fit: Cell<Option<f64>>,
}

/// WKNavigationActionPolicy, hand-rolled like the class itself: Cancel = 0, Allow = 1.
const POLICY_CANCEL: isize = 0;
const POLICY_ALLOW: isize = 1;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "DayWebNavUIKit"]
    #[ivars = NavIvars]
    struct WebNav;

    unsafe impl NSObjectProtocol for WebNav {}

    impl WebNav {
        // WKNavigationDelegate's webView:didFinishNavigation: — WKWebView calls it on the object we
        // set as its navigationDelegate; responding to the selector is all that's required.
        #[unsafe(method(webView:didFinishNavigation:))]
        fn did_finish(&self, web_view: &WKWebView, _navigation: *mut AnyObject) {
            if let Some(url) = current_url(web_view) {
                day_uikit::emit(self.ivars().node.get(), Report::Url.event(url));
            }
        }

        // Inline mode's link policy — same contract as the AppKit arm's `decide_policy`: a
        // main-frame navigation leaving the bundled site is CANCELLED and reported; the piece
        // runs the app's `LinkPolicy` (events are enqueue-only, the decision can't come back
        // through this callback). Raw msg_send shapes, like the rest of this hand-rolled arm.
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        fn decide_policy(
            &self,
            _web_view: &WKWebView,
            action: &AnyObject,
            handler: &block2::DynBlock<dyn Fn(isize)>,
        ) {
            let policy = match &*self.ivars().inline_base.borrow() {
                None => POLICY_ALLOW,
                Some(base) => {
                    let frame: *mut AnyObject = unsafe { msg_send![action, targetFrame] };
                    // Subframes stay the page's business; a nil target frame (window.open,
                    // target=_blank) is external by definition.
                    let main_frame =
                        !frame.is_null() && unsafe { msg_send![&*frame, isMainFrame] };
                    let sub_frame = !frame.is_null() && !main_frame;
                    let req: Retained<NSURLRequest> = unsafe { msg_send![action, request] };
                    let url = req
                        .URL()
                        .and_then(|u| u.absoluteString())
                        .map(|s| s.to_string())
                        .unwrap_or_default();
                    let inside = url.starts_with(base.as_str()) || url == "about:blank";
                    if sub_frame || inside {
                        POLICY_ALLOW
                    } else {
                        day_uikit::emit(self.ivars().node.get(), Report::Link.event(url));
                        POLICY_CANCEL
                    }
                }
            };
            handler.call((policy,));
        }

        // WKScriptMessageHandler's userContentController:didReceiveScriptMessage: — both the
        // app's `day` channel and the piece's own `dayFit` channel land here, told apart by
        // the message's name. Responding to the selector is all WebKit requires.
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        fn did_receive_message(&self, _controller: &AnyObject, message: &AnyObject) {
            let name: Retained<NSString> = unsafe { msg_send![message, name] };
            let body: *mut AnyObject = unsafe { msg_send![message, body] };
            let text = message_text(body);
            if name.to_string() == FIT_HANDLER {
                // Only the reporter can post here (the handler lives in its world), but the
                // value is still a measure of untrusted content: a non-finite number would
                // loop layout, an absurd one would give it a mile-high leaf.
                let Ok(h) = text.trim().parse::<f64>() else {
                    return;
                };
                if !h.is_finite() {
                    return;
                }
                let h = h.clamp(FIT_MIN, FIT_MAX);
                if self.ivars().fit.get() == Some(h) {
                    return;
                }
                self.ivars().fit.set(Some(h));
                day_uikit::emit(self.ivars().node.get(), Report::Fit.event(h.to_string()));
            } else {
                day_uikit::emit(self.ivars().node.get(), Report::Message.event(text));
            }
        }
    }
);

/// A posted script message as text: a string as itself, a number as its decimal spelling,
/// anything else as its JSON (`null` when it cannot serialize) — the GTK arm's contract.
fn message_text(body: *mut AnyObject) -> String {
    if body.is_null() {
        return "null".into();
    }
    // SAFETY: a non-null object WebKit handed the handler, alive for the call.
    let obj = unsafe { &*body };
    if let Some(s) = obj.downcast_ref::<NSString>() {
        return s.to_string();
    }
    let is_number: bool = unsafe { msg_send![obj, isKindOfClass: class!(NSNumber)] };
    if is_number {
        let s: Retained<NSString> = unsafe { msg_send![obj, stringValue] };
        return s.to_string();
    }
    let valid: bool = unsafe { msg_send![class!(NSJSONSerialization), isValidJSONObject: obj] };
    if valid {
        let data: *mut AnyObject = unsafe {
            msg_send![class!(NSJSONSerialization), dataWithJSONObject: obj, options: 0usize, error: std::ptr::null_mut::<*mut NSError>()]
        };
        if !data.is_null() {
            // NSUTF8StringEncoding = 4.
            let s: Option<Retained<NSString>> =
                unsafe { msg_send![NSString::alloc(), initWithData: &*data, encoding: 4usize] };
            if let Some(s) = s {
                return s.to_string();
            }
        }
    }
    "null".into()
}

impl WebNav {
    fn new(mtm: MainThreadMarker, node: NodeId, fit: Option<f64>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(NavIvars {
            node: Cell::new(node),
            inline_base: RefCell::new(None),
            fit: Cell::new(fit),
        });
        unsafe { msg_send![super(this), init] }
    }
}

/// The one process pool every view's configuration shares, so N views (a conversation's
/// message cards) run in one web content process rather than one each (the GTK arm's
/// `related_view` anchor).
fn process_pool() -> Retained<WKProcessPool> {
    POOL.with(|p| {
        p.borrow_mut()
            .get_or_insert_with(|| unsafe { msg_send![WKProcessPool::alloc(), init] })
            .clone()
    })
}

/// Fit-content mode (docs/webview.md): the reporter and its handler in the isolated world,
/// and the view's own scrolling off so the enclosing native scroll owns the gesture.
fn install_fit(web: &WKWebView, ucc: &WKUserContentController, nav: &WebNav) {
    unsafe {
        let world: Retained<WKContentWorld> =
            msg_send![class!(WKContentWorld), worldWithName: &*NSString::from_str(FIT_WORLD)];
        let script: Retained<WKUserScript> = msg_send![
            WKUserScript::alloc(),
            initWithSource: &*NSString::from_str(FIT_SCRIPT),
            injectionTime: INJECT_AT_DOCUMENT_END,
            forMainFrameOnly: true,
            inContentWorld: &*world
        ];
        let _: () = msg_send![ucc, addUserScript: &*script];
        let _: () = msg_send![
            ucc,
            addScriptMessageHandler: nav,
            contentWorld: &*world,
            name: &*NSString::from_str(FIT_HANDLER)
        ];
        let sv: Retained<AnyObject> = msg_send![web, scrollView];
        let _: () = msg_send![&*sv, setScrollEnabled: false];
        let _: () = msg_send![&*sv, setBounces: false];
    }
}

/// A document-mode base as WebKit wants it: a `file://` base through `fileURLWithPath:`
/// (`URLWithString:` returns nil for an unencoded path), anything else as a URL string.
fn base_nsurl(base: &str) -> Option<Retained<NSURL>> {
    if base.is_empty() {
        return None;
    }
    if let Some(path) = base.strip_prefix("file://") {
        let is_dir = path.ends_with('/');
        return Some(NSURL::fileURLWithPath_isDirectory(&NSString::from_str(path), is_dir));
    }
    NSURL::URLWithString(&NSString::from_str(base))
}

/// The base's spelling as WebKit will report navigations against it (the policy compares
/// prefixes, so both sides must come from the same `NSURL`).
fn base_text(base: &str) -> Option<String> {
    base_nsurl(base).and_then(|u| u.absoluteString()).map(|s| s.to_string())
}

day_core::tls_group! {
    // Keep each navigation delegate alive as long as its web view (delegate ref is weak).
    static DELEGATES: RefCell<HashMap<usize, Retained<WebNav>>> = RefCell::new(HashMap::new());
    // Session id -> the retained web view. Day drops its own reference when the page is navigated
    // away from, which on UIKit only detaches (`removeFromSuperview`); this reference is what keeps
    // the engine — and so the loaded page and its JavaScript context — alive until the app returns.
    static SESSIONS: RefCell<HashMap<u64, Retained<UIView>>> = RefCell::new(HashMap::new());
    // The shared process pool (see `process_pool`).
    static POOL: RefCell<Option<Retained<WKProcessPool>>> = RefCell::new(None);
}

fn current_url(web: &WKWebView) -> Option<String> {
    let url: Option<Retained<NSURL>> = unsafe { msg_send![web, URL] };
    let s = url?.absoluteString()?;
    Some(s.to_string())
}

fn load_url(web: &WKWebView, url: &str) {
    let ns = NSString::from_str(url);
    let Some(nsurl) = NSURL::URLWithString(&ns) else {
        return;
    };
    let req = NSURLRequest::requestWithURL(&nsurl);
    let _: *mut AnyObject = unsafe { msg_send![web, loadRequest: &*req] };
}

fn load_html(web: &WKWebView, html: &str, base: &str) {
    let ns = NSString::from_str(html);
    let base_url = base_nsurl(base);
    let _: *mut AnyObject =
        unsafe { msg_send![web, loadHTMLString: &*ns, baseURL: base_url.as_deref()] };
}

fn make(_backend: &mut Uikit, p: &WebProps, id: NodeId) -> Retained<UIView> {
    // A session already holding a view: re-attach it rather than build a new one. Only the node
    // changes — point the delegate at the node now showing it, and do NOT reload, since the whole
    // purpose is to come back to the page as it was left.
    if p.session != 0
        && let Some(view) = SESSIONS.with(|m| m.borrow().get(&p.session).cloned())
    {
        let key = (&*view as *const UIView) as usize;
        DELEGATES.with(|m| {
            if let Some(nav) = m.borrow().get(&key) {
                nav.ivars().node.set(id);
            }
        });
        return view;
    }

    let mtm = MainThreadMarker::new().unwrap();
    // The configuration carries the shared process pool and the user content controller the
    // message channels hang off; it has to be there at init, WebKit copies it.
    let cfg: Retained<WKWebViewConfiguration> =
        unsafe { msg_send![WKWebViewConfiguration::alloc(), init] };
    let _: () = unsafe { msg_send![&*cfg, setProcessPool: &*process_pool()] };
    let ucc: Retained<WKUserContentController> = unsafe { msg_send![&*cfg, userContentController] };
    let zero = CGRect { origin: CGPoint { x: 0.0, y: 0.0 }, size: CGSize { width: 0.0, height: 0.0 } };
    let web: Retained<WKWebView> =
        unsafe { msg_send![WKWebView::alloc(mtm), initWithFrame: zero, configuration: &*cfg] };
    // The app's estimate (`estimated_height`) opens a fit view at about its final size.
    let fit = p.fit.then(|| p.fit_estimate.clamp(FIT_MIN, FIT_MAX));
    let nav = WebNav::new(mtm, id, fit);
    let _: () = unsafe { msg_send![&web, setNavigationDelegate: &*nav] };
    // The app's channel: `window.webkit.messageHandlers.day.postMessage(v)`, installed only
    // for a view whose app listens (docs/webview-eval.md § Script messages).
    if p.messages {
        let _: () = unsafe {
            msg_send![&*ucc, addScriptMessageHandler: &*nav, name: &*NSString::from_str(MESSAGE_HANDLER)]
        };
    }
    if p.fit {
        install_fit(&web, &ucc, &nav);
    }
    if !p.inline_root.is_empty() {
        // Inline mode (docs/webview.md): the assets tree is loose files in the app bundle, so
        // `loadFileURL:allowingReadAccessToURL:` with the site ROOT is the whole load path —
        // WebKit resolves the page's relative references natively.
        if let Some(dir) = day_spec::resolve_asset_dir(&p.inline_root) {
            let root = NSURL::fileURLWithPath(&NSString::from_str(&dir.display().to_string()));
            let index = NSURL::fileURLWithPath(&NSString::from_str(
                &dir.join(&p.inline_start).display().to_string(),
            ));
            if let Some(base) = root.absoluteString() {
                *nav.ivars().inline_base.borrow_mut() = Some(base.to_string());
            }
            let _: *mut AnyObject =
                unsafe { msg_send![&web, loadFileURL: &*index, allowingReadAccessToURL: &*root] };
        } else {
            log::warn!(
                "day-piece-webview: inline site {:?} not found in the staged assets",
                p.inline_root
            );
        }
    } else if p.doc_mode {
        // Document mode: the HTML is the page; links are policed against the base as an
        // inline site's are (docs/webview.md). NOTE: the inline rule allows any URL under
        // the base prefix, so a relative link to a sibling file would navigate this view;
        // tighten to exact-base-or-fragment as the GTK arm does.
        if let Some(base) = base_text(&p.base_url) {
            *nav.ivars().inline_base.borrow_mut() = Some(base);
        }
        if !p.html.is_empty() {
            load_html(&web, &p.html, &p.base_url);
        }
    } else if !p.url.is_empty() {
        load_url(&web, &p.url);
    }
    let view: Retained<UIView> = Retained::from(<WKWebView as AsRef<UIView>>::as_ref(&web));
    DELEGATES.with(|m| {
        m.borrow_mut()
            .insert((view.as_ref() as *const UIView) as usize, nav)
    });
    if p.session != 0 {
        SESSIONS.with(|m| m.borrow_mut().insert(p.session, view.clone()));
    }
    view
}

/// The node a realized view belongs to. `update` gets only the native handle, so the id comes back
/// from the navigation delegate retained alongside it.
fn node_of(view: &Retained<UIView>) -> Option<NodeId> {
    let key = (&**view as *const UIView) as usize;
    DELEGATES.with(|m| m.borrow().get(&key).map(|nav| nav.ivars().node.get()))
}

/// Same contract as the AppKit arm (see its `eval`): the front-end's wrapper makes the result
/// always a JS string, so the completion's `id` is an `NSString` and no JSON walk is needed.
fn eval(web: &WKWebView, node: NodeId, req: u64, script: &str) {
    let js = NSString::from_str(script);
    let handler = RcBlock::new(move |result: *mut AnyObject, error: *mut NSError| {
        let payload = if !result.is_null() {
            // SAFETY: non-null result from WebKit; the wrapper guarantees an NSString.
            unsafe { &*result }
                .downcast_ref::<NSString>()
                .map(|s| s.to_string())
                .unwrap_or_else(|| engine_error("WebKitError", "non-string reply"))
        } else if !error.is_null() {
            // SAFETY: non-null NSError from WebKit.
            engine_error(
                "WebKitError",
                &unsafe { (*error).localizedDescription() }.to_string(),
            )
        } else {
            engine_error("WebKitError", "no result")
        };
        day_uikit::emit(node, eval_reply(req, payload));
    });
    // SAFETY: main thread (a renderer duty); WebKit copies the block before returning.
    let _: () = unsafe { msg_send![web, evaluateJavaScript: &*js, completionHandler: &*handler] };
}

fn update(_backend: &mut Uikit, h: &Retained<UIView>, patch: &WebPatch) {
    let Some(web) = (**h).downcast_ref::<WKWebView>() else {
        return;
    };
    unsafe {
        match patch {
            WebPatch::Eval { req, script } => {
                if let Some(node) = node_of(h) {
                    eval(web, node, *req, script);
                }
            }
            WebPatch::LoadHtml { html, base } => {
                let key = (h.as_ref() as *const UIView) as usize;
                DELEGATES.with(|m| {
                    if let Some(nav) = m.borrow().get(&key)
                        && let Some(base) = base_text(base)
                    {
                        *nav.ivars().inline_base.borrow_mut() = Some(base);
                    }
                });
                // A fit view keeps its height across the reload: the new document reports
                // its own as soon as it is laid out, and until then the last measured
                // height is the closest estimate there is (the GTK arm's rule).
                load_html(web, html, base)
            }
            WebPatch::Load(url) => load_url(web, url),
            WebPatch::Back => {
                let _: *mut AnyObject = msg_send![web, goBack];
            }
            WebPatch::Forward => {
                let _: *mut AnyObject = msg_send![web, goForward];
            }
            WebPatch::Stop => {
                let _: () = msg_send![web, stopLoading];
            }
            WebPatch::Reload => {
                let _: *mut AnyObject = msg_send![web, reload];
            }
        }
    }
}

/// Drop the retained navigation delegate when the view goes away.
///
/// Without this the map grows by one entry per realized web view, and — worse — its key is the
/// view's ADDRESS, which the allocator reuses: a later view landing on a freed address would
/// inherit the dead node's id and misroute its events.
fn release(_backend: &mut Uikit, h: &Retained<UIView>) {
    let key = (&**h as *const UIView) as usize;
    // A session-retained view is not going away — day is only detaching it from the page being
    // torn down, and the next visit re-attaches it. Its delegate has to outlive this node too, or
    // the returning view would report navigations to nobody.
    let retained = SESSIONS.with(|m| {
        m.borrow()
            .values()
            .any(|v| (&**v as *const UIView) as usize == key)
    });
    if retained {
        return;
    }
    DELEGATES.with(|m| {
        m.borrow_mut().remove(&key);
    });
}

/// A fit-content view is as tall as its document and as wide as it is offered; any other
/// view fills its space.
fn measure(_backend: &mut Uikit, h: &Retained<UIView>, p: day_spec::Proposal) -> day_spec::Size {
    let key = (&**h as *const UIView) as usize;
    let fit = DELEGATES.with(|m| m.borrow().get(&key).and_then(|nav| nav.ivars().fit.get()));
    match fit {
        Some(height) => day_spec::Size::new(p.width.unwrap_or(0.0), height),
        None => day_spec::Size::new(p.width.unwrap_or(0.0), p.height.unwrap_or(0.0)),
    }
}

day_pieces::renderer!(day_uikit::RENDERERS, Uikit,
    kind: KIND, props: WebProps, patch: WebPatch,
    make: make, update: update, measure: measure, release: release);
