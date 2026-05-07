// SPDX-License-Identifier: GPL-3.0-or-later
//! Position a context-menu Popover at a click point with Windows-style
//! quadrant flipping.
//!
//! Default placement is bottom-right of the cursor (the menu's
//! top-left corner sits at the click). When the click is in the right
//! half of the window we flip `halign` to End so the menu opens to
//! the left; in the bottom half we flip `position` to Top so the menu
//! opens upward. This avoids ever clipping off-screen and matches the
//! behavior people expect from native context menus.

use gtk::graphene;
use gtk::prelude::*;

/// Anchor `popover` under `parent` at click coords (x, y) (in
/// `parent`'s local space). Picks side + alignment so the popover
/// extends away from the nearest window edge.
pub fn anchor_at_click(popover: &gtk::Popover, parent: &impl IsA<gtk::Widget>, x: f64, y: f64) {
    let parent_widget: &gtk::Widget = parent.upcast_ref();

    // Translate the click into the toplevel window's local space so
    // we can compare it against the window's overall dimensions.
    let toplevel = parent_widget
        .root()
        .and_then(|r| r.dynamic_cast::<gtk::Window>().ok());
    let (ww, wh) = toplevel
        .as_ref()
        .map(|w| (w.width().max(1), w.height().max(1)))
        .unwrap_or((1280, 800));
    let click_in_window = toplevel
        .as_ref()
        .and_then(|w| parent_widget.compute_point(w, &graphene::Point::new(x as f32, y as f32)))
        .unwrap_or_else(|| graphene::Point::new(x as f32, y as f32));

    let right_half = click_in_window.x() > ww as f32 / 2.0;
    let bottom_half = click_in_window.y() > wh as f32 / 2.0;

    let position = if bottom_half {
        gtk::PositionType::Top
    } else {
        gtk::PositionType::Bottom
    };
    let halign = if right_half {
        gtk::Align::End
    } else {
        gtk::Align::Start
    };

    let rect = gtk::gdk::Rectangle::new(x as i32, y as i32, 1, 1);
    popover.set_pointing_to(Some(&rect));
    popover.set_position(position);
    popover.set_halign(halign);
}
