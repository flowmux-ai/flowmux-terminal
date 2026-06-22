// SPDX-License-Identifier: GPL-3.0-or-later
//! Standalone renderer demo for the libghostty-vt backend (task C / M2).
//!
//! Opens a GTK4 window, spawns a shell through `flowmux_terminal::pty::Pty`,
//! feeds its output into `flowmux_terminal::vt::Vt`, and draws the resulting
//! grid with Cairo. This exists so the renderer can be exercised and
//! screenshot-verified independently of flowmux's pane system; M4 promotes the
//! widget into the real pane tree. Build/run with:
//!
//!   cargo run -p flowmux --features libghostty --example ghostty_demo -- [cmd]
//!
//! With no argument it runs `$SHELL` (or bash) interactively. With an argument
//! it runs `sh -c <cmd>` and keeps the screen up (handy for deterministic
//! screenshots).

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk::cairo;
use gtk::gdk;
use gtk::glib;
use gtk::pango;
use gtk::prelude::*;

use flowmux_terminal::pty::Pty;
use flowmux_terminal::vt::{Rgb, Vt};

const APP_ID: &str = "com.flowmux.GhosttyDemo";
const FONT: &str = "Monospace 12";
const SCROLLBACK: usize = 2000;

struct Term {
    vt: Vt,
    pty: Pty,
    cell_w: f64,
    cell_h: f64,
    ascent: f64,
    cols: u16,
    rows: u16,
}

fn rgb(c: Rgb) -> (f64, f64, f64) {
    (
        c.r as f64 / 255.0,
        c.g as f64 / 255.0,
        c.b as f64 / 255.0,
    )
}

/// Measure monospace cell metrics (width, height, ascent) for FONT using Pango,
/// so the grid matches the font the glyphs are actually drawn with.
fn measure_cell() -> (f64, f64, f64) {
    let surf = cairo::ImageSurface::create(cairo::Format::ARgb32, 8, 8).unwrap();
    let cr = cairo::Context::new(&surf).unwrap();
    let layout = pangocairo::functions::create_layout(&cr);
    let desc = pango::FontDescription::from_string(FONT);
    layout.set_font_description(Some(&desc));

    // Cell width = precise monospace advance (matches ghostty_pane::measure_cell).
    layout.set_text("0000000000");
    let cell_w = (layout.pixel_size().0 as f64 / 10.0).round().max(1.0);

    let ctx = layout.context();
    let metrics = ctx.metrics(Some(&desc), None);
    let scale = pango::SCALE as f64;
    let ascent = metrics.ascent() as f64 / scale;
    let descent = metrics.descent() as f64 / scale;
    let cell_h = (ascent + descent).ceil().max(1.0);
    (cell_w, cell_h, ascent)
}

fn build_argv(cmd: Option<&str>) -> Vec<String> {
    match cmd {
        // Keep the rendered screen up after the command finishes.
        Some(c) => vec![
            "sh".into(),
            "-c".into(),
            format!("{c}; printf '\\n[demo: command finished]\\n'; exec sleep 86400"),
        ],
        None => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "bash".into());
            vec![shell, "-i".into()]
        }
    }
}

/// Encode a GTK key press into the bytes a terminal expects. Returns None for
/// keys we do not translate (modifiers alone, unhandled function keys).
fn encode_key(keyval: gdk::Key, state: gdk::ModifierType) -> Option<Vec<u8>> {
    use gdk::Key;
    let ctrl = state.contains(gdk::ModifierType::CONTROL_MASK);

    // Named keys first.
    let named: Option<&[u8]> = match keyval {
        Key::Return | Key::KP_Enter => Some(b"\r"),
        Key::BackSpace => Some(b"\x7f"),
        Key::Tab => Some(b"\t"),
        Key::Escape => Some(b"\x1b"),
        Key::Up => Some(b"\x1b[A"),
        Key::Down => Some(b"\x1b[B"),
        Key::Right => Some(b"\x1b[C"),
        Key::Left => Some(b"\x1b[D"),
        Key::Home => Some(b"\x1b[H"),
        Key::End => Some(b"\x1b[F"),
        Key::Page_Up => Some(b"\x1b[5~"),
        Key::Page_Down => Some(b"\x1b[6~"),
        Key::Delete => Some(b"\x1b[3~"),
        _ => None,
    };
    if let Some(bytes) = named {
        return Some(bytes.to_vec());
    }

    let ch = keyval.to_unicode()?;
    if ctrl {
        // Control codes: Ctrl-A..Ctrl-Z and a few punctuation maps.
        let b = ch.to_ascii_uppercase() as u32;
        if (b'A' as u32..=b'Z' as u32).contains(&b) {
            return Some(vec![(b - b'A' as u32 + 1) as u8]);
        }
        match ch {
            ' ' => return Some(vec![0]),
            '[' => return Some(vec![0x1b]),
            '\\' => return Some(vec![0x1c]),
            ']' => return Some(vec![0x1d]),
            _ => {}
        }
    }

    let mut buf = [0u8; 4];
    Some(ch.encode_utf8(&mut buf).as_bytes().to_vec())
}

