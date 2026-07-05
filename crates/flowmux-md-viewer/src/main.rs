// SPDX-License-Identifier: GPL-3.0-or-later

use std::path::{Path, PathBuf};
use std::process::ExitCode;
#[cfg(target_os = "linux")]
use std::time::Duration;
#[cfg(any(test, target_os = "linux"))]
use std::time::SystemTime;

#[cfg(target_os = "linux")]
use std::cell::{Cell, RefCell};
#[cfg(target_os = "linux")]
use std::rc::Rc;

#[cfg(target_os = "linux")]
use adw::prelude::*;
#[cfg(target_os = "linux")]
use flowmux_md_viewer::render_markdown_file;
#[cfg(any(test, target_os = "linux"))]
use flowmux_md_viewer::HtmlDocument;
use flowmux_md_viewer::RenderOptions;
#[cfg(any(test, target_os = "linux"))]
use gtk::gio;
#[cfg(any(test, target_os = "linux"))]
use gtk::prelude::FileExt;
#[cfg(target_os = "linux")]
use gtk::{gdk, glib};
#[cfg(target_os = "linux")]
use webkit6::prelude::*;

#[cfg(target_os = "linux")]
const APP_ID: &str = "com.flowmux.MdViewer";
const DEFAULT_WINDOW_HEIGHT: i32 = 700;
#[cfg(any(test, target_os = "linux"))]
const RENDER_TIMEOUT_SECS: u32 = 15;
#[cfg(target_os = "linux")]
const VIEWER_CHROME_CSS: &str = r#"
window.flowmux-md-viewer {
  background: #282c34;
}
window.flowmux-md-viewer > contents,
window.flowmux-md-viewer toolbarview,
window.flowmux-md-viewer headerbar {
  background: #282c34;
  color: #ffffff;
}
window.flowmux-md-viewer headerbar {
  border-bottom: 1px solid rgba(255, 255, 255, 0.10);
  box-shadow: none;
}
window.flowmux-md-viewer button {
  color: #ffffff;
}
window.flowmux-md-viewer button:hover {
  background: rgba(255, 255, 255, 0.09);
}
window.flowmux-md-viewer button:disabled {
  color: rgba(255, 255, 255, 0.38);
}
window.flowmux-md-viewer .md-viewer-content {
  background: #ffffff;
}
"#;

fn main() -> ExitCode {
    match Args::parse(std::env::args().skip(1)) {
        Ok(args) => {
            if args.help {
                print_help();
                return ExitCode::SUCCESS;
            }
            if let Some(output) = &args.render_png {
                return render_png(&args.path, output, &args.options);
            }
            run_app(args);
            ExitCode::SUCCESS
        }
        Err(message) => {
            eprintln!("{message}");
            eprintln!("Try `flowmux-md-viewer --help`.");
            ExitCode::from(2)
        }
    }
}

#[derive(Clone)]
struct Args {
    path: PathBuf,
    options: RenderOptions,
    window_height: i32,
    watch: bool,
    render_png: Option<PathBuf>,
    help: bool,
}

impl Args {
    fn parse<I>(args: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = String>,
    {
        let mut path = None;
        let mut options = RenderOptions {
            font_family: std::env::var("FLOWMUX_MD_VIEWER_FONT").ok(),
            ..Default::default()
        };
        let mut window_height = DEFAULT_WINDOW_HEIGHT;
        let mut watch = true;
        let mut render_png = None;
        let mut help = false;

        let mut iter = args.into_iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--help" | "-h" => help = true,
                "--font" => options.font_family = Some(next_value(&mut iter, "--font")?),
                "--zoom" => {
                    options.zoom = next_value(&mut iter, "--zoom")?
                        .parse()
                        .map_err(|_| "--zoom expects a number".to_string())?;
                }
                "--width" => {
                    options.width = next_value(&mut iter, "--width")?
                        .parse()
                        .map_err(|_| "--width expects an integer".to_string())?;
                }
                "--height" => {
                    window_height = next_value(&mut iter, "--height")?
                        .parse()
                        .map_err(|_| "--height expects an integer".to_string())?;
                }
                "--render-png" => {
                    render_png = Some(PathBuf::from(next_value(&mut iter, "--render-png")?))
                }
                "--watch" => watch = true,
                "--no-watch" => watch = false,
                _ if arg.starts_with('-') => return Err(format!("unknown option: {arg}")),
                _ => {
                    if path.replace(PathBuf::from(arg)).is_some() {
                        return Err("only one Markdown file can be opened".to_string());
                    }
                }
            }
        }

