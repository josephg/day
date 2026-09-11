---
title: "Programmatic scrolling"
description: "scroll_to and scroll position: driving native scroll views from Rust, and what each backend can promise."
---

<!--
Copyright © The Daybrite Project
SPDX-License-Identifier: CC-BY-SA-4.0
-->

# Programmatic scrolling

> **Status: implemented** on every backend (AppKit, UIKit, Android, GTK, Qt, XAML, ArkUI, mock).
> Every case goes through one primitive, `Toolkit::scroll_to(handle, rect, animated)`, with
> scrollRectToVisible semantics: day-core composes edges, offsets, and reveal-element targets
> into content-space rects, so each backend only implements "minimal scroll to make this rect
> visible". Verified by mock-toolkit unit tests (`crates/day-pieces/tests/mock_e2e.rs`) and the
> showcase Scrolling page + walkthrough.

The `scroll` piece stays gesture-first: the native widget owns the viewport, physics, and
indicators (DESIGN §7.6). This document covers driving it from code and from dayscript, and
reading back where it is (§ Reading the position).

## Authoring

```rust
let jump: Signal<Option<ScrollTarget>> = Signal::new(None);

scroll(column(rows)).scroll_target(jump);

button("Bottom").action(move || jump.set(Some(ScrollTarget::Bottom)));
button("Item 100").action(move || jump.set(Some(ScrollTarget::Id("row-100".into()))));
```

`.scroll_target(sig)` takes a `Signal<Option<ScrollTarget>>`: each `Some(target)` written to it
scrolls there (animated), then the signal resets to `None`, so the same target can be sent
twice in a row. `ScrollTarget` is:

| target | meaning |
|---|---|
| `Top` / `Bottom` | the vertical extremes |
| `Leading` / `Trailing` | the horizontal extremes (start/end in layout direction) |
| `Offset(Point)` | pin the viewport origin to a content-space point (clamped to range) |
| `Id(String)` | reveal the element with that dayscript id inside its nearest enclosing scroll |

Lower-level, `day_core::scroll_to(node, target)` drives any scroll node directly, and
`TreeOps::scroll_reveal(node, animated)` scrolls an element's nearest scroll ancestor so the
element is visible, the same call keyboard avoidance uses ([docs/focus.md](focus.md)). Reveals are minimal:
content already in view doesn't move.

The showcase's List page is the live reference (`Day-Showcase/src/pages/list.rs`): its scroll
buttons drive the recycling list's row rail (`.scroll_to_row`/`.scroll_to_end`, [docs/list.md](list.md));
`scroll_target` on a plain `scroll` piece works the same way with a `ScrollTarget`.

## Reading the position

> **Status: implemented** on GTK, UIKit, web-dom and mock (`Cap::ScrollReports` is `Native`
> there; the other backends answer `Unsupported` and `on_scroll` never runs there — not even
> for the programmatic and layout-driven reports below, which read the position through
> `Toolkit::scroll_offset` and would be zero without it). Verified by
> `crates/day-pieces/tests/mock_e2e.rs` (`scroll_state_follows_programmatic_scrolls_and_layout`,
> `scroll_reports_a_root_scrolls_resize_once_…`, `on_frame_reports_a_child_…`,
> `on_frame_accumulates_through_an_ancestor_…`, `on_frame_inside_a_nested_scroll_…`,
> `on_frame_inside_a_list_cell_…`, `list_on_scroll_reports_the_row_rail`) and the dayscript
> step test `scroll_to_dx_dy_moves_relative_to_the_current_offset`.

The read side of `scroll_target`: where the viewport is, and where a child sits in the
content, so an app can act on what is on screen — a mail reader marking a message read once
its card has scrolled into view, a list prefetching for the rows it shows.

```rust
let state: Signal<ScrollState> = Signal::new(ScrollState::default());

scroll(column(cards))
    .scroll_state(state)                          // the signal follows the position
    .on_scroll(|st| log::info!("{:?}", st.visible_rect()));   // or a callback

// A child that wants to know whether it is on screen:
card.on_frame(move |frame: Rect| {
    let shown = state.get_untracked().visible_rect().intersects(&frame);
    …
})
```

`ScrollState { offset, viewport, content }` is the viewport origin in content space, the
viewport's size (the scroll piece's laid-out frame) and the content's size (what
`ScrollLayout` reported); `visible_rect()` is the slice of content on screen and
`max_offset()` the far end. `TreeOps::scroll_state` answers `None` until the scroll's first
layout. `.on_frame(f)` (a `Decorate` modifier, so it goes on any piece) delivers the piece's
frame **in the content space of its nearest enclosing scroll** — the same space
`visible_rect` is in, accumulated through the realized ancestors between the two — in the
window when no scroll encloses it, or **relative to its list cell** when it sits inside a
`list` (the cell is natively scrolled by the list's own rail; the walk stops there, see
[docs/list.md](list.md) § Reading the position). Nested scrolls: the nearest one, so a card in
an inner scroll reports in the inner space, unmoved by the outer. The frame is layout's — a
`.transform(..)` is applied by the toolkit after placement and is not part of it. It fires
whenever layout moves or resizes the piece, including when something above it grows; a frame
alone says nothing about the viewport, so pair it with the scroll's state.

When the reports come, and from where:

