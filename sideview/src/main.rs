// flurry-sideview: Servo web pages in a right-edge layer surface, driven by the flurry process over
// stdin (widgets/sideview.rs). Commands, one per line:
//   show <page> <url> | hide | focus <true|false> <ignore|exclusive> | width <px> | dark <true|false>
// and "mapped" on stdout once a show's first frame is on screen.

mod claude;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use dpi::PhysicalSize;
use euclid::Scale;
use raw_window_handle::{DisplayHandle, RawDisplayHandle, RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle};
use servo::{
    ClipboardDelegate, Code, ConsoleLogLevel, CreateNewWebViewRequest, DevicePoint, EventLoopWaker, InputEvent, Key, KeyState, KeyboardEvent, LoadStatus, Location,
    Modifiers, MouseButton, MouseButtonAction, MouseButtonEvent, MouseMoveEvent, NamedKey, NavigationRequest, Opts, Preferences, RenderingContext, ServoBuilder,
    StringRequest, Theme, UserContentManager, UserScript, WebResourceLoad, WebResourceResponse, WebView, WebViewBuilder, WebViewDelegate, WebViewId, WebViewPoint,
    WheelDelta, WheelEvent, WheelMode, WindowRenderingContext,
};
use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{
    wl_buffer, wl_compositor, wl_data_device, wl_data_device_manager, wl_data_offer, wl_keyboard, wl_output, wl_pointer, wl_region, wl_registry,
    wl_seat, wl_subcompositor, wl_subsurface, wl_surface,
};
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle, WEnum, delegate_noop};
use wayland_protocols::wp::single_pixel_buffer::v1::client::wp_single_pixel_buffer_manager_v1;
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::layer_shell::v1::client::{zwlr_layer_shell_v1, zwlr_layer_surface_v1};
use xkbcommon::xkb;

const BORDER: i32 = 3;
const MARGIN: i32 = 5;
const SAFARI_UA: &str = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/26.0 Safari/605.1.15";

// Fix clipboard pasting: media pastes get rebuilt from the system clipboard as a real paste event
const PASTE_SHIM_JS: &str = r#"(function(){
  window.__flurryPaste = function(b64, type){
    var bin = atob(b64), u = new Uint8Array(bin.length);
    for (var i = 0; i < bin.length; i++) u[i] = bin.charCodeAt(i);
    var dt = new DataTransfer();
    dt.items.add(new File([u], 'pasted.' + type.split('/')[1], { type: type }));
    var t = document.activeElement || document.body;
    t.dispatchEvent(new ClipboardEvent('paste', { clipboardData: dt, bubbles: true, cancelable: true }));
  };
})();"#;

fn rgba(hex: u32) -> [u32; 4] {
    let c = |v: u32| ((v as u64 * u32::MAX as u64) / 255) as u32;
    [c((hex >> 16) & 0xff), c((hex >> 8) & 0xff), c(hex & 0xff), u32::MAX]
}
const DARK4: u32 = 0x4c566a;
const BLUE3: u32 = 0x5e81ac;

#[derive(Clone)]
struct Waker(i32);
impl EventLoopWaker for Waker {
    fn clone_box(&self) -> Box<dyn EventLoopWaker> {
        Box::new(self.clone())
    }
    fn wake(&self) {
        let one = 1u64;
        unsafe { libc::write(self.0, (&raw const one).cast(), 8) };
    }
}

#[derive(Default)]
struct Frame {
    dirty: Cell<bool>,
    /// console messages addressed to us (\u{1} prefix), with their page: the claude page's channel out
    messages: RefCell<Vec<(WebView, String)>>,
    /// the only page allowed to use that channel: any site can write to its console
    claude_url: Option<url::Url>,
    /// the web views made for it: the only ones that may read local files, and only its own
    claude_views: RefCell<Vec<WebViewId>>,
}

/// Local files are for the claude page's own directory; every other load of one fails.
fn filter_files(load: WebResourceLoad, page: Option<&url::Url>) {
    let url = &load.request().url;
    let own = page.and_then(|p| p.join(".").ok()).is_some_and(|dir| url.as_str().starts_with(dir.as_str()));
    if url.scheme() == "file" && !own {
        let url = url.clone();
        load.intercept(WebResourceResponse::new(url)).cancel();
    }
}

/// Pages write the clipboard as they like, but read it only to paste: within a second of a Ctrl+V
/// given to that page.
#[derive(Default)]
struct Clipboard(std::cell::OnceCell<Option<RefCell<arboard::Clipboard>>>);
impl Clipboard {
    fn with<T>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Option<T> {
        let c = self.0.get_or_init(|| arboard::Clipboard::new().ok().map(RefCell::new)).as_ref()?;
        f(&mut c.borrow_mut()).ok()
    }
}
impl ClipboardDelegate for Clipboard {
    fn clear(&self, _: WebView) {
        self.with(|c| c.clear());
    }
    fn get_text(&self, webview: WebView, request: StringRequest) {
        let pasting = PASTE_AT.with(|p| p.get()).is_some_and(|(id, at)| id == webview.id() && at.elapsed() < Duration::from_secs(1));
        match pasting.then(|| self.with(|c| c.get_text())).flatten() {
            Some(text) => request.success(text),
            None => request.failure(String::new()),
        }
    }
    fn set_text(&self, _: WebView, text: String) {
        self.with(|c| c.set_text(text));
    }
}

