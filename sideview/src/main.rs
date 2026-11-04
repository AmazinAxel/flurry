// flurry-sideview: Servo web pages in a right-edge layer surface, driven by the flurry process over
// stdin (widgets/sideview.rs). Commands, one per line:
//   show <page> <url> | hide | focus <true|false> <ignore|exclusive> | width <px> | dark <true|false>
// and on stdout: "mapped" once a show's first frame is on screen, "closed" when the compositor
// closed the panel (output gone), "open <url>" for a link to open in the default browser (flurry
// launches it: this process is sandboxed, see package.nix). Its one argument is the panel
// background before a page paints, as rrggbb (flurry's style).

mod claude;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::PathBuf;
use std::ptr::NonNull;
use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::prelude::{BASE64_STANDARD, Engine as _};
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

/// What the delegates share with the loop.
#[derive(Default)]
struct Shared {
    rendering: RefCell<Option<Rc<dyn RenderingContext>>>,
    /// popups waiting for their first navigation, dropped after a few seconds without one
    popups: RefCell<Vec<(WebView, Instant)>>,
    last_open: Cell<Option<Instant>>,
    /// the last click or key press given to the page on screen
    input_at: Cell<Option<(WebViewId, Instant)>>,
    /// the last Ctrl+V given to a page
    paste_at: Cell<Option<(WebViewId, Instant)>>,
    /// the clipboard read in progress (the last paste's)
    receive: RefCell<Option<Receive>>,
}

impl Shared {
    /// A click or key press given to page `id` on screen within the last second.
    fn recent_input(&self, id: WebViewId, now: Instant) -> bool {
        self.input_at.get().is_some_and(|(i, at)| i == id && now < at + Duration::from_secs(1))
    }

    /// Web links only: pages must not be able to hand local files or other schemes to their
    /// handlers. Only from the page on screen, right after a click or key there (a hidden page
    /// can't open anything), and at most one a second, so a page can't flood the browser. flurry
    /// opens it (outside this sandbox and its own service), checking the same again.
    fn open_external(&self, url: &str, from: WebViewId) {
        let now = Instant::now();
        if (url.starts_with("https://") || url.starts_with("http://"))
            && !url.contains(|c: char| c.is_whitespace() || c.is_control())
            && self.recent_input(from, now)
            && self.last_open.get().is_none_or(|at| now >= at + Duration::from_secs(1))
        {
            self.last_open.set(Some(now));
            tell(&format!("open {url}"));
        }
    }
}

/// Drops the shared web views, rendering context and clipboard read while the Wayland connection
/// (and Servo) are still alive: the delegates holding `Shared` would only let go after both.
struct ClearShared(Rc<Shared>);
impl Drop for ClearShared {
    fn drop(&mut self) {
        self.0.popups.borrow_mut().clear();
        self.0.rendering.borrow_mut().take();
        self.0.receive.borrow_mut().take();
    }
}

#[derive(Default)]
struct Frame {
    shared: Rc<Shared>,
    dirty: Cell<bool>,
    /// console messages addressed to us (\u{1} prefix), with their page: the claude page's channel out
    messages: RefCell<Vec<(WebView, String)>>,
    /// the only page allowed to use that channel: any site can write to its console
    claude_url: Option<url::Url>,
    /// the web views made for it: the only ones that may read local files, and only its own
    claude_views: RefCell<Vec<WebViewId>>,
}

/// The page is read from FLURRY_CLAUDE_PAGE (set by the package wrapper; kept out of the binary so
/// page edits don't rebuild Servo); debug builds fall back to the source tree. A release build's
/// source path means nothing at runtime (and anyone could create it), so there is no page then.
fn page_url() -> Option<url::Url> {
    let page = std::env::var_os("FLURRY_CLAUDE_PAGE").map(PathBuf::from);
    let page = page.or_else(|| cfg!(debug_assertions).then(|| concat!(env!("CARGO_MANIFEST_DIR"), "/claude").into()))?;
    url::Url::from_file_path(page.join("index.html")).ok()
}

