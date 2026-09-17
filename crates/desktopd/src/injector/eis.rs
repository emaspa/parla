//! Primary injector: KWin's EIS D-Bus interface over libei (reis crate).
//!
//! Plasma 6 removed fake-input; the RemoteDesktop portal shows a consent
//! dialog; KWin's private EIS interface (`org.kde.KWin` →
//! `/org/kde/KWin/EIS/RemoteDesktop` → `connectToEIS(caps) -> (fd, cookie)`)
//! is the clean path — no dialogs. Verified live on Plasma 6.7.5.
//!
//! Two device paths, preferred in order:
//! - `ei_text` (KWin built with libei ≥ 1.6 support): `utf8()` + `keysym()`
//!   requests, fully layout-independent. Not present on KWin 6.7.5.
//! - `ei_keyboard` (always present): KWin sends the active xkb keymap as an
//!   fd; we reverse-map chars/keysyms to keycodes through it, so output is
//!   correct on whatever layout the user actually runs.
//!
//! Known KWin quirks (from kwin-mcp's issue tracker, plan §5):
//! - KWin PAUSEs EIS devices right after the handshake; wait for RESUMED
//!   before `start_emulating` (bounded: 3 attempts, 4 s budget per attempt).
//! - The EIS context dies when our bus name disappears → the blocking zbus
//!   connection used for `connectToEIS` is held for the injector's lifetime.
//! - Delivery is unconfirmed across DISCONNECT → fail loudly and rebuild.
//!
//! Architecture: a dedicated worker thread owns the zbus blocking connection,
//! the ei context, and the (Send-hostile) xkb keymap. Callers talk to it over
//! a command channel, which keeps it independent of whatever async runtime
//! the daemon uses.

use std::collections::HashMap;
use std::io::ErrorKind;
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use reis::ei;
use reis::PendingRequestResult;
use xkbcommon::xkb;

use super::{normalize_chord, TextInjector};

/// KWin's private EIS entry point (Plasma >= 6.2).
const EIS_SERVICE: &str = "org.kde.KWin";
const EIS_PATH: &str = "/org/kde/KWin/EIS/RemoteDesktop";
const EIS_IFACE: &str = "org.kde.KWin.EIS.RemoteDesktop";

const CAPS_KEYBOARD: i32 = 1;
const RECONNECT_ATTEMPTS: u32 = 3;
const SETUP_BUDGET: Duration = Duration::from_secs(4);
const CMD_TIMEOUT: Duration = Duration::from_secs(30);
/// How long the worker waits for a command before servicing the socket
/// (pings, pauses, keymap changes arrive while nobody is typing).
const IDLE_POLL: Duration = Duration::from_millis(200);
/// How long a burst reads after its flush, to catch a DISCONNECT the
/// server sends in response.
const POST_FLUSH_READ: Duration = Duration::from_millis(20);
const NAME: &str = "parla";
/// evdev keycodes are xkb keycodes minus 8
const EVDEV_OFFSET: u32 = 8;

fn interfaces() -> HashMap<&'static str, u32> {
    HashMap::from([
        ("ei_callback", 1),
        ("ei_connection", 1),
        ("ei_seat", 1),
        ("ei_device", 1),
        ("ei_pingpong", 1),
        ("ei_keyboard", 1),
        ("ei_text", 1),
    ])
}

type Reply = mpsc::Sender<Result<(), String>>;

enum Cmd {
    Type(String, Reply),
    Chord(String, Reply),
    Probe(Reply),
}