fn draw(term: &mut Term, cr: &cairo::Context, w: i32, h: i32) {
    let _ = term.vt.update();
    let colors = term.vt.colors().unwrap_or(flowmux_terminal::vt::Colors {
        fg: Rgb { r: 220, g: 220, b: 220 },
        bg: Rgb { r: 0, g: 0, b: 0 },
        cursor: Rgb { r: 220, g: 220, b: 220 },
        cursor_has_value: false,
    });

    // Clear with the terminal default background.
    let (br, bgc, bb) = rgb(colors.bg);
    cr.set_source_rgb(br, bgc, bb);
    cr.rectangle(0.0, 0.0, w as f64, h as f64);
    let _ = cr.fill();

    // One reusable Pango layout for glyph rendering (gives real font fallback
    // for CJK/emoji, unlike cairo's toy text API).
    let layout = pangocairo::functions::create_layout(cr);
    let desc = pango::FontDescription::from_string(FONT);
    layout.set_font_description(Some(&desc));

    let (cols, rows) = term.vt.dims().unwrap_or((term.cols, term.rows));
    let cw = term.cell_w;
    let ch = term.cell_h;
    let ascent = term.ascent;

    for row in 0..rows {
        let y = row as f64 * ch;
        // Read the row once, render in two passes (all backgrounds, then all
        // glyphs) so a wide glyph's right half is not erased by the next
        // spacer cell's background.
        let cells: Vec<Option<flowmux_terminal::vt::Cell>> =
            (0..cols).map(|col| term.vt.cell(row, col)).collect();

        for (col, cell) in cells.iter().enumerate() {
            let Some(cell) = cell else { continue };
            let x = col as f64 * cw;
            let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
            if cell.selected {
                cr.set_source_rgb(0.20, 0.34, 0.55);
                cr.rectangle(x, y, cell_px_w, ch);
                let _ = cr.fill();
            } else {
                let bg = if cell.style.inverse {
                    Some(cell.fg)
                } else {
                    cell.bg
                };
                if let Some(b) = bg {
                    let (r, g, bl) = rgb(b);
                    cr.set_source_rgb(r, g, bl);
                    cr.rectangle(x, y, cell_px_w, ch);
                    let _ = cr.fill();
                }
            }
        }

        for (col, cell) in cells.iter().enumerate() {
            let Some(cell) = cell else { continue };
            let x = col as f64 * cw;
            let cell_px_w = if cell.wide { cw * 2.0 } else { cw };
            let fg = if cell.style.inverse {
                cell.bg.unwrap_or(colors.bg)
            } else {
                cell.fg
            };
            let (fr, fgc, fb) = rgb(fg);
            if !cell.text.is_empty() {
                layout.set_text(&cell.text);
                let baseline = layout.baseline() as f64 / pango::SCALE as f64;
                let glyph_w = layout.pixel_size().0 as f64;
                cr.set_source_rgb(fr, fgc, fb);
                if cell.wide && glyph_w > 1.0 && glyph_w < cell_px_w - 0.5 {
                    let sx = (cell_px_w / glyph_w).min(1.6);
                    cr.save().ok();
                    cr.translate(x, y + ascent - baseline);
                    cr.scale(sx, 1.0);
                    cr.move_to(0.0, 0.0);
                    pangocairo::functions::show_layout(cr, &layout);
                    cr.restore().ok();
                } else {
                    let x_off = ((cell_px_w - glyph_w) / 2.0).max(0.0);
                    cr.move_to(x + x_off, y + ascent - baseline);
                    pangocairo::functions::show_layout(cr, &layout);
                }
            }
            if cell.style.underline {
                cr.set_source_rgb(fr, fgc, fb);
                cr.rectangle(x, y + ascent + 2.0, cell_px_w, 1.0);
                let _ = cr.fill();
            }
            if cell.style.strikethrough {
                cr.set_source_rgb(fr, fgc, fb);
                cr.rectangle(x, y + ch * 0.5, cell_px_w, 1.0);
                let _ = cr.fill();
            }
        }
    }

    // Cursor: a translucent block so the glyph beneath stays legible.
    if let Some(cursor) = term.vt.cursor() {
        if cursor.visible && cursor.x < cols && cursor.y < rows {
            let (r, g, b) = if colors.cursor_has_value {
                rgb(colors.cursor)
            } else {
                rgb(colors.fg)
            };
            cr.set_source_rgba(r, g, b, 0.6);
            cr.rectangle(cursor.x as f64 * cw, cursor.y as f64 * ch, cw, ch);
            let _ = cr.fill();
        }
    }
}