/// Local files are for the claude page's own directory; every other load of one fails. Compared as
/// resolved paths: a URL prefix would let `..%2f` out.
fn filter_files(load: WebResourceLoad, page: Option<&url::Url>) {
    let url = &load.request().url;
    if url.scheme() != "file" {
        return;
    }
    let dir = page.and_then(|p| p.to_file_path().ok()).and_then(|p| p.parent()?.canonicalize().ok());
    let own = dir.is_some_and(|dir| url.to_file_path().ok().and_then(|p| p.canonicalize().ok()).is_some_and(|p| p.starts_with(&dir)));
    if !own {
        let url = url.clone();
        load.intercept(WebResourceResponse::new(url)).cancel();
    }
}

/// A line to flurry on stdout.
fn tell(msg: &str) {
    let mut out = std::io::stdout();
    let _ = out.write_all(format!("{msg}\n").as_bytes()).and_then(|_| out.flush());
}

/// The selection as this client sees it (wl_data_device): shared by the input handling and the
/// clipboard delegate.
type Selection = Rc<RefCell<Option<(wl_data_offer::WlDataOffer, Vec<String>)>>>;

/// Pages write (or clear) the clipboard only right after input given to them, and read it only to
/// paste: within a second of a Ctrl+V given to that page.
struct Clipboard {
    arboard: std::cell::OnceCell<Option<RefCell<arboard::Clipboard>>>,
    conn: Connection,
    selection: Selection,
    shared: Rc<Shared>,
}
impl Clipboard {
    fn with<T>(&self, f: impl FnOnce(&mut arboard::Clipboard) -> Result<T, arboard::Error>) -> Option<T> {
        let c = self.arboard.get_or_init(|| arboard::Clipboard::new().ok().map(RefCell::new)).as_ref()?;
        f(&mut c.borrow_mut()).ok()
    }
}
impl ClipboardDelegate for Clipboard {
    fn clear(&self, webview: WebView) {
        if self.shared.recent_input(webview.id(), Instant::now()) {
            self.with(|c| c.clear());
        }
    }
    fn get_text(&self, webview: WebView, request: StringRequest) {
        let pasting = self.shared.paste_at.get().is_some_and(|(id, at)| id == webview.id() && at.elapsed() < Duration::from_secs(1));
        // read like a media paste (by the loop, as the source writes), not through arboard (blocks)
        let sel = self.selection.borrow();
        let text = sel.as_ref().filter(|_| pasting).and_then(|(offer, mimes)| {
            let mime = ["text/plain;charset=utf-8", "UTF8_STRING", "text/plain"].into_iter().find(|t| mimes.iter().any(|m| m == t))?;
            Some((offer, mime))
        });
        match text {
            Some((offer, mime)) => {
                let done = move |bytes: Vec<u8>| match String::from_utf8(bytes) {
                    Ok(text) => request.success(text),
                    Err(_) => request.failure(String::new()),
                };
                // a read still going is dropped (a dropped request fails)
                *self.shared.receive.borrow_mut() = Receive::start(&self.conn, offer, mime, done);
            }
            None => request.failure(String::new()),
        }
    }
    fn set_text(&self, webview: WebView, text: String) {
        if self.shared.recent_input(webview.id(), Instant::now()) {
            self.with(|c| c.set_text(text));
        }
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
        let shared = &self.shared;
        if shared.popups.borrow().len() >= 2 {
            return;
        }
        let rendering = shared.rendering.borrow().clone();
        if let Some(rc) = rendering {
            let aux = request.builder(rc).delegate(Rc::new(Popup { from: parent.id(), shared: shared.clone() })).build();
            aux.hide(); // never in the scene
            shared.popups.borrow_mut().push((aux, Instant::now()));
        }
    }
}

/// A page's new window, by the page that opened it.
struct Popup {
    from: WebViewId,
    shared: Rc<Shared>,
}
impl WebViewDelegate for Popup {
    fn load_web_resource(&self, _: WebView, load: WebResourceLoad) {
        filter_files(load, None);
    }
    fn request_navigation(&self, webview: WebView, nav: NavigationRequest) {
        let url = nav.url.to_string();
        nav.deny();
        if url != "about:blank" {
            self.shared.open_external(&url, self.from);
            self.shared.popups.borrow_mut().retain(|(wv, _)| *wv != webview);
        }
    }
}