| cause | reported by | when |
|---|---|---|
| the user scrolls (wheel, drag, kinetic) | the toolkit, `Event::ScrollChanged` — GTK from the `GtkScrolledWindow` adjustments, web-dom from `scroll` | at most one per frame (GTK arms a tick callback on the first per-pixel notification and emits the current values once), delivered at the next event drain |
| a programmatic scroll (`scroll_target`, `TreeOps::scroll_to_target`/`scroll_reveal`, dayscript `scroll_to`) | day-core, after `Toolkit::scroll_to` returns, with `Toolkit::scroll_offset` | where `Cap::ScrollReports` is answered (an arm without `scroll_offset` would report zero, so day-core stays silent there). After an ANIMATED scroll the offset is the one at the call, before the animation moved anything; the toolkit's own report says where it settled |
| layout resizes the viewport or the content | day-core, from `place_node` / `set_scroll_content` | where the cap is answered; on the viewport's SIZE (a scroll that merely moves shows the same slice — and a scroll at the window's root, which the toolkit sizes, still reports when the window does); one report per drain per scroll (a pass that changes both queues one) |
| the natively-owned extent of a `list` changes (rows added, the host resized) | GTK, from the adjustments' `upper`/`page-size` | as a gesture |

A toolkit that reports a programmatic scroll on its own (GTK's `set_value` notifies) sends a
duplicate of day-core's; the piece delivers **one callback per distinct state**, so an
`on_scroll` listener hears each position once whichever side reported it first. Every report
is queue-only (DESIGN §8.3): handlers run at the drain after the layout that produced the
frame, never inside the native callback, and each handler runs under the scope it was
registered in. The §14.6 event observer sees every `ScrollChanged` as the app receives it —
a report coalesced away in the same drain is dropped from the queue and the fresh one
enqueued through the normal path, never rewritten in place.

`Toolkit::scroll_offset(handle)` is the duty behind both — day-core reads it for the
synthesized reports, and makes them only where `Cap::ScrollReports` is answered: an arm that
has the duty but not the cap (XAML today, docs/duty-matrix.md) gets none until it emits the
user's scrolls too, rather than reports that are right by luck.
UIKit (2026-09): every `scroll` piece's `UIScrollView` gets a delegate and the `list`'s
table delegate hears the same call — `scrollViewDidScroll:` fires per frame of a drag, of a
fling's deceleration and of a pull, and the arm coalesces to one report per main-queue turn,
emitting the offset it reads THEN. The offset is the viewport origin in content space:
`contentOffset` plus `adjustedContentInset` (a list under a translucent bar rests at a
negative `contentOffset`), which is what `Toolkit::scroll_offset` answers there too. The
AppKit, Android, Qt and ArkUI arms are not written: `NSScrollView`'s
`NSViewBoundsDidChangeNotification` on the clip view, `View.OnScrollChangeListener`,
`QScrollBar::valueChanged` and `NODE_SCROLL_EVENT_ON_SCROLL` are the hooks, each wanting the
same per-frame coalescing.

A `list` has the same channel for its own row rail — `list(..).on_scroll(..)` — see
[docs/list.md](list.md) § Reading the position.

## dayscript

```yaml
- scroll_to: { id: page-scroll, edge: bottom }     # top | bottom | leading | trailing
- scroll_to: { id: page-scroll, x: 0, y: 300 }     # pin the viewport origin
- scroll_to: { id: page-scroll, dy: 400 }          # move it, relative to where it is (dx too)
- scroll_to: { id: row-100 }                       # reveal an element in its nearest scroll
```

The step is unanimated so the next step sees the settled position; the scroll's `on_scroll`
listeners hear it at the next drain, so a `wait_idle` after the step lets what they do
(mark something read, load something) land before the assertion that checks it. `dx`/`dy`
add to the current offset, so two steps add up and a step past an edge clamps like any
offset; they need a `scroll` piece's id (an element inside one is a reveal).
`assert_visible` remains a presence check (realized + nonzero frame; DESIGN Appendix C); it
does not test whether an element is inside the viewport, so pair `scroll_to` with
screenshots when the test is about what's on screen, or assert the app-level effect of the
scroll.

## How a target becomes a scroll

`ScrollLayout` reports the content size per scroll node (cached in the tree), so day-core can
compose each target into a content-space rect: edges become 1×1 rects at the extremes, `Offset`
becomes a viewport-sized rect (minimal-reveal on a viewport-sized rect pins the origin exactly),
and reveal-element accumulates native-ancestor origins from the element up to its scroll. Every
backend then applies the same "minimal scroll to make the rect visible" rule:

| backend | native call |
|---|---|
| AppKit | `NSView.scrollRectToVisible` on the document view |
| UIKit | `UIScrollView.scrollRectToVisible(_:animated:)` |
| Android | offset math + `ScrollView.smoothScrollTo` / `scrollTo` (per axis class) |
| GTK | adjustment clamp + `set_value` (no animation; GTK adjusts immediately) |
| Qt | scroll-bar clamp + `setValue` (no animation) |
| XAML | `ScrollViewer.ChangeView` (shim `day_xaml_scroll_to`) |
| ArkUI | `NODE_SCROLL_OFFSET` get/compute/set (300 ms animation when animated) |
| mock | records the computed offset (`MockWidget::scroll_offset`), the unit-test probe |

Nested scrolls reveal in the nearest enclosing scroll only; driving an outer scroll takes a
second target aimed at it.