impl WebViewDelegate for Frame {
    fn show_console_message(&self, webview: WebView, _: ConsoleLogLevel, message: String) {
        if let Some(m) = message.strip_prefix('\u{1}')
            && self.claude_url.is_some()
            && webview.url() == self.claude_url
        {
            self.messages.borrow_mut().push((webview, m.to_string()));
        }
    }
    fn request_navigation(&self, webview: WebView, nav: NavigationRequest) {
        // the claude page stays itself; web pages stay on the web (no local files, so no way into
        // the claude page's channel either)
        let ok = if self.claude_url.is_some() && webview.url() == self.claude_url {
            Some(&nav.url) == self.claude_url.as_ref()
        } else {
            matches!(nav.url.scheme(), "http" | "https" | "about" | "blob" | "javascript")
        };
        if ok { nav.allow() } else { nav.deny() }
    }
    fn load_web_resource(&self, webview: WebView, load: WebResourceLoad) {
        let own = self.claude_views.borrow().contains(&webview.id());
        filter_files(load, self.claude_url.as_ref().filter(|_| own));
    }
    fn notify_new_frame_ready(&self, _: WebView) {
        self.dirty.set(true);
    }
    fn request_create_new(&self, parent: WebView, request: CreateNewWebViewRequest) {
        // new windows open in the default browser: catch the popup's first navigation; a page
        // already waiting on two gets none (window.open returns null)
        if POPUPS.with(|p| p.borrow().len()) >= 2 {
            return;
        }
        RENDERING.with(|r| {
            if let Some(rc) = r.borrow().clone() {
                let aux = request.builder(rc).delegate(Rc::new(Popup(parent.id()))).build();
                aux.hide(); // never in the scene
                POPUPS.with(|p| p.borrow_mut().push((aux, Instant::now())));
            }
        });
    }
}

/// Web links only: pages must not be able to hand local files or other schemes to their handlers.
/// Only from the page on screen, right after a click or key there (a hidden page can't open
/// anything), and at most one a second, so a page can't flood the browser.
fn open_external(url: &str, from: WebViewId) {
    let now = Instant::now();
    if (url.starts_with("https://") || url.starts_with("http://"))
        && INPUT_AT.with(|i| i.get()).is_some_and(|(id, at)| id == from && now < at + Duration::from_secs(1))
        && LAST_OPEN.with(|l| l.get().is_none_or(|at| now >= at + Duration::from_secs(1)))
        && let Ok(mut child) = std::process::Command::new("xdg-open").arg(url).spawn()
    {
        LAST_OPEN.with(|l| l.set(Some(now)));
        std::thread::spawn(move || child.wait()); // reaped
    }
}

/// A page's new window, by the page that opened it.
struct Popup(WebViewId);
impl WebViewDelegate for Popup {
    fn load_web_resource(&self, _: WebView, load: WebResourceLoad) {
        filter_files(load, None);
    }
    fn request_navigation(&self, webview: WebView, nav: NavigationRequest) {
        let url = nav.url.to_string();
        nav.deny();
        if url != "about:blank" {
            open_external(&url, self.0);
            POPUPS.with(|p| p.borrow_mut().retain(|(wv, _)| *wv != webview));
        }
    }
}

thread_local! {
    static RENDERING: RefCell<Option<Rc<dyn RenderingContext>>> = const { RefCell::new(None) };
    /// popups waiting for their first navigation, dropped after a few seconds without one
    static POPUPS: RefCell<Vec<(WebView, Instant)>> = const { RefCell::new(Vec::new()) };
    static LAST_OPEN: Cell<Option<Instant>> = const { Cell::new(None) };
    /// the last click or key press given to the page on screen
    static INPUT_AT: Cell<Option<(WebViewId, Instant)>> = const { Cell::new(None) };
    /// the last Ctrl+V given to a page
    static PASTE_AT: Cell<Option<(WebViewId, Instant)>> = const { Cell::new(None) };
}

struct State {
    size: (u32, u32),
    configured: bool,
    scale: i32,
    ctx: xkb::Context,
    xkb: Option<xkb::State>,
    repeat: (i32, i32),
    pointer: (f64, f64),
    has_keyboard: bool,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    pointer_dev: Option<wl_pointer::WlPointer>,
    events: Vec<InputEvent>,
    /// (key, pressed, repeat)
    keys: Vec<(xkb::Keycode, bool, bool)>,
    repeat_key: Option<(xkb::Keycode, Instant)>,
    /// announced offers not yet claimed by a selection or drag
    offers: Vec<(wl_data_offer::WlDataOffer, Vec<String>)>,
    selection: Option<(wl_data_offer::WlDataOffer, Vec<String>)>,
    /// drags are not taken; their offer is only held until they leave
    dnd: Option<wl_data_offer::WlDataOffer>,
    closed: bool,
}

impl State {
    /// Keyboard focus gone (or the panel hidden): nothing keeps repeating, presses not yet handled
    /// are dropped (releases still go through).
    fn release_keys(&mut self) {
        self.repeat_key = None;
        self.keys.retain(|k| !k.1);
    }
}

struct Surfaces {
    border: wl_surface::WlSurface,
    content: wl_surface::WlSurface,
    viewport: wp_viewport::WpViewport,
    layer: Option<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1>,
}