/// Handle to the EIS worker thread.
pub struct EisInjector {
    cmd_tx: Option<mpsc::Sender<Cmd>>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl EisInjector {
    /// Spawn the worker and wait for the initial EIS session to come up.
    /// Fails if KWin's EIS interface is unavailable or setup times out —
    /// callers then fall back to the next injector.
    pub fn new() -> anyhow::Result<Self> {
        let (cmd_tx, cmd_rx) = mpsc::channel::<Cmd>();
        let (init_tx, init_rx) = mpsc::channel::<Result<(), String>>();
        let worker = std::thread::Builder::new()
            .name("parla-eis".into())
            .spawn(move || worker_main(cmd_rx, init_tx))?;
        init_rx
            .recv_timeout(CMD_TIMEOUT)
            .map_err(|e| anyhow::anyhow!("EIS worker did not report init: {e}"))?
            .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(Self {
            cmd_tx: Some(cmd_tx),
            worker: Some(worker),
        })
    }

    fn request(&self, cmd: Cmd) -> anyhow::Result<()> {
        let (tx, rx) = mpsc::channel();
        let cmd_tx = self
            .cmd_tx
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("EIS worker already shut down"))?;
        match cmd {
            Cmd::Type(text, _) => cmd_tx.send(Cmd::Type(text, tx))?,
            Cmd::Chord(chord, _) => cmd_tx.send(Cmd::Chord(chord, tx))?,
            Cmd::Probe(_) => cmd_tx.send(Cmd::Probe(tx))?,
        }
        rx.recv_timeout(CMD_TIMEOUT)
            .map_err(|e| anyhow::anyhow!("EIS worker unresponsive: {e}"))?
            .map_err(|e| anyhow::anyhow!("EIS: {e}"))
    }
}

impl TextInjector for EisInjector {
    fn name(&self) -> &'static str {
        "eis"
    }

    fn type_text(&self, text: &str) -> anyhow::Result<()> {
        anyhow::ensure!(!text.is_empty(), "nothing to type");
        self.request(Cmd::Type(text.to_string(), unused_reply()))
    }

    fn key_chord(&self, chord: &str) -> anyhow::Result<()> {
        let keys = normalize_chord(chord);
        anyhow::ensure!(!keys.is_empty(), "empty key chord");
        self.request(Cmd::Chord(chord.to_string(), unused_reply()))
    }

    fn probe(&self) -> anyhow::Result<()> {
        self.request(Cmd::Probe(unused_reply()))
    }
}

fn unused_reply() -> Reply {
    mpsc::channel().0
}

