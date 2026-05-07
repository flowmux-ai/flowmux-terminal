// SPDX-License-Identifier: GPL-3.0-or-later
//! Anchor a context-menu Popover with its top-left corner at the
//! click point — Windows/GNOME context-menu convention.
//!
//! The menu always opens **bottom-right** of the cursor. If part of
//! the menu would land outside the toplevel window we *shift* it up
//! and/or left by exactly the overflow, never flipping orientation.
//! That keeps the cursor at (or near) the menu's top-left in the
//! common case while still guaranteeing the whole menu is visible.
//!
//! Mechanics:
//!
//!   1. translate click coords into the toplevel window's local space,
//!   2. measure the popover's natural size (set_child / set_parent
//!      have already been called, so the layout system can answer),
//!   3. clamp the desired top-left anchor so anchor + size fits in
//!      the window,
//!   4. translate the clamped anchor back into the popover's parent
//!      widget coords and feed it to GTK as a 1×1 pointing-rect
//!      whose center lands `width/2` to the right of the anchor —
//!      the popover, centered horizontally on the rect, then has
//!      its left edge at the anchor.
//!
//! `set_position(Bottom)` keeps the popover below the rect; we do
//! not touch halign because horizontal placement is encoded into the
//! rect itself.

use gtk::graphene;
use gtk::prelude::*;

pub fn anchor_at_click(popover: &gtk::Popover, parent: &impl IsA<gtk::Widget>, x: f64, y: f64) {
    let parent_widget: &gtk::Widget = parent.upcast_ref();

    let toplevel = parent_widget
        .root()
        .and_then(|r| r.dynamic_cast::<gtk::Window>().ok());
    let (ww, wh) = toplevel
        .as_ref()
        .map(|w| (w.width().max(1) as f32, w.height().max(1) as f32))
        .unwrap_or((1280.0, 800.0));

    let click_in_win = toplevel
        .as_ref()
        .and_then(|w| {
            let widget: &gtk::Widget = w.upcast_ref();
            parent_widget.compute_point(widget, &graphene::Point::new(x as f32, y as f32))
        })
        .unwrap_or_else(|| graphene::Point::new(x as f32, y as f32));

    let (_, nat_w, _, _) = popover.measure(gtk::Orientation::Horizontal, -1);
    let (_, nat_h, _, _) = popover.measure(gtk::Orientation::Vertical, -1);
    // Conservative floor — measure may report 0 for popovers that
    // haven't been allocated yet; small enough to be a no-op clamp
    // when the popover is in fact larger.
    let mw = (nat_w as f32).max(160.0);
    let mh = (nat_h as f32).max(96.0);

    let cx = click_in_win.x();
    let cy = click_in_win.y();
    let mut ax = cx;
    let mut ay = cy;
    if ax + mw > ww {
        ax = (ww - mw).max(0.0);
    }
    if ay + mh > wh {
        ay = (wh - mh).max(0.0);
    }

    let anchor_in_parent = toplevel
        .as_ref()
        .and_then(|w| {
            let widget: &gtk::Widget = w.upcast_ref();
            widget.compute_point(parent_widget, &graphene::Point::new(ax, ay))
        })
        .unwrap_or_else(|| graphene::Point::new(ax, ay));

    // 1×1 rect whose center is mw/2 right of the desired anchor —
    // the popover, centered horizontally on the rect, then sits with
    // its left edge exactly at the anchor.
    let rect_cx = (anchor_in_parent.x() + mw / 2.0) as i32;
    let rect_y = anchor_in_parent.y() as i32 - 1;
    let rect = gtk::gdk::Rectangle::new(rect_cx, rect_y, 1, 1);
    popover.set_pointing_to(Some(&rect));
    popover.set_position(gtk::PositionType::Bottom);
    popover.set_halign(gtk::Align::Fill); // reset any prior halign
}