fn named(sym: xkb::Keysym) -> Option<NamedKey> {
    use xkb::Keysym as K;
    Some(match sym {
        K::Return | K::KP_Enter => NamedKey::Enter,
        K::BackSpace => NamedKey::Backspace,
        K::Delete => NamedKey::Delete,
        K::Tab | K::ISO_Left_Tab => NamedKey::Tab,
        K::Escape => NamedKey::Escape,
        K::Left => NamedKey::ArrowLeft,
        K::Right => NamedKey::ArrowRight,
        K::Up => NamedKey::ArrowUp,
        K::Down => NamedKey::ArrowDown,
        K::Home => NamedKey::Home,
        K::End => NamedKey::End,
        K::Prior => NamedKey::PageUp,
        K::Next => NamedKey::PageDown,
        K::Shift_L | K::Shift_R => NamedKey::Shift,
        K::Control_L | K::Control_R => NamedKey::Control,
        K::Alt_L | K::Alt_R => NamedKey::Alt,
        K::Super_L | K::Super_R => NamedKey::Meta,
        K::F5 => NamedKey::F5,
        _ => return None,
    })
}

fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let Ok(conn) = Connection::connect_to_env() else { return };
    let Ok((globals, mut queue)) = registry_queue_init::<State>(&conn) else { return };
    let qh = queue.handle();
    let compositor: wl_compositor::WlCompositor = globals.bind(&qh, 4..=6, ()).expect("wl_compositor");
    let subcompositor: wl_subcompositor::WlSubcompositor = globals.bind(&qh, 1..=1, ()).expect("wl_subcompositor");
    let shell: zwlr_layer_shell_v1::ZwlrLayerShellV1 = globals.bind(&qh, 4..=4, ()).expect("layer shell");
    let viewporter: wp_viewporter::WpViewporter = globals.bind(&qh, 1..=1, ()).expect("viewporter");
    let pixels: wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1 = globals.bind(&qh, 1..=1, ()).expect("single pixel");
    let seat: wl_seat::WlSeat = globals.bind(&qh, 5..=9, ()).expect("wl_seat");
    let ddm: Option<wl_data_device_manager::WlDataDeviceManager> = globals.bind(&qh, 3..=3, ()).ok();
    let _dd = ddm.as_ref().map(|m| m.get_data_device(&seat, &qh, ()));

    let border = compositor.create_surface(&qh, ());
    let content = compositor.create_surface(&qh, ());
    let sub = subcompositor.get_subsurface(&content, &border, &qh, ());
    sub.set_desync();
    sub.set_position(BORDER, BORDER);
    // input goes to the border surface, whose coordinates the pointer handling assumes
    let none = compositor.create_region(&qh, ());
    content.set_input_region(Some(&none));
    none.destroy();
    content.commit();
    let viewport = viewporter.get_viewport(&border, &qh, ());
    let mut s = Surfaces { border, content, viewport, layer: None };

    let mut state = State {
        size: (400, 1),
        configured: false,
        // the outputs' until the surface is told its own
        scale: 1,
        ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
        xkb: None,
        repeat: (25, 600),
        pointer: (0.0, 0.0),
        has_keyboard: false,
        keyboard: None,
        pointer_dev: None,
        events: vec![],
        keys: vec![],
        repeat_key: None,
        offers: vec![],
        selection: None,
        dnd: None,
        closed: false,
    };
    // the outputs' scale to start with: just their first announcement
    let outputs: Vec<wl_output::WlOutput> = globals.contents().with_list(|list| {
        list.iter()
            .filter(|g| g.interface == "wl_output" && g.version >= 2)
            .map(|g| globals.registry().bind(g.name, g.version.min(4), &qh, ()))
            .collect()
    });
    if queue.roundtrip(&mut state).is_err() {
        return;
    }
    for o in outputs.into_iter().filter(|o| o.version() >= 3) {
        o.release();
    }

    let wake_fd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("flurry-sideview"));
    let opts = Opts { config_dir: data, ..Default::default() };
    let mut prefs = Preferences::default();
    prefs.dom_intersection_observer_enabled = true;
    prefs.dom_async_clipboard_enabled = true;
    prefs.dom_fontface_enabled = true;
    prefs.layout_variable_fonts_enabled = true;
    prefs.user_agent = SAFARI_UA.into();
    prefs.editing_caret_blink_time = 0; // steady caret
    prefs.shell_background_color_rgba = [0x2e as f64 / 255.0, 0x34 as f64 / 255.0, 0x40 as f64 / 255.0, 1.0];
    let servo = ServoBuilder::default().opts(opts).preferences(prefs).event_loop_waker(Box::new(Waker(wake_fd))).build();
    let scripts = Rc::new(UserContentManager::new(&servo));
    scripts.add_script(Rc::new(UserScript::new(PASTE_SHIM_JS.into(), None)));

    let mut width: u32 = 400;
    let mut focused = false;
    let mut zone = "ignore".to_string();
    let mut dark = true;
    let mut pages: HashMap<String, WebView> = HashMap::new();
    let mut current: Option<String> = None;
    let mut visible = false;
    // configured since the last map: presenting before that blocks in eglSwapBuffers on a frame
    // callback the unmapped surface never gets (re-shows hung until the helper was killed)
    let mut mapped = false;
    // deadline for a page just shown to produce its first frame (painting before it renders the
    // previous scene)
    let mut awaiting: Option<Instant> = None;
    let mut skip_blank = false;
    let mut rendering: Option<Rc<WindowRenderingContext>> = None;
    // one Claude Code process for every page showing flurry:claude: replies go to the page that
    // sent the last message (two such pages share the conversation)
    let mut chat = claude::Chat::default();
    let mut chat_view: Option<WebView> = None;
    let frame = Rc::new(Frame { claude_url: claude::page_url(), ..Default::default() });
    let clipboard = Rc::new(Clipboard::default());
    // the panel's border waits for the first frame presented after a map, and flurry for "mapped"
    let mut bordered = false;
    let mut announce = false;
    let mut prewarm = false;
    // a show before the first configure (no rendering context yet), run once there is one
    let mut pending_show: Option<String> = None;

    let stdin_fd = 0;
    unsafe { libc::fcntl(stdin_fd, libc::F_SETFL, libc::fcntl(stdin_fd, libc::F_GETFL) | libc::O_NONBLOCK) };
    let mut input: Vec<u8> = Vec::new();
    let wl_fd = conn.as_fd().as_raw_fd();

    let map = |s: &mut Surfaces, width: u32, focused: bool, zone: &str| {
        if s.layer.is_none() {
            let l = shell.get_layer_surface(&s.border, None, zwlr_layer_shell_v1::Layer::Overlay, "sideview".into(), &qh, ());
            use zwlr_layer_surface_v1::Anchor;
            l.set_anchor(Anchor::Top | Anchor::Bottom | Anchor::Right);
            l.set_margin(MARGIN, 0, MARGIN, 0);
            s.layer = Some(l);
        }
        let l = s.layer.as_ref().unwrap();
        l.set_size(width, 0);
        l.set_keyboard_interactivity(if focused {
            zwlr_layer_surface_v1::KeyboardInteractivity::Exclusive
        } else {
            zwlr_layer_surface_v1::KeyboardInteractivity::OnDemand
        });
        l.set_exclusive_zone(if zone == "exclusive" { width as i32 } else { -1 });
        s.border.commit();
    };
    let border_buffers: RefCell<Vec<wl_buffer::WlBuffer>> = RefCell::default();
    let paint_border = |s: &Surfaces, color: u32| {
        let [r, g, b, a] = rgba(color);
        let buf = pixels.create_u32_rgba_buffer(r, g, b, a, &qh, ());
        s.border.attach(Some(&buf), 0, 0);
        s.border.damage(0, 0, i32::MAX, i32::MAX);
        s.border.commit();
        // the old one is replaced by this commit; destroying before the commit would unmap
        let mut bufs = border_buffers.borrow_mut();
        for old in bufs.drain(..) {
            old.destroy();
        }
        bufs.push(buf);
    };

    while !state.closed {
        // commands from flurry
        let mut buf = [0u8; 4096];
        loop {
            let n = unsafe { libc::read(stdin_fd, buf.as_mut_ptr().cast(), buf.len()) };
            if n == 0 {
                return; // flurry is gone
            }
            if n < 0 {
                break;
            }
            input.extend_from_slice(&buf[..n as usize]);
        }
        let mut lines: Vec<String> = rendering.as_ref().and_then(|_| pending_show.take()).into_iter().collect();
        while let Some(i) = input.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = input.drain(..=i).collect();
            lines.push(String::from_utf8_lossy(&line).trim().to_string());
        }
        for line in lines {
            let mut w = line.splitn(3, ' ');
            match (w.next(), w.next(), w.next()) {
                (Some("show"), Some(name), Some(url)) => {
                    let is_claude = url == claude::URL;
                    let target = || if is_claude { claude::page_url() } else { url::Url::parse(url).ok() };
                    if !pages.contains_key(name) && target().is_none() {
                        continue; // nothing to show
                    }
                    // a visible page swapped for another: its hide builds a frame of neither page
                    let swap = visible && current.as_deref().is_some_and(|c| c != name && pages.contains_key(c));
                    if !visible {
                        visible = true;
                        map(&mut s, width, focused, &zone);
                    }
                    announce = true;
                    INPUT_AT.with(|i| i.set(None)); // input given to the page shown before doesn't count
                    let Some(rc) = &rendering else {
                        // web views are created once the first configure gives the surface a size
                        pending_show = Some(line.clone());
                        continue;
                    };
                    current = Some(name.to_string());
                    if !pages.contains_key(name)
                        && let Some(u) = target()
                    {
                        let wv = WebViewBuilder::new(&servo, rc.clone())
                            .url(u)
                            .hidpi_scale_factor(Scale::new(state.scale as f32))
                            .user_content_manager(scripts.clone())
                            .delegate(frame.clone())
                            .clipboard_delegate(clipboard.clone())
                            .build();
                        wv.notify_theme_change(if dark { Theme::Dark } else { Theme::Light });
                        if is_claude {
                            frame.claude_views.borrow_mut().push(wv.id());
                        }
                        pages.insert(name.to_string(), wv);
                    }
                    // first reply without waiting for Claude Code to start (once the page is up)
                    prewarm |= is_claude;
                    // hide before show: the shared scene never holds two pages at once
                    for (n, wv) in &pages {
                        if n != name {
                            wv.hide();
                            wv.set_focused(false);
                        }
                    }
                    if let Some(wv) = pages.get(name) {
                        wv.show();
                        wv.set_focused(true);
                    }
                    // paint on the frame the show produces, not the stale scene still built
                    frame.dirty.set(false);
                    skip_blank = swap;
                    let loading = pages.get(name).is_some_and(|wv| wv.load_status() != LoadStatus::Complete);
                    awaiting = Some(Instant::now() + Duration::from_millis(if loading { 1000 } else { 150 }));
                }
                (Some("hide"), _, _) => {
                    // a show still waiting for the first configure is cancelled, not replayed after
                    pending_show = None;
                    state.release_keys();
                    INPUT_AT.with(|i| i.set(None));
                    (bordered, announce) = (false, false);
                    // hidden web views drop out of the scene and are throttled (no animations or
                    // rAF, timers at most once a second); losing focus fires blur, a cue for pages
                    // to pause themselves
                    for wv in pages.values() {
                        wv.hide();
                        wv.set_focused(false);
                    }
                    awaiting = None;
                    visible = false;
                    mapped = false;
                    if let Some(l) = s.layer.take() {
                        l.destroy();
                    }
                    // an unhandled configure of that layer surface must not count for the next one
                    state.configured = false;
                    s.border.attach(None, 0, 0);
                    s.border.commit();
                }
                (Some("focus"), Some(f), Some(z)) => {
                    focused = f == "true";
                    zone = z.to_string();
                    if visible {
                        map(&mut s, width, focused, &zone);
                    }
                }
                (Some("width"), Some(px), _) => {
                    width = px.parse().unwrap_or(400);
                    if visible {
                        map(&mut s, width, focused, &zone);
                    }
                }
                (Some("dark"), Some(d), _) => {
                    dark = d == "true";
                    for wv in pages.values() {
                        wv.notify_theme_change(if dark { Theme::Dark } else { Theme::Light });
                    }
                }
                _ => {}
            }
        }

        // configure: (re)size the web content to the surface minus the border
        if state.configured && visible {
            state.configured = false;
            let (w, h) = state.size;
            s.viewport.set_destination(w as i32, h as i32);
            let cw = (w as i32 - BORDER).max(1) as u32;
            let ch = (h as i32 - 2 * BORDER).max(1) as u32;
            s.content.set_buffer_scale(state.scale);
            for wv in pages.values() {
                wv.set_hidpi_scale_factor(Scale::new(state.scale as f32));
            }
            let phys = PhysicalSize::new(cw * state.scale as u32, ch * state.scale as u32);
            match &rendering {
                Some(rc) => {
                    // WebView::resize resizes the shared context and every web view; resizing the
                    // context first would make it a no-op (sizes already equal)
                    match pages.values().next() {
                        Some(wv) => wv.resize(phys),
                        None => rc.resize(phys),
                    }
                }
                None => {
                    let display = RawDisplayHandle::Wayland(WaylandDisplayHandle::new(NonNull::new(conn.backend().display_ptr().cast()).unwrap()));
                    let window = RawWindowHandle::Wayland(WaylandWindowHandle::new(NonNull::new(s.content.id().as_ptr().cast()).unwrap()));
                    if let Ok(rc) =
                        WindowRenderingContext::new(unsafe { DisplayHandle::borrow_raw(display) }, unsafe { WindowHandle::borrow_raw(window) }, phys)
                    {
                        let rc = Rc::new(rc);
                        let _ = rc.make_current();
                        RENDERING.with(|r| *r.borrow_mut() = Some(rc.clone()));
                        rendering = Some(rc);
                    }
                }
            }
            mapped = true;
            if bordered {
                paint_border(&s, if state.has_keyboard { BLUE3 } else { DARK4 }); // commits the new size
            }
            if awaiting.is_none() {
                frame.dirty.set(true);
            }
            continue; // run a pending show now
        }

        // keyboard: repeat, Ctrl+R reload, media paste
        let now = Instant::now();
        if let Some((code, at)) = state.repeat_key
            && now >= at
        {
            state.keys.push((code, true, true));
            state.repeat_key = Some((code, now + Duration::from_millis((1000 / state.repeat.0.max(1)) as u64)));
        }
        let keys: Vec<(xkb::Keycode, bool, bool)> = state.keys.drain(..).collect();
        for (code, pressed, repeat) in keys {
            let Some(x) = &state.xkb else { continue };
            let sym = x.key_get_one_sym(code);
            let active = |m| x.mod_name_is_active(m, xkb::STATE_MODS_EFFECTIVE);
            let ctrl = active(xkb::MOD_NAME_CTRL);
            // a repeat rate of 0 turns repeating off
            if pressed && state.repeat.0 > 0 && x.get_keymap().key_repeats(code) {
                if state.repeat_key.map(|r| r.0) != Some(code) {
                    state.repeat_key = Some((code, now + Duration::from_millis(state.repeat.1 as u64)));
                }
            } else if !pressed && state.repeat_key.map(|r| r.0) == Some(code) {
                state.repeat_key = None;
            }
            let wv = current.as_ref().and_then(|c| pages.get(c));
            if pressed && ctrl && sym == xkb::Keysym::r {
                if !repeat && let Some(wv) = wv {
                    wv.reload(); // ctrl+R to reload page
                    if frame.claude_url.is_some() && wv.url() == frame.claude_url {
                        // the reloaded chat starts empty: so does its conversation
                        chat.reset();
                        chat.prewarm();
                        chat_view = None;
                    }
                }
                continue;
            }
            if pressed
                && ctrl
                && sym == xkb::Keysym::v
                && let Some(wv) = wv
            {
                PASTE_AT.with(|p| p.set(Some((wv.id(), now))));
            }
            if pressed
                && !repeat
                && ctrl
                && sym == xkb::Keysym::v
                && let Some((offer, mimes)) = &state.selection
                && let Some(mime) = mimes.iter().find(|m| m.starts_with("image/") || m.starts_with("video/"))
                && let Some(wv) = wv
            {
                let bytes = receive(&conn, offer, mime);
                if !bytes.is_empty() {
                    wv.evaluate_javascript(format!("window.__flurryPaste({:?}, {:?})", base64(&bytes), mime), |_| {});
                    continue;
                }
            }
            let text: String = sym.key_char().filter(|c| !c.is_control()).map(String::from).unwrap_or_default();
            let key = match named(sym) {
                Some(n) => Key::Named(n),
                None if !text.is_empty() => Key::Character(text),
                None => continue,
            };
            let mut modifiers = Modifiers::empty();
            for (name, m) in [
                (xkb::MOD_NAME_SHIFT, Modifiers::SHIFT),
                (xkb::MOD_NAME_CTRL, Modifiers::CONTROL),
                (xkb::MOD_NAME_ALT, Modifiers::ALT),
                (xkb::MOD_NAME_LOGO, Modifiers::META),
            ] {
                if active(name) {
                    modifiers |= m;
                }
            }
            let ks = if pressed { KeyState::Down } else { KeyState::Up };
            state.events.push(InputEvent::Keyboard(KeyboardEvent::new_without_event(ks, key, Code::Unidentified, Location::Standard, modifiers, repeat, false)));
        }

        servo.spin_event_loop();
        // the claude page: its requests out, Claude Code's events back in
        for (wv, m) in frame.messages.take() {
            if m == "claude:new" {
                chat.reset();
                chat.prewarm(); // the next chat starts without a process launch
            } else if m == "claude:stop" {
                chat.interrupt();
            } else if let Some(parts) = m.strip_prefix("claude:user") {
                // only a well-formed user message reaches Claude Code
                let sent = claude::user_message(parts).is_some_and(|line| chat.send(&line));
                if !sent {
                    wv.evaluate_javascript(r#"window.__claude(['{"type":"exit"}'])"#, |_| {}); // claude not found: unblock the page
                }
                chat_view = Some(wv);
            } else if let Some(url) = m.strip_prefix("open:") {
                open_external(url, wv.id());
            } else {
                eprintln!("flurry-sideview: {m}"); // page errors (window.onerror)
            }
        }
        // one script-thread round trip per read, however many events arrived; the lines go in as
        // data (string literals), parsed by the page
        let lines = chat.read();
        if let Some(wv) = &chat_view
            && !lines.is_empty()
        {
            let lines: Vec<String> = lines.iter().map(|l| claude::quote(l)).collect();
            wv.evaluate_javascript(format!("window.__claude([{}])", lines.join(",")), |_| {});
        }
        POPUPS.with(|p| p.borrow_mut().retain(|(_, at)| at.elapsed() < Duration::from_secs(5)));
        if let Some(wv) = current.as_ref().and_then(|c| pages.get(c)) {
            for ev in state.events.drain(..) {
                let real = match &ev {
                    InputEvent::MouseButton(e) => e.action == MouseButtonAction::Down,
                    InputEvent::Keyboard(e) => e.event.state == KeyState::Down && !e.event.repeat,
                    _ => false,
                };
                if real && visible {
                    INPUT_AT.with(|i| i.set(Some((wv.id(), Instant::now()))));
                }
                wv.notify_input_event(ev);
            }
            if awaiting.is_some() && frame.dirty.get() {
                if wv.load_status() != LoadStatus::Complete {
                    frame.dirty.set(false); // still loading: its frames are blank, the old one stays up
                } else if skip_blank {
                    // the first frame after a swap is the empty in-between scene; the shown page's
                    // follows, unless both arrived together (then the renderer already holds it)
                    frame.dirty.set(false);
                    skip_blank = false;
                    awaiting = Some(Instant::now() + Duration::from_millis(16));
                }
            }
            if awaiting.is_some_and(|at| Instant::now() >= at) {
                frame.dirty.set(true); // no frame came (nothing changed): paint what there is
                skip_blank = false;
            }
            if visible && mapped
                && frame.dirty.replace(false)
                && let Some(rc) = &rendering
            {
                if !bordered {
                    // maps the panel, with this frame: never the stale one before it; sent ahead of
                    // the swap, which may wait on the frame callback of one shown before the hide
                    bordered = true;
                    paint_border(&s, if state.has_keyboard { BLUE3 } else { DARK4 });
                    let _ = conn.flush();
                }
                wv.paint();
                rc.present();
                awaiting = None;
                if std::mem::take(&mut announce) {
                    use std::io::Write;
                    let mut out = std::io::stdout();
                    let _ = out.write_all(b"mapped\n").and_then(|_| out.flush());
                }
                if std::mem::take(&mut prewarm) {
                    chat.prewarm();
                }
            }
        } else {
            state.events.clear();
        }

        // a protocol error kills the connection: exit so flurry respawns us on the next command
        if queue.dispatch_pending(&mut state).is_err() {
            return;
        }
        let _ = conn.flush();
        let Some(guard) = conn.prepare_read() else { continue };
        let mut fds = [
            libc::pollfd { fd: wl_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: wake_fd, events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: stdin_fd, events: libc::POLLIN, revents: 0 },
            // -1 (no chat process, or nothing to write) is ignored by poll
            libc::pollfd { fd: chat.fd(), events: libc::POLLIN, revents: 0 },
            libc::pollfd { fd: chat.write_fd(), events: libc::POLLOUT, revents: 0 },
        ];
        let wake_at = match (state.repeat_key.map(|r| r.1), awaiting.filter(|_| visible && mapped)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        };
        let timeout = wake_at.map(|at| at.saturating_duration_since(Instant::now()).as_millis() as i32).unwrap_or(-1);
        unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, timeout) };
        if fds[0].revents & libc::POLLIN != 0 {
            if guard.read().is_err() {
                return;
            }
        } else {
            drop(guard);
        }
        if fds[1].revents & libc::POLLIN != 0 {
            let mut b = 0u64;
            unsafe { libc::read(wake_fd, (&raw mut b).cast(), 8) };
        }
        if fds[4].revents != 0 {
            chat.flush(); // a failure shows as the process's end on its stdout
        }
        let had_keyboard = state.has_keyboard;
        if queue.dispatch_pending(&mut state).is_err() {
            return;
        }
        if state.has_keyboard != had_keyboard && visible && bordered {
            // #sideview:focus-within
            paint_border(&s, if state.has_keyboard { BLUE3 } else { DARK4 });
        }
    }
}