/// Block up to `timeout` for the fd to become readable. Returns true if so.
fn poll_readable(fd: std::os::unix::io::RawFd, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: single valid pollfd, bounded timeout.
    let rc = unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as libc::c_int) };
    rc > 0 && (pfd.revents & libc::POLLIN) != 0
}

/// Headless capture: run the command, pump the PTY into the VT for a bounded
/// window, render the grid into an offscreen PNG. No display/GTK required — the
/// `draw()` path is pure Cairo, so this works under any environment.
fn capture(cmd: Option<&str>, path: &str) {
    let (cell_w, cell_h, ascent) = measure_cell();
    let cols: u16 = 100;
    let rows: u16 = 30;

    let mut vt = Vt::new(cols, rows, SCROLLBACK).expect("vt new");
    // Optional theme colors for verifying the libghostty color path:
    // FLOWMUX_DEMO_COLORS="fgR,fgG,fgB,bgR,bgG,bgB[,curR,curG,curB]".
    if let Ok(spec) = std::env::var("FLOWMUX_DEMO_COLORS") {
        let n: Vec<u8> = spec.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        if n.len() >= 6 {
            let fg = Rgb { r: n[0], g: n[1], b: n[2] };
            let bg = Rgb { r: n[3], g: n[4], b: n[5] };
            let cur = if n.len() >= 9 {
                Rgb { r: n[6], g: n[7], b: n[8] }
            } else {
                fg
            };
            vt.set_default_colors(fg, bg, cur);
        }
    }
    let env = vec![
        ("TERM".to_string(), "xterm-256color".to_string()),
        ("COLORTERM".to_string(), "truecolor".to_string()),
    ];
    let argv_owned = build_argv(cmd);
    let argv: Vec<&str> = argv_owned.iter().map(|s| s.as_str()).collect();
    let mut pty = Pty::spawn(&argv, None, &env, cols, rows).expect("pty spawn");
    let _ = pty.resize(cols, rows, cell_w as u16, cell_h as u16);

    let fd = pty.master_fd();
    let mut buf = [0u8; 8192];
    let deadline = Instant::now() + Duration::from_secs(4);
    let mut last_data = Instant::now();
    loop {
        if poll_readable(fd, Duration::from_millis(150)) {
            match pty.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    vt.write(&buf[..n]);
                    last_data = Instant::now();
                }
                Err(_) => break,
            }
        } else if last_data.elapsed() > Duration::from_millis(400) {
            // Output has settled.
            break;
        }
        if Instant::now() > deadline {
            break;
        }
    }

    // Optional selection for verifying the selection path:
    // FLOWMUX_DEMO_SELECT="sx,sy,ex,ey".
    if let Ok(spec) = std::env::var("FLOWMUX_DEMO_SELECT") {
        let n: Vec<u16> = spec.split(',').filter_map(|s| s.trim().parse().ok()).collect();
        if n.len() == 4 {
            vt.set_selection((n[0], n[1]), (n[2], n[3]), false);
            if let Some(text) = {
                vt.update();
                vt.selection_text()
            } {
                eprintln!("selection_text = {text:?}");
            }
        }
    }

    let mut term = Term {
        vt,
        pty,
        cell_w,
        cell_h,
        ascent,
        cols,
        rows,
    };

    let w = (cols as f64 * cell_w) as i32;
    let h = (rows as f64 * cell_h) as i32;
    let mut surface = cairo::ImageSurface::create(cairo::Format::ARgb32, w, h).expect("surface");
    {
        let cr = cairo::Context::new(&surface).expect("context");
        draw(&mut term, &cr, w, h);
    }
    write_png(&mut surface, w as usize, h as usize, path);
    eprintln!("captured {w}x{h} -> {path}");
}

/// Encode an opaque ARgb32 ImageSurface to an RGB PNG. cairo stores ARGB32 as
/// native-endian 0xAARRGGBB, i.e. little-endian byte order B,G,R,A; our fills
/// are opaque so we drop alpha.
fn write_png(surface: &mut cairo::ImageSurface, w: usize, h: usize, path: &str) {
    surface.flush();
    let stride = surface.stride() as usize;
    let data = surface.data().expect("surface data");
    let mut rgb = Vec::with_capacity(w * h * 3);
    for y in 0..h {
        let row = &data[y * stride..y * stride + w * 4];
        for x in 0..w {
            let i = x * 4;
            rgb.push(row[i + 2]); // R
            rgb.push(row[i + 1]); // G
            rgb.push(row[i]); // B
        }
    }
    drop(data);

    let file = std::fs::File::create(path).expect("create png");
    let writer = std::io::BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, w as u32, h as u32);
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut w_png = encoder.write_header().expect("png header");
    w_png.write_image_data(&rgb).expect("png data");
}