/// A clipboard transfer in progress: the source writes at its own pace and the loop reads
/// whenever the pipe is readable, so a slow or endless source never holds up the panel; what
/// goes over the cap is dropped. A newer paste replaces (drops) it.
struct Receive {
    file: std::fs::File,
    data: Vec<u8>,
    done: Box<dyn FnOnce(Vec<u8>)>,
}

impl Receive {
    /// Asks the selection's source for `mime`; `done` gets the data, empty if the transfer failed.
    fn start(conn: &Connection, offer: &wl_data_offer::WlDataOffer, mime: &str, done: impl FnOnce(Vec<u8>) + 'static) -> Option<Self> {
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } < 0 {
            return None;
        }
        let (rd, wr) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        offer.receive(mime.into(), wr.as_fd());
        drop(wr);
        let _ = conn.flush();
        unsafe { libc::fcntl(rd.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) };
        Some(Self { file: std::fs::File::from(rd), data: vec![], done: Box::new(done) })
    }

    /// Takes what the source has written so far; None once it is over (the end, an error or the
    /// cap) and the data handed on.
    fn pump(mut self) -> Option<Self> {
        const CAP: usize = 20 << 20;
        let mut chunk = [0u8; 65536];
        loop {
            match self.file.read(&mut chunk) {
                Ok(0) => break,
                Ok(n) if self.data.len() + n <= CAP => self.data.extend_from_slice(&chunk[..n]),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return Some(self),
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                _ => {
                    self.data.clear();
                    break;
                }
            }
        }
        (self.done)(self.data);
        None
    }
}

struct State {
    size: (u32, u32),
    configured: bool,
    scale: i32,
    ctx: xkb::Context,
    xkb: Option<xkb::State>,
    /// dead keys and Compose sequences (the locale's table)
    compose: Option<xkb::compose::State>,
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
    selection: Selection,
    /// drags are not taken; their offer is only held until they leave
    dnd: Option<wl_data_offer::WlDataOffer>,
    /// the compositor closed the layer surface (its output went away)
    layer_closed: bool,
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
        K::ISO_Level3_Shift => NamedKey::AltGraph,
        K::Insert => NamedKey::Insert,
        K::Caps_Lock => NamedKey::CapsLock,
        K::Num_Lock => NamedKey::NumLock,
        K::Scroll_Lock => NamedKey::ScrollLock,
        K::Print => NamedKey::PrintScreen,
        K::Pause => NamedKey::Pause,
        K::Menu => NamedKey::ContextMenu,
        K::F1 => NamedKey::F1,
        K::F2 => NamedKey::F2,
        K::F3 => NamedKey::F3,
        K::F4 => NamedKey::F4,
        K::F5 => NamedKey::F5,
        K::F6 => NamedKey::F6,
        K::F7 => NamedKey::F7,
        K::F8 => NamedKey::F8,
        K::F9 => NamedKey::F9,
        K::F10 => NamedKey::F10,
        K::F11 => NamedKey::F11,
        K::F12 => NamedKey::F12,
        _ => return None,
    })
}