fn receive(conn: &Connection, offer: &wl_data_offer::WlDataOffer, mime: &str) -> Vec<u8> {
    let mut fds = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
        return vec![];
    }
    let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
    offer.receive(mime.into(), wr.as_fd());
    drop(wr);
    let _ = conn.flush();
    // a slow or endless source must not hang the panel: what isn't all there in time is dropped
    const CAP: usize = 20 << 20;
    let deadline = Instant::now() + Duration::from_millis(500);
    unsafe { libc::fcntl(rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
    let mut file = std::fs::File::from(rd);
    let mut out = vec![];
    let mut chunk = [0u8; 65536];
    loop {
        match file.read(&mut chunk) {
            Ok(0) => return out,
            Ok(n) if out.len() + n <= CAP => out.extend_from_slice(&chunk[..n]),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
                let mut pfd = libc::pollfd { fd: file.as_raw_fd(), events: libc::POLLIN, revents: 0 };
                if left <= 0 || unsafe { libc::poll(&mut pfd, 1, left) } <= 0 {
                    return vec![];
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            _ => return vec![],
        }
    }
}

fn base64(b: &[u8]) -> String {
    const T: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                s.push(T[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                s.push('=');
            }
        }
    }
    s
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}
delegate_noop!(State: ignore wl_compositor::WlCompositor);
delegate_noop!(State: ignore wl_subcompositor::WlSubcompositor);
delegate_noop!(State: ignore wl_subsurface::WlSubsurface);
delegate_noop!(State: ignore wl_region::WlRegion);
delegate_noop!(State: ignore wl_buffer::WlBuffer);
delegate_noop!(State: ignore wp_viewporter::WpViewporter);
delegate_noop!(State: ignore wp_viewport::WpViewport);
delegate_noop!(State: ignore wp_single_pixel_buffer_manager_v1::WpSinglePixelBufferManagerV1);
delegate_noop!(State: ignore wl_data_device_manager::WlDataDeviceManager);

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, ev: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        // the largest: downscaled is sharper than upscaled until the surface knows its own
        if let wl_output::Event::Scale { factor } = ev {
            state.scale = state.scale.max(factor);
        }
    }
}
delegate_noop!(State: zwlr_layer_shell_v1::ZwlrLayerShellV1);