fn main() -> glib::ExitCode {
    // Headless screenshot mode (used for renderer verification): render once to
    // a PNG and exit, without opening a window.
    if let Ok(path) = std::env::var("GHOSTTY_DEMO_CAPTURE") {
        capture(std::env::args().nth(1).as_deref(), &path);
        return glib::ExitCode::SUCCESS;
    }

    let cmd: Option<String> = std::env::args().nth(1);

    let app = gtk::Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        let (cell_w, cell_h, ascent) = measure_cell();

        // Initial geometry: 100x30 cells.
        let init_cols: u16 = 100;
        let init_rows: u16 = 30;

        let vt = Vt::new(init_cols, init_rows, SCROLLBACK).expect("vt new");
        let env = vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("COLORTERM".to_string(), "truecolor".to_string()),
        ];
        let argv_owned = build_argv(cmd.as_deref());
        let argv: Vec<&str> = argv_owned.iter().map(|s| s.as_str()).collect();
        let pty = Pty::spawn(&argv, None, &env, init_cols, init_rows).expect("pty spawn");

        let term = Rc::new(RefCell::new(Term {
            vt,
            pty,
            cell_w,
            cell_h,
            ascent,
            cols: init_cols,
            rows: init_rows,
        }));

        let area = gtk::DrawingArea::new();
        area.set_content_width((init_cols as f64 * cell_w) as i32);
        area.set_content_height((init_rows as f64 * cell_h) as i32);

        // Draw.
        {
            let term = term.clone();
            area.set_draw_func(move |_area, cr, w, h| {
                draw(&mut term.borrow_mut(), cr, w, h);
            });
        }

        // Resize: recompute grid from the new pixel size.
        {
            let term = term.clone();
            area.connect_resize(move |area, w, h| {
                let mut t = term.borrow_mut();
                let cols = ((w as f64 / t.cell_w).floor() as i64).clamp(1, u16::MAX as i64) as u16;
                let rows = ((h as f64 / t.cell_h).floor() as i64).clamp(1, u16::MAX as i64) as u16;
                if (cols, rows) != (t.cols, t.rows) {
                    t.cols = cols;
                    t.rows = rows;
                    let cw = t.cell_w as u16;
                    let chh = t.cell_h as u16;
                    t.vt.resize(cols, rows, cw as u32, chh as u32);
                    let _ = t.pty.resize(cols, rows, cw, chh);
                    area.queue_draw();
                }
            });
        }

        // PTY output -> vt -> redraw, driven by the glib main loop.
        {
            let term = term.clone();
            let area = area.clone();
            let fd = term.borrow().pty.master_fd();
            glib::source::unix_fd_add_local(fd, glib::IOCondition::IN, move |_fd, _cond| {
                let mut buf = [0u8; 8192];
                let mut t = term.borrow_mut();
                match t.pty.read(&mut buf) {
                    Ok(0) => return glib::ControlFlow::Break, // child exited
                    Ok(n) => t.vt.write(&buf[..n]),
                    Err(_) => return glib::ControlFlow::Break,
                }
                drop(t);
                area.queue_draw();
                glib::ControlFlow::Continue
            });
        }

        let window = gtk::ApplicationWindow::builder()
            .application(app)
            .title("flowmux — libghostty-vt renderer demo")
            .default_width((init_cols as f64 * cell_w) as i32)
            .default_height((init_rows as f64 * cell_h) as i32)
            .child(&area)
            .build();

        // Keyboard -> PTY.
        {
            let term = term.clone();
            let area = area.clone();
            let kc = gtk::EventControllerKey::new();
            kc.connect_key_pressed(move |_kc, keyval, _code, state| {
                if let Some(bytes) = encode_key(keyval, state) {
                    let mut t = term.borrow_mut();
                    let _ = t.pty.write(&bytes);
                    drop(t);
                    area.queue_draw();
                    glib::Propagation::Stop
                } else {
                    glib::Propagation::Proceed
                }
            });
            window.add_controller(kc);
        }

        window.present();
    });

    // Examples must not consume CLI args meant for us; we read argv ourselves.
    let empty: Vec<String> = vec![];
    app.run_with_args(&empty)
}