/// The physical key (KeyboardEvent.code) and its location, from the evdev keycode.
fn code(evdev: u32) -> (Code, Location) {
    use Code as C;
    use Location::{Left as L, Numpad as N, Right as R, Standard as S};
    const ROW_Q: [Code; 10] = [C::KeyQ, C::KeyW, C::KeyE, C::KeyR, C::KeyT, C::KeyY, C::KeyU, C::KeyI, C::KeyO, C::KeyP];
    const ROW_A: [Code; 9] = [C::KeyA, C::KeyS, C::KeyD, C::KeyF, C::KeyG, C::KeyH, C::KeyJ, C::KeyK, C::KeyL];
    const ROW_Z: [Code; 7] = [C::KeyZ, C::KeyX, C::KeyC, C::KeyV, C::KeyB, C::KeyN, C::KeyM];
    const DIGITS: [Code; 10] = [C::Digit1, C::Digit2, C::Digit3, C::Digit4, C::Digit5, C::Digit6, C::Digit7, C::Digit8, C::Digit9, C::Digit0];
    const FKEYS: [Code; 10] = [C::F1, C::F2, C::F3, C::F4, C::F5, C::F6, C::F7, C::F8, C::F9, C::F10];
    match evdev {
        1 => (C::Escape, S),
        2..=11 => (DIGITS[evdev as usize - 2], S),
        12 => (C::Minus, S),
        13 => (C::Equal, S),
        14 => (C::Backspace, S),
        15 => (C::Tab, S),
        16..=25 => (ROW_Q[evdev as usize - 16], S),
        26 => (C::BracketLeft, S),
        27 => (C::BracketRight, S),
        28 => (C::Enter, S),
        29 => (C::ControlLeft, L),
        30..=38 => (ROW_A[evdev as usize - 30], S),
        39 => (C::Semicolon, S),
        40 => (C::Quote, S),
        41 => (C::Backquote, S),
        42 => (C::ShiftLeft, L),
        43 => (C::Backslash, S),
        44..=50 => (ROW_Z[evdev as usize - 44], S),
        51 => (C::Comma, S),
        52 => (C::Period, S),
        53 => (C::Slash, S),
        54 => (C::ShiftRight, R),
        55 => (C::NumpadMultiply, N),
        56 => (C::AltLeft, L),
        57 => (C::Space, S),
        58 => (C::CapsLock, S),
        59..=68 => (FKEYS[evdev as usize - 59], S),
        69 => (C::NumLock, N),
        70 => (C::ScrollLock, S),
        71 => (C::Numpad7, N),
        72 => (C::Numpad8, N),
        73 => (C::Numpad9, N),
        74 => (C::NumpadSubtract, N),
        75 => (C::Numpad4, N),
        76 => (C::Numpad5, N),
        77 => (C::Numpad6, N),
        78 => (C::NumpadAdd, N),
        79 => (C::Numpad1, N),
        80 => (C::Numpad2, N),
        81 => (C::Numpad3, N),
        82 => (C::Numpad0, N),
        83 => (C::NumpadDecimal, N),
        86 => (C::IntlBackslash, S),
        87 => (C::F11, S),
        88 => (C::F12, S),
        96 => (C::NumpadEnter, N),
        97 => (C::ControlRight, R),
        98 => (C::NumpadDivide, N),
        99 => (C::PrintScreen, S),
        100 => (C::AltRight, R),
        102 => (C::Home, S),
        103 => (C::ArrowUp, S),
        104 => (C::PageUp, S),
        105 => (C::ArrowLeft, S),
        106 => (C::ArrowRight, S),
        107 => (C::End, S),
        108 => (C::ArrowDown, S),
        109 => (C::PageDown, S),
        110 => (C::Insert, S),
        111 => (C::Delete, S),
        119 => (C::Pause, S),
        125 => (C::MetaLeft, L),
        126 => (C::MetaRight, R),
        127 => (C::ContextMenu, S),
        _ => (C::Unidentified, S),
    }
}