impl Drop for EisInjector {
    fn drop(&mut self) {
        // Close the command channel FIRST (Drop::drop runs before field
        // drops — joining while cmd_tx is still alive would deadlock the
        // worker in recv()). The worker then disconnects the EIS context
        // and drops the zbus connection on its own thread.
        self.cmd_tx.take();
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

// ---------------------------------------------------------------------------
// Worker thread: owns everything EIS/xkb (no Send requirements inside).
// ---------------------------------------------------------------------------

struct Worker {
    conn: zbus::blocking::Connection,
    inner: Option<Inner>,
}

/// Compiled keymap plus reverse-lookup caches.
struct KeymapData {
    keymap: xkb::Keymap,
    /// xkb keycodes of modifier keys, found by scanning the keymap.
    shift: Option<u32>,
    altgr: Option<u32>,
    /// Active layout group, from ei_keyboard.modifiers. Lookups resolve
    /// against it, so a user on the second layout of two gets that one.
    group: u32,
    char_cache: HashMap<char, CharKeys>,
}

/// How to produce one character: an xkb keycode plus required modifiers.
#[derive(Clone, Copy)]
struct CharKeys {
    keycode: u32,
    shift: bool,
    altgr: bool,
}

impl KeymapData {
    fn new(keymap: xkb::Keymap) -> Self {
        let shift = Self::find_modifier(&keymap, xkb::Keysym::Shift_L);
        let altgr = Self::find_modifier(&keymap, xkb::Keysym::ISO_Level3_Shift);
        Self {
            keymap,
            shift,
            altgr,
            group: 0,
            char_cache: HashMap::new(),
        }
    }

    fn set_group(&mut self, group: u32) {
        let group = if group < self.keymap.num_layouts() { group } else { 0 };
        if group != self.group {
            self.group = group;
            self.char_cache.clear();
        }
    }

    fn find_modifier(keymap: &xkb::Keymap, sym: xkb::Keysym) -> Option<u32> {
        (keymap.min_keycode().raw()..=keymap.max_keycode().raw()).find(|&kc| {
            keymap
                .key_get_syms_by_level(xkb::Keycode::new(kc), 0, 0)
                .contains(&sym)
        })
    }

    /// Locate the keycode and shift level producing a keysym on the active
    /// layout group, falling back to group 0 for keys the group lacks.
    fn locate(&self, sym: xkb::Keysym) -> Option<(u32, u32)> {
        let groups: &[u32] = if self.group == 0 { &[0] } else { &[self.group, 0] };
        for &group in groups {
            for kc in self.keymap.min_keycode().raw()..=self.keymap.max_keycode().raw() {
                let key = xkb::Keycode::new(kc);
                if group >= self.keymap.num_layouts_for_key(key) {
                    continue;
                }
                let levels = self.keymap.num_levels_for_key(key, group).min(4);
                for level in 0..levels {
                    if self
                        .keymap
                        .key_get_syms_by_level(key, group, level)
                        .contains(&sym)
                    {
                        return Some((kc, level));
                    }
                }
            }
        }
        None
    }

    fn char_keys(&mut self, c: char) -> Result<CharKeys, String> {
        if let Some(ck) = self.char_cache.get(&c) {
            return Ok(*ck);
        }
        let sym = match c {
            // from_char maps these to Linefeed/Tab-as-control, which no
            // keymap carries; the keys that produce them do
            '\n' | '\r' => xkb::Keysym::Return,
            '\t' => xkb::Keysym::Tab,
            _ => xkb::Keysym::from_char(c),
        };
        if sym == xkb::Keysym::NoSymbol {
            return Err(format!("no keysym for char {c:?}"));
        }
        let (keycode, level) = self
            .locate(sym)
            .ok_or_else(|| format!("char {c:?} not reachable on the active keymap"))?;
        let ck = CharKeys {
            keycode,
            // level 1 = shift, level 2 = altgr, level 3 = both (standard
            // group layout on virtually all XKB symbols)
            shift: level == 1 || level == 3,
            altgr: level == 2 || level == 3,
        };
        self.char_cache.insert(c, ck);
        Ok(ck)
    }
}

struct Inner {
    context: ei::Context,
    cookie: i32,
    device: Option<ei::Device>,
    keyboard: Option<ei::Keyboard>,
    text: Option<ei::Text>,
    keymap: Option<KeymapData>,
    last_serial: u32,
    sequence: u32,
    // setup/health state
    /// OR of every keyboard/text capability mask the seat advertised;
    /// bound once at seat.done (a later bind replaces the earlier set).
    capability_mask: u64,
    capability_bound: bool,
    device_done: bool,
    resumed: bool,
    dead: bool,
}

impl Inner {
    fn can_type(&self) -> bool {
        self.text.is_some() || (self.keyboard.is_some() && self.keymap.is_some())
    }

    fn is_ready(&self) -> bool {
        self.can_type() && self.device_done && self.resumed && !self.dead
    }

    /// Pump socket events. Blocks only while data keeps arriving; returns
    /// once drained or the deadline passes.
    fn pump(&mut self, deadline: Instant) -> Result<(), String> {
        loop {
            match self.context.read() {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    if Instant::now() >= deadline {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(e) => {
                    self.dead = true;
                    return Err(e.to_string());
                }
            }
            while let Some(result) = self.context.pending_event() {
                let request = match result {
                    PendingRequestResult::Request(r) => r,
                    PendingRequestResult::ParseError(e) => {
                        tracing::warn!("EIS parse error: {e:?}");
                        self.dead = true;
                        return Err("EIS protocol parse error".into());
                    }
                    PendingRequestResult::InvalidObject(_) => continue,
                };
                self.handle(request)?;
            }
            let _ = self.context.flush();
            if Instant::now() >= deadline {
                return Ok(());
            }
        }
    }

    fn handle(&mut self, request: ei::Event) -> Result<(), String> {
        match request {
            ei::Event::Handshake(handshake, request) => match request {
                ei::handshake::Event::HandshakeVersion { version: _ } => {
                    handshake.handshake_version(1);
                    handshake.name(NAME);
                    handshake.context_type(ei::handshake::ContextType::Sender);
                    for (iface, version) in interfaces() {
                        handshake.interface_version(iface, version);
                    }
                    handshake.finish();
                }
                ei::handshake::Event::Connection { connection: _, serial } => {
                    self.last_serial = serial;
                }
                _ => {}
            },
            ei::Event::Connection(_connection, request) => match request {
                ei::connection::Event::Ping { ping } => {
                    ping.done(0);
                }
                ei::connection::Event::Disconnected { .. } => {
                    tracing::warn!("EIS connection disconnected by server");
                    self.dead = true;
                }
                _ => {}
            },
            ei::Event::Seat(seat, request) => match request {
                ei::seat::Event::Capability { mask, interface } => {
                    tracing::debug!("EIS seat capability: {interface} (mask {mask})");
                    if interface == "ei_keyboard" || interface == "ei_text" {
                        self.capability_mask |= mask;
                    }
                }
                ei::seat::Event::Done => {
                    if self.capability_mask == 0 {
                        return Err(
                            "KWin seat advertised no keyboard/text capability".into()
                        );
                    }
                    if !self.capability_bound {
                        seat.bind(self.capability_mask);
                        self.capability_bound = true;
                    }
                }
                ei::seat::Event::Device { device } => {
                    self.device = Some(device);
                }
                _ => {}
            },
            ei::Event::Device(_device, request) => match request {
                ei::device::Event::Interface { object } => {
                    tracing::debug!("EIS device interface: {}", object.interface());
                    match object.interface() {
                        "ei_keyboard" => self.keyboard = object.downcast::<ei::Keyboard>(),
                        "ei_text" => self.text = object.downcast::<ei::Text>(),
                        _ => {}
                    }
                }
                ei::device::Event::Done => {
                    if !self.can_type() {
                        return Err(
                            "EIS device offers neither ei_text nor ei_keyboard+keymap".into()
                        );
                    }
                    self.device_done = true;
                }
                ei::device::Event::Resumed { serial } => {
                    self.last_serial = serial;
                    self.resumed = true;
                }
                ei::device::Event::Paused { serial } => {
                    self.last_serial = serial;
                    self.resumed = false;
                }
                // no Removed event in the protocol: device removal arrives as
                // a connection Disconnected, handled above
                _ => {}
            },
            ei::Event::Keyboard(_kb, request) => match request {
                ei::keyboard::Event::Keymap {
                    keymap_type,
                    size,
                    keymap: fd,
                } => {
                    tracing::debug!("EIS keymap received ({size} bytes, type {keymap_type:?})");
                    let ctx = xkb::Context::new(0);
                    let keymap = unsafe {
                        xkb::Keymap::new_from_fd(
                            &ctx,
                            fd,
                            size as usize,
                            xkb::KEYMAP_FORMAT_TEXT_V1,
                            0,
                        )
                    }
                    .map_err(|e| format!("keymap fd error: {e}"))?
                    .ok_or("xkb failed to compile the KWin keymap")?;
                    let group = self.keymap.as_ref().map_or(0, |k| k.group);
                    let mut data = KeymapData::new(keymap);
                    data.set_group(group);
                    self.keymap = Some(data);
                }
                ei::keyboard::Event::Modifiers { serial, group, .. } => {
                    self.last_serial = serial;
                    if let Some(k) = self.keymap.as_mut() {
                        k.set_group(group);
                    }
                }
                _ => {}
            },
            other => tracing::trace!("unhandled EIS event: {other:?}"),
        }
        Ok(())
    }

    /// Close the current logical event group. One key event per frame:
    /// press and release of the same key inside one frame is a no-op by
    /// spec, and the server may drop or disconnect a client that does it.
    fn frame(&self) {
        if let Some(d) = &self.device {
            d.frame(self.last_serial, now_micros());
        }
    }

    /// One key event in its own frame.
    fn key(&self, kb: &ei::Keyboard, xkb_keycode: u32, state: ei::keyboard::KeyState) {
        kb.key(xkb_keycode - EVDEV_OFFSET, state);
        self.frame();
    }

    fn press_release(&self, kb: &ei::Keyboard, xkb_keycode: u32) {
        self.key(kb, xkb_keycode, ei::keyboard::KeyState::Press);
        self.key(kb, xkb_keycode, ei::keyboard::KeyState::Released);
    }

    fn modifier(&self, kb: &ei::Keyboard, keycode: Option<u32>, what: &str, state: ei::keyboard::KeyState) {
        match keycode {
            Some(kc) => self.key(kb, kc, state),
            None => tracing::warn!("{what} not on keymap; char may come out wrong"),
        }
    }

    /// One keysym event on the ei_text path, in its own frame.
    fn keysym(&self, t: &ei::Text, sym: u32, state: ei::keyboard::KeyState) {
        t.keysym(sym, state);
        self.frame();
    }
}

fn worker_main(cmd_rx: mpsc::Receiver<Cmd>, init_tx: mpsc::Sender<Result<(), String>>) {
    let worker = match Worker::new() {
        Ok(w) => w,
        Err(e) => {
            let _ = init_tx.send(Err(e.to_string()));
            return;
        }
    };
    let mut worker = worker;
    match worker.connect() {
        Ok(()) => {
            tracing::info!("EIS session established");
            let _ = init_tx.send(Ok(()));
        }
        Err(e) => {
            let _ = init_tx.send(Err(e.to_string()));
            return;
        }
    }

    loop {
        let cmd = match cmd_rx.recv_timeout(IDLE_POLL) {
            Ok(cmd) => cmd,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // idle: answer pings, notice pauses/keymap changes/disconnects
                worker.service();
                continue;
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        let (reply, result) = match cmd {
            Cmd::Type(text, reply) => (reply, worker.type_text(&text)),
            Cmd::Chord(chord, reply) => (reply, worker.key_chord(&chord)),
            Cmd::Probe(reply) => (reply, worker.probe()),
        };
        let _ = reply.send(result.map_err(|e| e.to_string()));
    }
    // cmd_tx dropped → clean shutdown
    worker.shutdown();
}

impl Worker {
    fn new() -> anyhow::Result<Self> {
        Ok(Self {
            conn: zbus::blocking::Connection::session()?,
            inner: None,
        })
    }

    /// Drain whatever the server sent while nobody was typing. A dead
    /// session stays marked dead; the next command reconnects.
    fn service(&mut self) {
        if let Some(inner) = self.inner.as_mut() {
            if !inner.dead {
                if let Err(e) = inner.pump(Instant::now()) {
                    tracing::warn!("EIS session lost while idle: {e}");
                }
            }
        }
    }

    fn shutdown(&mut self) {
        if let Some(inner) = self.inner.take() {
            self.dbus_disconnect(inner.cookie);
        }
    }

    fn dbus_disconnect(&mut self, cookie: i32) {
        let _ = self.conn.call_method(
            Some(EIS_SERVICE),
            EIS_PATH,
            Some(EIS_IFACE),
            "disconnect",
            &(cookie),
        );
    }

    fn connect(&mut self) -> anyhow::Result<()> {
        let mut last_err = anyhow::anyhow!("no connection attempted");
        for attempt in 1..=RECONNECT_ATTEMPTS {
            match self.try_connect() {
                Ok(()) => {
                    tracing::debug!("EIS connect ok (attempt {attempt})");
                    return Ok(());
                }
                Err(e) => {
                    tracing::debug!("EIS connect attempt {attempt} failed: {e:#}");
                    last_err = e;
                    std::thread::sleep(Duration::from_millis(300));
                }
            }
        }
        Err(last_err)
    }

    fn try_connect(&mut self) -> anyhow::Result<()> {
        let reply = self
            .conn
            .call_method(
                Some(EIS_SERVICE),
                EIS_PATH,
                Some(EIS_IFACE),
                "connectToEIS",
                &(CAPS_KEYBOARD),
            )
            .map_err(|e| {
                anyhow::anyhow!("connectToEIS failed (KWin too old or plugin disabled): {e}")
            })?;
        let (fd, cookie): (zbus::zvariant::OwnedFd, i32) = reply.body().deserialize()?;
        // zvariant::OwnedFd -> std OwnedFd -> UnixStream (reis sets non-blocking)
        let std_fd: std::os::fd::OwnedFd = fd.into();
        let stream = UnixStream::from(std_fd);
        let context = ei::Context::new(stream)?;
        let _handshake = context.handshake();
        context.flush()?;

        let mut inner = Inner {
            context,
            cookie,
            device: None,
            keyboard: None,
            text: None,
            keymap: None,
            last_serial: u32::MAX,
            sequence: 0,
            capability_mask: 0,
            capability_bound: false,
            device_done: false,
            resumed: false,
            dead: false,
        };
        // KWin pauses the device right after the handshake; RESUMED normally
        // lands within ~0.5 s. Wait for the full ready state within budget.
        let deadline = Instant::now() + SETUP_BUDGET;
        while !inner.is_ready() && !inner.dead && Instant::now() < deadline {
            inner
                .pump(Instant::now() + Duration::from_millis(100))
                .map_err(anyhow::Error::msg)?;
        }
        anyhow::ensure!(!inner.dead, "EIS session died during setup");
        anyhow::ensure!(
            inner.is_ready(),
            "EIS setup timeout (bound={}, done={}, resumed={}, can_type={})",
            inner.capability_bound,
            inner.device_done,
            inner.resumed,
            inner.can_type()
        );
        self.inner = Some(inner);
        Ok(())
    }

    /// Make sure we hold a live, resumed session; reconnect when dead.
    fn ensure_ready(&mut self) -> anyhow::Result<()> {
        let dead = self.inner.as_ref().is_none_or(|i| i.dead);
        if dead {
            if let Some(old) = self.inner.take() {
                self.dbus_disconnect(old.cookie);
            }
            self.connect()?;
        }
        // drain events that arrived since the last call, whether or not we
        // looked ready: a PAUSE, keymap change or DISCONNECT that landed
        // while idle changes what we may do next
        let inner = self.inner.as_mut().expect("session set by connect()");
        inner.pump(Instant::now()).map_err(anyhow::Error::msg)?;
        let deadline = Instant::now() + Duration::from_millis(800);
        while !inner.is_ready() && !inner.dead && Instant::now() < deadline {
            inner
                .pump(Instant::now() + Duration::from_millis(50))
                .map_err(anyhow::Error::msg)?;
        }
        if inner.dead {
            // one retry through the reconnect path
            if let Some(old) = self.inner.take() {
                self.dbus_disconnect(old.cookie);
            }
            self.connect()?;
            return Ok(());
        }
        anyhow::ensure!(inner.is_ready(), "EIS session not ready (paused?)");
        Ok(())
    }

    /// Run one emulation burst: start_emulating → events → frame → stop.
    fn burst(&mut self, send: impl FnOnce(&mut Inner) -> anyhow::Result<()>) -> anyhow::Result<()> {
        self.ensure_ready()?;
        let inner = self.inner.as_mut().unwrap();
        let device = inner
            .device
            .clone()
            .ok_or_else(|| anyhow::anyhow!("no EIS device"))?;

        device.start_emulating(inner.last_serial, inner.sequence);
        inner.sequence += 1;
        let result = send(inner);
        device.stop_emulating(inner.last_serial);
        inner.context.flush()?;
        // read back: a protocol complaint arrives as DISCONNECT right away
        inner
            .pump(Instant::now() + POST_FLUSH_READ)
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            !inner.dead,
            "EIS session died mid-burst; text may not have landed"
        );
        result
    }

    fn type_text(&mut self, text: &str) -> anyhow::Result<()> {
        let chars: Vec<char> = text.chars().collect();
        self.burst(move |inner| {
            if let Some(t) = inner.text.clone() {
                // layout-independent UTF-8 path (KWin with ei_text support)
                let s: String = chars.into_iter().collect();
                let mut start = 0;
                while start < s.len() {
                    let end = floor_char_boundary(&s, start + 2048);
                    t.utf8(&s[start..end]);
                    inner.frame();
                    start = end;
                }
            } else {
                let keyboard = inner
                    .keyboard
                    .clone()
                    .ok_or_else(|| anyhow::anyhow!("no EIS keyboard"))?;
                let keymap = inner
                    .keymap
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("no EIS keymap"))?;
                let shift = keymap.shift;
                let altgr = keymap.altgr;
                // resolve first: char_keys needs &mut for its cache, the
                // key helpers need &Inner
                let resolved: Vec<(char, Result<CharKeys, String>)> =
                    chars.iter().map(|&c| (c, keymap.char_keys(c))).collect();
                let mut skipped = Vec::new();
                let mut typed = 0usize;
                use ei::keyboard::KeyState::{Press, Released};
                for (c, res) in resolved {
                    let Ok(ck) = res else {
                        // char not reachable on the active layout (emoji,
                        // accented letters on US, ...): type the rest and
                        // report it — losing one glyph beats losing the
                        // whole utterance, silently losing it is worse
                        skipped.push(c);
                        continue;
                    };
                    if ck.shift {
                        inner.modifier(&keyboard, shift, "Shift", Press);
                    }
                    if ck.altgr {
                        inner.modifier(&keyboard, altgr, "AltGr", Press);
                    }
                    inner.press_release(&keyboard, ck.keycode);
                    if ck.altgr {
                        inner.modifier(&keyboard, altgr, "AltGr", Released);
                    }
                    if ck.shift {
                        inner.modifier(&keyboard, shift, "Shift", Released);
                    }
                    typed += 1;
                }
                if !skipped.is_empty() {
                    let mut missing: Vec<char> = skipped;
                    missing.dedup();
                    anyhow::bail!(
                        "typed {typed} of {} chars; not on the active keymap: {}",
                        chars.len(),
                        missing
                            .iter()
                            .map(|c| format!("{c:?}"))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                }
            }
            Ok(())
        })
    }

    fn key_chord(&mut self, chord: &str) -> anyhow::Result<()> {
        let keys = normalize_chord(chord);
        anyhow::ensure!(!keys.is_empty(), "empty key chord");
        self.burst(move |inner| {
            use ei::keyboard::KeyState::{Press, Released};
            if let Some(t) = inner.text.clone() {
                // keysym path: server resolves against the active layout
                let syms: Vec<u32> = keys
                    .iter()
                    .map(|k| keysym_for(k).map(|s| s.raw()))
                    .collect::<anyhow::Result<_>>()?;
                let (mods, main) = syms.split_at(syms.len() - 1);
                for &m in mods {
                    inner.keysym(&t, m, Press);
                }
                for &m in main {
                    inner.keysym(&t, m, Press);
                    inner.keysym(&t, m, Released);
                }
                for &m in mods.iter().rev() {
                    inner.keysym(&t, m, Released);
                }
                return Ok(());
            }
            let keyboard = inner
                .keyboard
                .clone()
                .ok_or_else(|| anyhow::anyhow!("no EIS keyboard"))?;
            let keymap = inner
                .keymap
                .as_mut()
                .ok_or_else(|| anyhow::anyhow!("no EIS keymap"))?;
            // resolve every chord member to a raw xkb keycode via the keymap
            let mut keycodes = Vec::with_capacity(keys.len());
            for k in &keys {
                let sym = keysym_for(k)?;
                let (kc, _) = keymap
                    .locate(sym)
                    .ok_or_else(|| anyhow::anyhow!("key {k:?} not on the active keymap"))?;
                keycodes.push(kc);
            }
            let (mods, main) = keycodes.split_at(keycodes.len() - 1);
            for &kc in mods {
                inner.key(&keyboard, kc, Press);
            }
            for &kc in main {
                inner.press_release(&keyboard, kc);
            }
            for &kc in mods.iter().rev() {
                inner.key(&keyboard, kc, Released);
            }
            Ok(())
        })
    }

    fn probe(&mut self) -> anyhow::Result<()> {
        let inner = self
            .inner
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("no EIS session"))?;
        anyhow::ensure!(!inner.dead, "EIS session marked dead");
        // pump briefly to notice server-side disconnects
        inner
            .pump(Instant::now() + Duration::from_millis(50))
            .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(!inner.dead, "EIS session died during probe");
        Ok(())
    }
}

fn floor_char_boundary(s: &str, bound: usize) -> usize {
    if bound >= s.len() {
        return s.len();
    }
    let mut b = bound;
    while !s.is_char_boundary(b) {
        b -= 1;
    }
    b
}

/// Frame timestamps are CLOCK_MONOTONIC microseconds by protocol.
fn now_micros() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: clock_gettime writes into a valid, properly aligned timespec
    let rc = unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    if rc != 0 {
        return 0;
    }
    (ts.tv_sec as u64) * 1_000_000 + (ts.tv_nsec as u64) / 1_000
}

/// Chord key name → X11 keysym. Used for both the ei_text.keysym path and
/// keymap lookups on the ei_keyboard path.
fn keysym_for(key: &str) -> anyhow::Result<xkb::Keysym> {
    use xkb::Keysym;
    let sym = match key {
        "ctrl" | "control" => Keysym::Control_L,
        "alt" => Keysym::Alt_L,
        "shift" => Keysym::Shift_L,
        "meta" | "super" | "win" => Keysym::Super_L,
        "altgr" => Keysym::ISO_Level3_Shift,
        "enter" | "return" => Keysym::Return,
        "tab" => Keysym::Tab,
        "escape" | "esc" => Keysym::Escape,
        "backspace" => Keysym::BackSpace,
        "delete" | "del" => Keysym::Delete,
        "insert" | "ins" => Keysym::Insert,
        "home" => Keysym::Home,
        "end" => Keysym::End,
        "pageup" | "pgup" => Keysym::Page_Up,
        "pagedown" | "pgdn" => Keysym::Page_Down,
        "left" => Keysym::Left,
        "up" => Keysym::Up,
        "right" => Keysym::Right,
        "down" => Keysym::Down,
        "space" => Keysym::space,
        "f1" => Keysym::F1,
        "f2" => Keysym::F2,
        "f3" => Keysym::F3,
        "f4" => Keysym::F4,
        "f5" => Keysym::F5,
        "f6" => Keysym::F6,
        "f7" => Keysym::F7,
        "f8" => Keysym::F8,
        "f9" => Keysym::F9,
        "f10" => Keysym::F10,
        "f11" => Keysym::F11,
        "f12" => Keysym::F12,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => {
                    let sym = Keysym::from_char(c);
                    anyhow::ensure!(sym != Keysym::NoSymbol, "no keysym for {other:?}");
                    sym
                }
                _ => anyhow::bail!("no keysym mapping for {other:?}"),
            }
        }
    };
    Ok(sym)
}