        if help {
            path = Some(PathBuf::new());
        }
        let path = path.ok_or_else(|| "missing Markdown file path".to_string())?;
        Ok(Self {
            path,
            options,
            window_height: window_height.max(240),
            watch,
            render_png,
            help,
        })
    }
}

fn next_value<I>(iter: &mut I, option: &str) -> Result<String, String>
where
    I: Iterator<Item = String>,
{
    iter.next()
        .ok_or_else(|| format!("{option} requires a value"))
}

fn print_help() {
    println!(
        "Usage: flowmux-md-viewer [OPTIONS] <file.md>\n\n\
         Options:\n\
           --font <family>       CSS font-family for Markdown body text\n\
           --zoom <factor>       Initial zoom, 0.25..4.0 (default: 1.0)\n\
           --width <px>          Render/window width (default: 900)\n\
           --height <px>         Initial window height (default: 700)\n\
           --render-png <path>   Render once through WebKit and save a PNG\n\
           --no-watch            Disable live reload polling\n\
           -h, --help            Show this help"
    );
}

#[cfg(target_os = "linux")]
fn render_png(input: &Path, output: &Path, options: &RenderOptions) -> ExitCode {
    let document = match render_markdown_file(input, options) {
        Ok(document) => document,
        Err(err) => {
            eprintln!("{err}");
            return ExitCode::from(1);
        }
    };

    if let Err(err) = gtk::init() {
        eprintln!("initialize GTK/WebKit: {err}");
        return ExitCode::from(1);
    }

    let web_view = markdown_web_view(options);
    web_view.set_size_request(options.normalized_width() as i32, 1);

    let window = gtk::Window::builder()
        .default_width(options.normalized_width() as i32)
        .default_height(1)
        .decorated(false)
        .opacity(0.0)
        .child(&web_view)
        .build();

    let main_loop = glib::MainLoop::new(None, false);
    let result: Rc<RefCell<Option<Result<(), String>>>> = Rc::new(RefCell::new(None));
    let output = output.to_path_buf();

    let result_for_load = Rc::clone(&result);
    let loop_for_load = main_loop.clone();
    web_view.connect_load_changed(move |web_view, event| {
        if event != webkit6::LoadEvent::Finished {
            return;
        }

        let web_view = web_view.clone();
        let result_for_snapshot = Rc::clone(&result_for_load);
        let loop_for_snapshot = loop_for_load.clone();
        let output = output.clone();
        glib::timeout_add_local_once(Duration::from_millis(300), move || {
            web_view.snapshot(
                webkit6::SnapshotRegion::FullDocument,
                webkit6::SnapshotOptions::NONE,
                Option::<&gio::Cancellable>::None,
                move |snapshot| {
                    let saved = snapshot
                        .map_err(|err| format!("snapshot Markdown page: {err}"))
                        .and_then(|texture| {
                            texture
                                .save_to_png(&output)
                                .map_err(|err| format!("save {}: {err}", output.display()))
                        });
                    *result_for_snapshot.borrow_mut() = Some(saved);
                    loop_for_snapshot.quit();
                },
            );
        });
    });

    let result_for_timeout = Rc::clone(&result);
    let loop_for_timeout = main_loop.clone();
    glib::timeout_add_seconds_local_once(RENDER_TIMEOUT_SECS, move || {
        if result_for_timeout.borrow().is_none() {
            *result_for_timeout.borrow_mut() =
                Some(Err("timed out rendering Markdown PNG".to_string()));
            loop_for_timeout.quit();
        }
    });

    window.present();
    load_document(&web_view, &document);
    main_loop.run();
    window.close();

    let final_result = result.borrow_mut().take();
    match final_result {
        Some(Ok(())) => ExitCode::SUCCESS,
        Some(Err(err)) => {
            eprintln!("{err}");
            ExitCode::from(1)
        }
        None => {
            eprintln!("render did not produce a PNG");
            ExitCode::from(1)
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn render_png(_input: &Path, _output: &Path, _options: &RenderOptions) -> ExitCode {
    eprintln!("flowmux-md-viewer --render-png requires the Linux WebKitGTK build.");
    ExitCode::from(1)
}

#[cfg(target_os = "linux")]
fn run_app(args: Args) {
    let app = adw::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    app.connect_startup(|_| install_viewer_chrome_theme());
    app.connect_activate(move |app| build_window(app, args.clone()));
    app.run_with_args(&[APP_ID]);
}

#[cfg(not(target_os = "linux"))]
fn run_app(args: Args) {
    let _ = (args.window_height, args.watch);
    eprintln!("flowmux-md-viewer requires the Linux WebKitGTK build.");
}

#[cfg(target_os = "linux")]
fn install_viewer_chrome_theme() {
    adw::StyleManager::default().set_color_scheme(adw::ColorScheme::ForceDark);
    let provider = gtk::CssProvider::new();
    provider.load_from_string(VIEWER_CHROME_CSS);
    if let Some(display) = gdk::Display::default() {
        gtk::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

#[cfg(target_os = "linux")]
struct ViewerState {
    path: PathBuf,
    options: RenderOptions,
    web_view: webkit6::WebView,
    zoom_label: gtk::Label,
    navigation: NavigationState,
    last_signature: Option<FileSignature>,
}

#[cfg(target_os = "linux")]
impl ViewerState {
    fn render(&mut self) {
        self.zoom_label
            .set_text(&format!("{:.0}%", self.options.normalized_zoom() * 100.0));
        self.web_view
            .set_zoom_level(self.options.normalized_zoom() as f64);
        self.navigation.begin_markdown_load();

        match render_markdown_file(&self.path, &self.options) {
            Ok(document) => load_document(&self.web_view, &document),
            Err(err) => self.web_view.load_html(
                &error_html(&format!("Could not render Markdown:\n{err}")),
                None,
            ),
        }
    }

    fn set_zoom(&mut self, zoom: f32) {
        self.options.zoom = zoom.clamp(0.25, 4.0);
        self.render();
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct NavigationState {
    back: gtk::Button,
    forward: gtk::Button,
    at_markdown_home: Rc<Cell<bool>>,
    loading_markdown_home: Rc<Cell<bool>>,
}

#[cfg(target_os = "linux")]
impl NavigationState {
    fn new() -> Self {
        let back = gtk::Button::from_icon_name("go-previous-symbolic");
        back.set_tooltip_text(Some("Back"));
        back.add_css_class("flat");
        let forward = gtk::Button::from_icon_name("go-next-symbolic");
        forward.set_tooltip_text(Some("Forward"));
        forward.add_css_class("flat");

        Self {
            back,
            forward,
            at_markdown_home: Rc::new(Cell::new(true)),
            loading_markdown_home: Rc::new(Cell::new(false)),
        }
    }

    fn begin_markdown_load(&self) {
        self.loading_markdown_home.set(true);
        self.at_markdown_home.set(true);
    }

    fn handle_load_event(&self, event: webkit6::LoadEvent) {
        match event {
            webkit6::LoadEvent::Started | webkit6::LoadEvent::Redirected => {
                if !self.loading_markdown_home.get() {
                    self.at_markdown_home.set(false);
                }
            }
            webkit6::LoadEvent::Finished => {
                if self.loading_markdown_home.replace(false) {
                    self.at_markdown_home.set(true);
                }
            }
            _ => {}
        }
    }

    fn update_buttons(&self, web_view: &webkit6::WebView) {
        self.back
            .set_sensitive(web_view.can_go_back() || !self.at_markdown_home.get());
        self.forward.set_sensitive(web_view.can_go_forward());
    }
}

#[cfg(target_os = "linux")]
fn build_window(app: &adw::Application, args: Args) {
    let title = args
        .path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("Markdown")
        .to_string();

    let web_view = markdown_web_view(&args.options);
    let zoom_label = gtk::Label::new(None);
    zoom_label.add_css_class("dim-label");
    let navigation = NavigationState::new();

    let state = Rc::new(RefCell::new(ViewerState {
        path: args.path.clone(),
        options: args.options.clone(),
        web_view: web_view.clone(),
        zoom_label: zoom_label.clone(),
        navigation: navigation.clone(),
        last_signature: file_signature(&args.path),
    }));

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&web_view)
        .build();
    scrolled.add_css_class("md-viewer-content");

    let overlay = gtk::Overlay::new();
    overlay.set_child(Some(&scrolled));
    overlay.add_overlay(&zoom_controls(&state, &zoom_label));

    let toolbar = adw::ToolbarView::new();
    let header = adw::HeaderBar::new();
    install_navigation_controls(&header, &state, &navigation);
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&overlay));

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title(&title)
        .default_width(args.options.normalized_width() as i32)
        .default_height(args.window_height)
        .content(&toolbar)
        .build();
    window.add_css_class("flowmux-md-viewer");
    install_viewer_keys(&window, &state, &navigation);
    state.borrow_mut().render();

    if args.watch {
        if let Some(monitor) = install_live_reload(&state) {
            let monitor_for_destroy = monitor.clone();
            window.connect_destroy(move |_| {
                monitor_for_destroy.cancel();
            });
        }
    }

    window.present();
}

#[cfg(target_os = "linux")]
fn markdown_web_view(options: &RenderOptions) -> webkit6::WebView {
    let web_view = webkit6::WebView::new();
    web_view.set_zoom_level(options.normalized_zoom() as f64);
    if let Some(settings) = webkit6::prelude::WebViewExt::settings(&web_view) {
        settings.set_enable_javascript(false);
        settings.set_enable_javascript_markup(false);
        settings.set_allow_file_access_from_file_urls(true);
        settings.set_allow_universal_access_from_file_urls(false);
        settings.set_enable_developer_extras(false);
    }
    web_view
}

#[cfg(target_os = "linux")]
fn load_document(web_view: &webkit6::WebView, document: &HtmlDocument) {
    let base_uri = document_base_uri(document);
    web_view.load_html(&document.html, base_uri.as_deref());
}

#[cfg(target_os = "linux")]
fn install_navigation_controls(
    header: &adw::HeaderBar,
    state: &Rc<RefCell<ViewerState>>,
    navigation: &NavigationState,
) {
    {
        let state = Rc::clone(state);
        let navigation = navigation.clone();
        navigation.back.connect_clicked(move |_| {
            let web_view = state.borrow().web_view.clone();
            if web_view.can_go_back() {
                web_view.go_back();
            } else if !navigation.at_markdown_home.get() {
                state.borrow_mut().render();
            }
        });
    }
    {
        let state = Rc::clone(state);
        navigation.forward.connect_clicked(move |_| {
            let web_view = state.borrow().web_view.clone();
            if web_view.can_go_forward() {
                web_view.go_forward();
            }
        });
    }
    {
        let navigation = navigation.clone();
        state
            .borrow()
            .web_view
            .connect_load_changed(move |web_view, event| {
                navigation.handle_load_event(event);
                navigation.update_buttons(web_view);
            });
    }
    {
        let web_view = state.borrow().web_view.clone();
        navigation.update_buttons(&web_view);
    }

    header.pack_start(&navigation.back);
    header.pack_start(&navigation.forward);
}

#[cfg(target_os = "linux")]
fn go_back_or_home(state: &Rc<RefCell<ViewerState>>, navigation: &NavigationState) {
    let web_view = state.borrow().web_view.clone();
    if web_view.can_go_back() {
        web_view.go_back();
    } else if !navigation.at_markdown_home.get() {
        state.borrow_mut().render();
    }
}

#[cfg(target_os = "linux")]
fn go_forward(state: &Rc<RefCell<ViewerState>>) {
    let web_view = state.borrow().web_view.clone();
    if web_view.can_go_forward() {
        web_view.go_forward();
    }
}

#[cfg(target_os = "linux")]
fn install_viewer_keys(
    window: &adw::ApplicationWindow,
    state: &Rc<RefCell<ViewerState>>,
    navigation: &NavigationState,
) {
    let key = gtk::EventControllerKey::new();
    let state_for_key = Rc::clone(state);
    let navigation_for_key = navigation.clone();
    key.connect_key_pressed(move |_, keyval, _, state_flags| {
        if state_flags.contains(gdk::ModifierType::ALT_MASK)
            && !state_flags.contains(gdk::ModifierType::CONTROL_MASK)
        {
            return match keyval {
                gdk::Key::Left => {
                    go_back_or_home(&state_for_key, &navigation_for_key);
                    glib::Propagation::Stop
                }
                gdk::Key::Right => {
                    go_forward(&state_for_key);
                    glib::Propagation::Stop
                }
                _ => glib::Propagation::Proceed,
            };
        }

        if !state_flags.contains(gdk::ModifierType::CONTROL_MASK) {
            return glib::Propagation::Proceed;
        }
        match keyval {
            gdk::Key::plus | gdk::Key::KP_Add | gdk::Key::equal => {
                let zoom = state_for_key.borrow().options.normalized_zoom() * 1.15;
                state_for_key.borrow_mut().set_zoom(zoom);
                glib::Propagation::Stop
            }
            gdk::Key::minus | gdk::Key::KP_Subtract => {
                let zoom = state_for_key.borrow().options.normalized_zoom() / 1.15;
                state_for_key.borrow_mut().set_zoom(zoom);
                glib::Propagation::Stop
            }
            gdk::Key::_0 | gdk::Key::KP_0 => {
                state_for_key.borrow_mut().set_zoom(1.0);
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(key);
}

#[cfg(any(test, target_os = "linux"))]
fn document_base_uri(document: &HtmlDocument) -> Option<String> {
    let dir = document.base_dir.as_ref()?;
    let mut uri = gio::File::for_path(dir).uri().to_string();
    if !uri.ends_with('/') {
        uri.push('/');
    }
    Some(uri)
}

#[cfg(target_os = "linux")]
fn error_html(message: &str) -> String {
    format!(
        "<!doctype html><meta charset=\"utf-8\"><body style=\"font: 14px monospace; padding: 24px\"><h1>Markdown render error</h1><pre>{}</pre></body>",
        html_escape(message)
    )
}

#[cfg(target_os = "linux")]
fn html_escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

#[cfg(target_os = "linux")]
fn zoom_controls(state: &Rc<RefCell<ViewerState>>, zoom_label: &gtk::Label) -> gtk::Box {
    let controls = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    controls.add_css_class("toolbar");
    controls.add_css_class("osd");
    controls.set_halign(gtk::Align::End);
    controls.set_valign(gtk::Align::End);
    controls.set_margin_bottom(18);
    controls.set_margin_end(18);

    let zoom_out = gtk::Button::builder()
        .label("-")
        .tooltip_text("Zoom out")
        .build();
    let zoom_in = gtk::Button::builder()
        .label("+")
        .tooltip_text("Zoom in")
        .build();

    let state_for_out = Rc::clone(state);
    zoom_out.connect_clicked(move |_| {
        let zoom = state_for_out.borrow().options.normalized_zoom() / 1.15;
        state_for_out.borrow_mut().set_zoom(zoom);
    });

    let state_for_in = Rc::clone(state);
    zoom_in.connect_clicked(move |_| {
        let zoom = state_for_in.borrow().options.normalized_zoom() * 1.15;
        state_for_in.borrow_mut().set_zoom(zoom);
    });

    controls.append(&zoom_out);
    controls.append(zoom_label);
    controls.append(&zoom_in);
    controls
}

#[cfg(target_os = "linux")]
fn install_live_reload(state: &Rc<RefCell<ViewerState>>) -> Option<gio::FileMonitor> {
    let file = gio::File::for_path(&state.borrow().path);
    let monitor = file
        .monitor_file(gio::FileMonitorFlags::NONE, None::<&gio::Cancellable>)
        .ok()?;
    let state = Rc::clone(state);
    monitor.connect_changed(move |_, _, _, event| {
        if !should_reload_for_event(event) {
            return;
        }
        let mut state = state.borrow_mut();
        let signature = file_signature(&state.path);
        if signature != state.last_signature {
            state.last_signature = signature;
            state.render();
        }
    });
    Some(monitor)
}

#[cfg(any(test, target_os = "linux"))]
fn should_reload_for_event(event: gio::FileMonitorEvent) -> bool {
    matches!(
        event,
        gio::FileMonitorEvent::Changed
            | gio::FileMonitorEvent::ChangesDoneHint
            | gio::FileMonitorEvent::Created
            | gio::FileMonitorEvent::Deleted
            | gio::FileMonitorEvent::Moved
            | gio::FileMonitorEvent::Renamed
            | gio::FileMonitorEvent::MovedIn
            | gio::FileMonitorEvent::MovedOut
    )
}

#[cfg(any(test, target_os = "linux"))]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileSignature {
    modified: Option<SystemTime>,
    len: u64,
}

#[cfg(any(test, target_os = "linux"))]
fn file_signature(path: &Path) -> Option<FileSignature> {
    let metadata = std::fs::metadata(path).ok()?;
    Some(FileSignature {
        modified: metadata.modified().ok(),
        len: metadata.len(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    use std::time::Duration;

    static ENV_LOCK: Mutex<()> = Mutex::new(());
    fn parse_args(args: &[&str]) -> Args {
        Args::parse(args.iter().map(|arg| arg.to_string())).expect("parse args")
    }

    struct EnvGuard {
        key: &'static str,
        old: Option<String>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let old = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, old }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            if let Some(value) = &self.old {
                std::env::set_var(self.key, value);
            } else {
                std::env::remove_var(self.key);
            }
        }
    }

    #[test]
    fn font_option_overrides_environment_default() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set("FLOWMUX_MD_VIEWER_FONT", "Env Font");
        let args = parse_args(&["--font", "Cli Font", "doc.md"]);
        assert_eq!(args.options.font_family.as_deref(), Some("Cli Font"));
    }

    #[test]
    fn environment_font_is_used_when_cli_font_is_absent() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set("FLOWMUX_MD_VIEWER_FONT", "Env Font");
        let args = parse_args(&["doc.md"]);
        assert_eq!(args.options.font_family.as_deref(), Some("Env Font"));
    }

    #[test]
    fn font_option_is_optional() {
        let _lock = ENV_LOCK.lock().unwrap();
        let _guard = EnvGuard::set("FLOWMUX_MD_VIEWER_FONT", "");
        std::env::remove_var("FLOWMUX_MD_VIEWER_FONT");
        let args = parse_args(&["doc.md"]);
        assert_eq!(args.options.font_family, None);
    }

    #[test]
    fn file_signature_changes_when_file_updates_or_disappears() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("live.md");
        std::fs::write(&path, "a").expect("write initial file");
        let initial = file_signature(&path).expect("initial signature");

        std::fs::write(&path, "abcd").expect("update file");
        let updated = file_signature(&path).expect("updated signature");
        assert_ne!(initial, updated);

        std::fs::remove_file(&path).expect("remove file");
        assert_ne!(Some(updated), file_signature(&path));
    }

    #[test]
    fn file_monitor_event_filter_tracks_content_and_path_changes() {
        for event in [
            gio::FileMonitorEvent::Changed,
            gio::FileMonitorEvent::ChangesDoneHint,
            gio::FileMonitorEvent::Created,
            gio::FileMonitorEvent::Deleted,
            gio::FileMonitorEvent::Moved,
            gio::FileMonitorEvent::Renamed,
            gio::FileMonitorEvent::MovedIn,
            gio::FileMonitorEvent::MovedOut,
        ] {
            assert!(should_reload_for_event(event), "{event:?} should reload");
        }

        for event in [
            gio::FileMonitorEvent::AttributeChanged,
            gio::FileMonitorEvent::PreUnmount,
            gio::FileMonitorEvent::Unmounted,
        ] {
            assert!(
                !should_reload_for_event(event),
                "{event:?} should not reload"
            );
        }
    }

    #[test]
    fn document_base_uri_points_at_markdown_directory() {
        let dir = tempfile::tempdir().expect("tempdir");
        let document = HtmlDocument {
            html: String::new(),
            base_dir: Some(dir.path().to_path_buf()),
        };
        let uri = document_base_uri(&document).expect("base uri");
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with('/'));
    }

    #[test]
    fn render_png_argument_does_not_require_gapplication_file_open() {
        let args = parse_args(&["--render-png", "out.png", "doc.md"]);
        assert_eq!(args.render_png.as_deref(), Some(Path::new("out.png")));
        assert_eq!(args.path, PathBuf::from("doc.md"));
    }

    #[test]
    fn render_timeout_is_short_enough_for_tests() {
        assert!(Duration::from_secs(RENDER_TIMEOUT_SECS as u64) <= Duration::from_secs(15));
    }
}