/// The locale's Compose table (LC_ALL, LC_CTYPE, LANG, as setlocale picks it).
fn compose_state(ctx: &xkb::Context) -> Option<xkb::compose::State> {
    let locale = ["LC_ALL", "LC_CTYPE", "LANG"].iter().filter_map(std::env::var_os).find(|v| !v.is_empty()).unwrap_or_else(|| "C".into());
    let table = xkb::compose::Table::new_from_locale(ctx, &locale, xkb::compose::COMPILE_NO_FLAGS).ok()?;
    Some(xkb::compose::State::new(&table, xkb::compose::STATE_NO_FLAGS))
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
        size: (0, 0),
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
        selection: Selection::default(),
        compose: None,
        dnd: None,
        layer_closed: false,
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
        .map(PathBuf::from)
        .filter(|d| d.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/share")))
        .map(|d| d.join("flurry-sideview"));
    // cookies, auth cache and local storage: Servo creates them 0644, so the directory is private
    if let Some(d) = &data {
        let _ = std::fs::DirBuilder::new().recursive(true).mode(0o700).create(d);
        let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o700));
    }
    let opts = Opts { config_dir: data, ..Default::default() };
    let mut prefs = Preferences::default();
    prefs.dom_intersection_observer_enabled = true;
    prefs.dom_async_clipboard_enabled = true;
    prefs.dom_fontface_enabled = true;
    prefs.layout_variable_fonts_enabled = true;
    prefs.user_agent = SAFARI_UA.into();
    prefs.editing_caret_blink_time = 0; // steady caret
    if let Some(bg) = std::env::args().nth(1).and_then(|a| u32::from_str_radix(&a, 16).ok()) {
        let c = |shift: u32| ((bg >> shift) & 0xff) as f64 / 255.0;
        prefs.shell_background_color_rgba = [c(16), c(8), c(0), 1.0];
    }
    let servo = ServoBuilder::default().opts(opts).preferences(prefs).event_loop_waker(Box::new(Waker(wake_fd))).build();
    let scripts = Rc::new(UserContentManager::new(&servo));
    scripts.add_script(Rc::new(UserScript::new(PASTE_SHIM_JS.into(), None)));
    // dropped before Servo (and the connection, declared before it)
    let shared = Rc::new(Shared::default());
    let _clear = ClearShared(shared.clone());

    // flurry sends its width, scheme and focus before the first show
    let mut width: u32 = 0;
    let mut focused = false;
    let mut exclusive = false;
    let mut dark = true;
    let mut pages: HashMap<String, WebView> = HashMap::new();
    let mut current: Option<String> = None;
    let mut visible = false;
    // configured since the last map: presenting before that blocks in eglSwapBuffers on a frame
    // callback the unmapped surface never gets (re-shows hung until the helper was killed)
    let mut mapped = false;
    // a page just shown waits for the first frame its show produces (painting before it renders
    // the previous scene); one still loading, for its load to complete (its frames until then
    // are blank, so the old one stays up)
    let mut awaiting = false;
    let mut loading = false;
    let mut rendering: Option<Rc<WindowRenderingContext>> = None;
    // one Claude Code process for every page showing flurry:claude: replies go to the page that
    // sent the last message (two such pages share the conversation)
    let mut chat = claude::Chat::default();
    let mut chat_view: Option<WebView> = None;
    let frame = Rc::new(Frame { shared: shared.clone(), claude_url: page_url(), ..Default::default() });
    let clipboard = Rc::new(Clipboard { arboard: Default::default(), conn: conn.clone(), selection: state.selection.clone(), shared: shared.clone() });
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

    let map = |s: &mut Surfaces, width: u32, focused: bool, exclusive: bool| {
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
        l.set_exclusive_zone(if exclusive { width as i32 } else { -1 });
        s.border.commit();
    };
    // the border's two colors, without and with the keyboard (#sideview:focus-within)
    let border_buffers = [DARK4, BLUE3].map(|c| {
        let [r, g, b, a] = rgba(c);
        pixels.create_u32_rgba_buffer(r, g, b, a, &qh, ())
    });
    let paint_border = |s: &Surfaces, focused: bool| {
        s.border.attach(Some(&border_buffers[focused as usize]), 0, 0);
        s.border.damage(0, 0, i32::MAX, i32::MAX);
        s.border.commit();
    };

    loop {
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
        let end = input.iter().rposition(|&b| b == b'\n').map_or(0, |i| i + 1);
        lines.extend(input[..end].split(|&b| b == b'\n').filter(|l| !l.is_empty()).map(|l| String::from_utf8_lossy(l).trim().to_string()));
        input.drain(..end);
        // the output went away: hidden like a hide, and flurry is told (the pages stay)
        if std::mem::take(&mut state.layer_closed) {
            lines.push("hide".into());
            tell("closed");
        }
        for line in lines {
            let mut w = line.splitn(3, ' ');
            match (w.next(), w.next(), w.next()) {
                (Some("show"), Some(name), Some(url)) => {
                    let is_claude = url == claude::URL;
                    let target = if is_claude { frame.claude_url.clone() } else { url::Url::parse(url).ok() };
                    if !pages.contains_key(name) && target.is_none() {
                        continue; // nothing to show
                    }
                    // the page on screen shown again: its show changes nothing, so no frame comes
                    let again = visible && current.as_deref() == Some(name);
                    if !visible {
                        visible = true;
                        map(&mut s, width, focused, exclusive);
                    }
                    announce = true;
                    shared.input_at.set(None); // input given to the page shown before doesn't count
                    let Some(rc) = &rendering else {
                        // web views are created once the first configure gives the surface a size
                        pending_show = Some(line.clone());
                        continue;
                    };
                    current = Some(name.to_string());
                    if !pages.contains_key(name)
                        && let Some(u) = target
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
                    // show before hide: each show or hide builds a frame of its own, and a swap's
                    // in-between one then still holds the shown page (hiding first made it one of
                    // neither page, blank)
                    let shown = pages.get(name);
                    if let Some(wv) = shown {
                        wv.show();
                    }
                    for (n, wv) in &pages {
                        if n != name {
                            wv.hide();
                            wv.set_focused(false);
                        }
                    }
                    if let Some(wv) = shown {
                        wv.set_focused(true);
                    }
                    // paint on the frame the show produces, not the stale scene still built
                    frame.dirty.set(again);
                    (awaiting, loading) = (!again, false);
                }
                (Some("hide"), _, _) => {
                    // a show still waiting for the first configure is cancelled, not replayed after
                    pending_show = None;
                    state.release_keys();
                    shared.input_at.set(None);
                    (bordered, announce) = (false, false);
                    // hidden web views drop out of the scene and are throttled (no animations or
                    // rAF, timers at most once a second); losing focus fires blur, a cue for pages
                    // to pause themselves
                    for wv in pages.values() {
                        wv.hide();
                        wv.set_focused(false);
                    }
                    awaiting = false;
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
                    exclusive = z == "exclusive";
                    if visible {
                        map(&mut s, width, focused, exclusive);
                    }
                }
                (Some("width"), Some(px), _) => {
                    if let Ok(px) = px.parse() {
                        width = px;
                        if visible {
                            map(&mut s, width, focused, exclusive);
                        }
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
                        *shared.rendering.borrow_mut() = Some(rc.clone());
                        rendering = Some(rc);
                    }
                }
            }
            mapped = true;
            if bordered {
                paint_border(&s, state.has_keyboard); // commits the new size
            }
            if !awaiting {
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
            // shortcuts by the first layout's base symbol too: Ctrl+R on a non-Latin layout
            let base = x.get_keymap().key_get_syms_by_level(code, 0, 0).first().copied();
            let is = |k: xkb::Keysym| sym == k || base == Some(k);
            let wv = current.as_ref().and_then(|c| pages.get(c));
            if pressed && ctrl && is(xkb::Keysym::r) {
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
                && is(xkb::Keysym::v)
                && let Some(wv) = wv
            {
                shared.paste_at.set(Some((wv.id(), now)));
            }
            if pressed
                && !repeat
                && ctrl
                && is(xkb::Keysym::v)
                && let Some(wv) = wv
            {
                // media only when there's no text to paste: copied rich text also offers an image
                let sel = state.selection.borrow();
                let media = sel.as_ref().filter(|(_, mimes)| !mimes.iter().any(|m| m.starts_with("text/plain"))).and_then(|(offer, mimes)| {
                    let mime = mimes.iter().find(|m| m.starts_with("image/") || m.starts_with("video/"))?;
                    Some((offer, mime))
                });
                if let Some((offer, mime)) = media {
                    // pasted once it has all come in (to the page it was given to)
                    let (wv, m) = (wv.clone(), mime.clone());
                    let paste = move |bytes: Vec<u8>| {
                        if !bytes.is_empty() {
                            wv.evaluate_javascript(format!("window.__flurryPaste({:?}, {:?})", BASE64_STANDARD.encode(&bytes), m), |_| {});
                        }
                    };
                    *shared.receive.borrow_mut() = Receive::start(&conn, offer, mime, paste);
                    continue;
                }
            }
            let mut text: String = sym.key_char().filter(|c| !c.is_control()).map(String::from).unwrap_or_default();
            if pressed
                && !repeat
                && let Some(c) = &mut state.compose
                && matches!(c.feed(sym), xkb::compose::FeedResult::Accepted)
            {
                match c.status() {
                    xkb::compose::Status::Composing => continue,
                    xkb::compose::Status::Composed => {
                        text = c.utf8().unwrap_or_default();
                        c.reset();
                    }
                    xkb::compose::Status::Cancelled => {
                        c.reset();
                        continue;
                    }
                    xkb::compose::Status::Nothing => {}
                }
            }
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
            let (code, location) = self::code(code.raw().saturating_sub(8));
            state.events.push(InputEvent::Keyboard(KeyboardEvent::new_without_event(ks, key, code, location, modifiers, repeat, false)));
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
                    wv.evaluate_javascript(r#"window.__claude(['{"type":"exit","error":"claude not found"}'])"#, |_| {});
                }
                chat_view = Some(wv);
            } else if let Some(url) = m.strip_prefix("open:") {
                shared.open_external(url, wv.id());
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
        shared.popups.borrow_mut().retain(|(_, at)| at.elapsed() < Duration::from_secs(5));
        if let Some(wv) = current.as_ref().and_then(|c| pages.get(c)) {
            for ev in state.events.drain(..) {
                let real = match &ev {
                    InputEvent::MouseButton(e) => e.action == MouseButtonAction::Down,
                    InputEvent::Keyboard(e) => e.event.state == KeyState::Down && !e.event.repeat,
                    _ => false,
                };
                if real && visible {
                    shared.input_at.set(Some((wv.id(), Instant::now())));
                }
                wv.notify_input_event(ev);
            }
            if awaiting {
                if wv.load_status() == LoadStatus::Started {
                    frame.dirty.set(false); // nothing parsed yet: its frames are blank, the old one stays up
                    loading = true;
                } else if std::mem::take(&mut loading) {
                    frame.dirty.set(true); // its head parsed (a Servo event; a stalled subresource can't hold it back)
                }
            }
            if visible && mapped
                && frame.dirty.replace(false)
                && let Some(rc) = &rendering
            {
                if !bordered {
                    // maps the panel, with this frame: never the stale one before it; sent ahead of
                    // the swap, which may wait on the frame callback of one shown before the hide
                    bordered = true;
                    paint_border(&s, state.has_keyboard);
                    let _ = conn.flush();
                }
                wv.paint();
                rc.present();
                awaiting = false;
                if std::mem::take(&mut announce) {
                    tell("mapped");
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
            // a paste coming in
            libc::pollfd { fd: shared.receive.borrow().as_ref().map_or(-1, |r| r.file.as_raw_fd()), events: libc::POLLIN, revents: 0 },
        ];
        let timeout = state.repeat_key.map(|r| r.1.saturating_duration_since(Instant::now()).as_millis() as i32).unwrap_or(-1);
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
        if fds[5].revents != 0
            && let Some(r) = shared.receive.take().and_then(Receive::pump)
        {
            *shared.receive.borrow_mut() = Some(r);
        }
        let had_keyboard = state.has_keyboard;
        if queue.dispatch_pending(&mut state).is_err() {
            return;
        }
        if state.has_keyboard != had_keyboard && visible && bordered {
            // #sideview:focus-within
            paint_border(&s, state.has_keyboard);
        }
    }
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
            zwlr_layer_surface_v1::Event::Closed => state.layer_closed = true,
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
                state.compose = compose_state(&state.ctx);
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
                if let Some((old, _)) = state.selection.borrow_mut().take() {
                    old.destroy();
                }
                let mime = id.as_ref().and_then(|o| state.offers.iter().position(|(p, _)| p == o)).map(|i| state.offers.swap_remove(i).1);
                // any other announced offer is stale now
                for (o, _) in state.offers.drain(..) {
                    o.destroy();
                }
                *state.selection.borrow_mut() = id.map(|o| (o, mime.unwrap_or_default()));
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
            } else if let Some((sel, m)) = &mut *state.selection.borrow_mut()
                && sel == o
            {
                m.push(mime_type);
            }
        }
    }
}