impl Dispatch<wl_surface::WlSurface, ()> for State {
    fn event(state: &mut Self, _: &wl_surface::WlSurface, ev: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_surface::Event::PreferredBufferScale { factor } = ev
            && factor != state.scale
        {
            state.scale = factor;
            state.configured = true;
        }
    }
}

impl Dispatch<zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ()> for State {
    fn event(state: &mut Self, l: &zwlr_layer_surface_v1::ZwlrLayerSurfaceV1, ev: zwlr_layer_surface_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            zwlr_layer_surface_v1::Event::Configure { serial, width, height } => {
                l.ack_configure(serial);
                state.size = (width, height);
                state.configured = true;
            }
            zwlr_layer_surface_v1::Event::Closed => state.closed = true,
            _ => {}
        }
    }
}

impl Dispatch<wl_seat::WlSeat, ()> for State {
    fn event(state: &mut Self, seat: &wl_seat::WlSeat, ev: wl_seat::Event, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let wl_seat::Event::Capabilities { capabilities: WEnum::Value(caps) } = ev {
            if !caps.contains(wl_seat::Capability::Keyboard) {
                if let Some(k) = state.keyboard.take() {
                    k.release();
                }
                state.has_keyboard = false;
                state.release_keys();
            } else if state.keyboard.is_none() {
                state.keyboard = Some(seat.get_keyboard(qh, ()));
            }
            if !caps.contains(wl_seat::Capability::Pointer) {
                if let Some(p) = state.pointer_dev.take() {
                    p.release();
                }
            } else if state.pointer_dev.is_none() {
                state.pointer_dev = Some(seat.get_pointer(qh, ()));
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, ()> for State {
    fn event(state: &mut Self, _: &wl_keyboard::WlKeyboard, ev: wl_keyboard::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_keyboard::Event::Keymap { fd, size, .. } => {
                let km = unsafe { xkb::Keymap::new_from_fd(&state.ctx, fd, size as usize, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS) };
                if let Ok(Some(km)) = km {
                    state.xkb = Some(xkb::State::new(&km));
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some(x) = &mut state.xkb {
                    x.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::RepeatInfo { rate, delay } => state.repeat = (rate, delay),
            wl_keyboard::Event::Enter { .. } => state.has_keyboard = true,
            wl_keyboard::Event::Leave { .. } => {
                state.has_keyboard = false;
                state.release_keys();
            }
            wl_keyboard::Event::Key { key, state: ks, .. } => {
                state.keys.push((xkb::Keycode::new(key + 8), ks == WEnum::Value(wl_keyboard::KeyState::Pressed), false))
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_pointer::WlPointer, ()> for State {
    fn event(state: &mut Self, _: &wl_pointer::WlPointer, ev: wl_pointer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        let s = state.scale as f64;
        // pointer coordinates are on the border surface; the page starts BORDER px in
        let point = |p: (f64, f64)| WebViewPoint::Device(DevicePoint::new(((p.0 - BORDER as f64) * s) as f32, ((p.1 - BORDER as f64) * s) as f32));
        match ev {
            wl_pointer::Event::Enter { surface_x, surface_y, .. } | wl_pointer::Event::Motion { surface_x, surface_y, .. } => {
                state.pointer = (surface_x, surface_y);
                state.events.push(InputEvent::MouseMove(MouseMoveEvent::new(point(state.pointer))));
            }
            wl_pointer::Event::Button { button, state: bs, .. } => {
                let button = match button {
                    0x110 => MouseButton::Primary,
                    0x111 => MouseButton::Secondary,
                    0x112 => MouseButton::Auxiliary,
                    0x113 => MouseButton::Back,
                    0x114 => MouseButton::Forward,
                    b => MouseButton::Other(b as u16),
                };
                let action = if bs == WEnum::Value(wl_pointer::ButtonState::Pressed) { MouseButtonAction::Down } else { MouseButtonAction::Up };
                state.events.push(InputEvent::MouseButton(MouseButtonEvent::new(action, button, point(state.pointer))));
            }
            wl_pointer::Event::Axis { axis, value, .. } => {
                let (x, y) = match axis {
                    WEnum::Value(wl_pointer::Axis::HorizontalScroll) => (-value * s, 0.0),
                    _ => (0.0, -value * s),
                };
                let delta = WheelDelta { x, y, z: 0.0, mode: WheelMode::DeltaPixel };
                state.events.push(InputEvent::Wheel(WheelEvent::new(delta, point(state.pointer))));
            }
            _ => {}
        }
    }
}

impl Dispatch<wl_data_device::WlDataDevice, ()> for State {
    fn event(state: &mut Self, _: &wl_data_device::WlDataDevice, ev: wl_data_device::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match ev {
            wl_data_device::Event::DataOffer { id } => state.offers.push((id, vec![])),
            wl_data_device::Event::Selection { id } => {
                if let Some((old, _)) = state.selection.take() {
                    old.destroy();
                }
                let mime = id.as_ref().and_then(|o| state.offers.iter().position(|(p, _)| p == o)).map(|i| state.offers.swap_remove(i).1);
                // any other announced offer is stale now
                for (o, _) in state.offers.drain(..) {
                    o.destroy();
                }
                state.selection = id.map(|o| (o, mime.unwrap_or_default()));
            }
            wl_data_device::Event::Enter { id, .. } => {
                if let Some(o) = &id {
                    state.offers.retain(|(p, _)| p != o);
                }
                if let Some(old) = std::mem::replace(&mut state.dnd, id) {
                    old.destroy();
                }
            }
            wl_data_device::Event::Leave | wl_data_device::Event::Drop => {
                if let Some(o) = state.dnd.take() {
                    o.destroy();
                }
            }
            _ => {}
        }
    }

    wayland_client::event_created_child!(State, wl_data_device::WlDataDevice, [
        wl_data_device::EVT_DATA_OFFER_OPCODE => (wl_data_offer::WlDataOffer, ()),
    ]);
}

impl Dispatch<wl_data_offer::WlDataOffer, ()> for State {
    fn event(state: &mut Self, o: &wl_data_offer::WlDataOffer, ev: wl_data_offer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_data_offer::Event::Offer { mime_type } = ev {
            if let Some((_, m)) = state.offers.iter_mut().find(|(p, _)| p == o) {
                m.push(mime_type);
            } else if let Some((sel, m)) = &mut state.selection
                && sel == o
            {
                m.push(mime_type);
            }
        }
    }
}
