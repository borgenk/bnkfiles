//! A keyboard and a mouse that exist only inside the test compositor.
//!
//! wlroots compositors let a client create virtual input devices. What this
//! sends, the compositor delivers to the focused window exactly as it would a
//! real key press or click, so the app's whole input path runs: focus, the
//! keymap, modifiers, key repeat.
//!
//! It speaks the wire format through the app's own connection code, which is
//! compiled into this test from `src/platform/`.

use std::io::Write;
use std::os::fd::AsRawFd;
use std::path::Path;
use std::time::{Duration, Instant};

use crate::platform::conn::{Connection, Fill};
use crate::platform::wire::{Arg, Reader};

/// Linux evdev key codes, as the virtual keyboard takes them.
pub mod key {
    pub const ESC: u32 = 1;
    pub const BACKSPACE: u32 = 14;
    pub const TAB: u32 = 15;
    pub const SLASH: u32 = 53;
    pub const A: u32 = 30;
    pub const P: u32 = 25;
    pub const L: u32 = 38;
    pub const T: u32 = 20;
    pub const R: u32 = 19;
    pub const S: u32 = 31;
    pub const N1: u32 = 2;
    pub const N2: u32 = 3;
    pub const N3: u32 = 4;
    pub const Q: u32 = 16;
    pub const ENTER: u32 = 28;
    pub const I: u32 = 23;
    pub const D: u32 = 32;
    pub const F: u32 = 33;
    pub const V: u32 = 47;
    pub const G: u32 = 34;
    pub const B: u32 = 48;
    pub const X: u32 = 45;
    pub const F2: u32 = 60;
    pub const F5: u32 = 63;
    pub const HOME: u32 = 102;
    pub const UP: u32 = 103;
    pub const LEFT: u32 = 105;
    pub const RIGHT: u32 = 106;
    pub const END: u32 = 107;
    pub const DOWN: u32 = 108;
    pub const PAGEDOWN: u32 = 109;
    pub const DELETE: u32 = 111;
    pub(super) const SHIFT: u32 = 42;
    pub(super) const CTRL: u32 = 29;
    pub(super) const ALT: u32 = 56;
    /// The Super key, which the rig's compositor bindings hang off.
    pub(super) const SUPER: u32 = 125;
}

/// A US layout, by reference to the system's xkb data.
const KEYMAP: &str = "xkb_keymap {\n\
    xkb_keycodes { include \"evdev+aliases(qwerty)\" };\n\
    xkb_types { include \"complete\" };\n\
    xkb_compat { include \"complete\" };\n\
    xkb_symbols { include \"pc+us+inet(evdev)\" };\n\
};\n";

const WL_DISPLAY: u32 = 1;
const DISPLAY_SYNC: u16 = 0;
const DISPLAY_GET_REGISTRY: u16 = 1;
const DISPLAY_EV_ERROR: u16 = 0;
const REGISTRY_BIND: u16 = 0;
const REGISTRY_EV_GLOBAL: u16 = 0;

const KEYBOARD_MANAGER_CREATE: u16 = 0;
const KEYBOARD_KEYMAP: u16 = 0;
const KEYBOARD_KEY: u16 = 1;
const KEYBOARD_MODIFIERS: u16 = 2;
/// Super is Mod4 in the keymap, the seventh modifier.
const MOD_SUPER: u32 = 1 << 6;
const MOD_SHIFT: u32 = 1;
const MOD_CTRL: u32 = 1 << 2;
const MOD_ALT: u32 = 1 << 3;

const OUTPUT_MANAGER_CREATE_CONFIGURATION: u16 = 0;
const OUTPUT_MANAGER_EV_HEAD: u16 = 0;
const OUTPUT_MANAGER_EV_DONE: u16 = 1;
const CONFIGURATION_ENABLE_HEAD: u16 = 0;
const CONFIGURATION_APPLY: u16 = 2;
const CONFIGURATION_EV_SUCCEEDED: u16 = 0;
const CONFIGURATION_HEAD_SET_SCALE: u16 = 4;
const KEYMAP_FORMAT_XKB_V1: u32 = 1;

const POINTER_MANAGER_CREATE: u16 = 0;
const POINTER_MOTION_ABSOLUTE: u16 = 1;
const POINTER_BUTTON: u16 = 2;
const POINTER_FRAME: u16 = 4;
const POINTER_AXIS_SOURCE: u16 = 5;
const POINTER_AXIS_DISCRETE: u16 = 7;
const AXIS_SOURCE_WHEEL: u32 = 0;
/// What one notch of a wheel is worth, in the axis units a compositor reports.
const NOTCH: i32 = 15;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
/// The thumb buttons most mice send for back and forward.
pub const BTN_SIDE: u32 = 0x113;
pub const BTN_EXTRA: u32 = 0x114;
const AXIS_VERTICAL: u32 = 0;

const RELEASED: u32 = 0;
const PRESSED: u32 = 1;

const ROUNDTRIP_DEADLINE: Duration = Duration::from_secs(5);

pub struct Input {
    conn: Connection,
    next_id: u32,
    keyboard: u32,
    pointer: u32,
    /// The output manager, the one output it announced, and the serial of the
    /// state it last described, which a change has to quote.
    output_manager: u32,
    output_head: u32,
    output_serial: u32,
    started: Instant,
}

impl Input {
    /// Connect to the compositor at `socket` and create one keyboard and one
    /// pointer. `scratch` is a directory to put the keymap file in.
    pub fn connect(socket: &Path, scratch: &Path) -> Input {
        let conn = Connection::connect_to(socket).expect("connect the input driver");
        let mut input = Input {
            conn,
            next_id: 2,
            keyboard: 0,
            pointer: 0,
            output_manager: 0,
            output_head: 0,
            output_serial: 0,
            started: Instant::now(),
        };

        let registry = input.new_id();
        input
            .conn
            .request(WL_DISPLAY, DISPLAY_GET_REGISTRY, &[Arg::NewId(registry)]);
        let mut globals: Vec<(u32, String, u32)> = Vec::new();
        input.roundtrip(|object, opcode, body| {
            if object == registry && opcode == REGISTRY_EV_GLOBAL {
                let mut r = Reader::new(body);
                let name = r.u32().expect("global name");
                let interface = r.string().expect("global interface").to_string();
                let version = r.u32().expect("global version");
                globals.push((name, interface, version));
            }
        });

        let seat = input.bind(registry, &globals, "wl_seat", 1);
        let keyboards = input.bind(registry, &globals, "zwp_virtual_keyboard_manager_v1", 1);
        let pointers = input.bind(registry, &globals, "zwlr_virtual_pointer_manager_v1", 2);

        input.keyboard = input.new_id();
        input.conn.request(
            keyboards,
            KEYBOARD_MANAGER_CREATE,
            &[Arg::Object(seat), Arg::NewId(input.keyboard)],
        );
        // The keymap travels as a file descriptor. The compositor maps it, so
        // the file has to outlive this call; it lives in the rig's directory.
        let path = scratch.join("keymap.xkb");
        let mut file = std::fs::File::create(&path).expect("create the keymap file");
        file.write_all(KEYMAP.as_bytes()).expect("write the keymap");
        file.write_all(&[0]).expect("terminate the keymap");
        let file = std::fs::File::open(&path).expect("reopen the keymap file");
        input
            .conn
            .request_with_fd(
                input.keyboard,
                KEYBOARD_KEYMAP,
                &[
                    Arg::Uint(KEYMAP_FORMAT_XKB_V1),
                    Arg::Uint(KEYMAP.len() as u32 + 1),
                ],
                file.as_raw_fd(),
            )
            .expect("send the keymap");

        input.pointer = input.new_id();
        input.conn.request(
            pointers,
            POINTER_MANAGER_CREATE,
            &[Arg::Object(seat), Arg::NewId(input.pointer)],
        );
        // Binding the output manager makes the compositor describe its outputs;
        // the roundtrip below reads that, and every later one keeps it current.
        input.output_manager = input.bind(registry, &globals, "zwlr_output_manager_v1", 1);
        input.settle();
        assert_ne!(input.output_head, 0, "the compositor announced no output");
        input
    }

    /// Set the scale of the compositor's output, as a display settings panel
    /// would. The app hears of it as a new preferred scale for its window.
    pub fn set_output_scale(&mut self, scale: f64) {
        let configuration = self.new_id();
        self.conn.request(
            self.output_manager,
            OUTPUT_MANAGER_CREATE_CONFIGURATION,
            &[Arg::NewId(configuration), Arg::Uint(self.output_serial)],
        );
        let head = self.new_id();
        self.conn.request(
            configuration,
            CONFIGURATION_ENABLE_HEAD,
            &[Arg::NewId(head), Arg::Object(self.output_head)],
        );
        self.conn.request(
            head,
            CONFIGURATION_HEAD_SET_SCALE,
            // A wl_fixed: 24.8 fixed point.
            &[Arg::Int((scale * 256.0).round() as i32)],
        );
        self.conn.request(configuration, CONFIGURATION_APPLY, &[]);
        let mut applied = false;
        self.roundtrip(|object, opcode, _| {
            if object == configuration && opcode == CONFIGURATION_EV_SUCCEEDED {
                applied = true;
            }
        });
        assert!(applied, "the compositor refused output scale {scale}");
    }

    /// Press `code` while Shift is held.
    pub fn shift(&mut self, code: u32) {
        self.key(key::SHIFT, PRESSED);
        self.modifiers(MOD_SHIFT);
        self.key(code, PRESSED);
        self.key(code, RELEASED);
        self.key(key::SHIFT, RELEASED);
        self.modifiers(0);
        self.settle();
    }

    /// Press `code` while Ctrl is held.
    pub fn ctrl(&mut self, code: u32) {
        self.key(key::CTRL, PRESSED);
        self.modifiers(MOD_CTRL);
        self.key(code, PRESSED);
        self.key(code, RELEASED);
        self.key(key::CTRL, RELEASED);
        self.modifiers(0);
        self.settle();
    }

    /// Press `code` while Alt is held.
    pub fn alt(&mut self, code: u32) {
        self.key(key::ALT, PRESSED);
        self.modifiers(MOD_ALT);
        self.key(code, PRESSED);
        self.key(code, RELEASED);
        self.key(key::ALT, RELEASED);
        self.modifiers(0);
        self.settle();
    }

    /// Move the pointer to `(x, y)` on an output of `extent` pixels, without
    /// pressing anything.
    pub fn point_at(&mut self, x: u32, y: u32, extent: (u32, u32)) {
        self.move_to(x, y, extent);
    }

    /// Press and release `code`.
    pub fn tap(&mut self, code: u32) {
        self.key(code, PRESSED);
        self.key(code, RELEASED);
        self.settle();
    }

    /// Press `code` while Super is held: one of the rig's compositor bindings.
    ///
    /// A virtual keyboard has to state the modifiers itself. The compositor
    /// does not work them out from the key presses, as it would for a real
    /// keyboard.
    pub fn super_chord(&mut self, code: u32) {
        self.key(key::SUPER, PRESSED);
        self.modifiers(MOD_SUPER);
        self.key(code, PRESSED);
        self.key(code, RELEASED);
        self.key(key::SUPER, RELEASED);
        self.modifiers(0);
        self.settle();
    }

    /// Hold `code` down for `held`, long enough for the app's key repeat to
    /// run, then let go.
    pub fn hold(&mut self, code: u32, held: Duration) {
        self.key(code, PRESSED);
        self.settle();
        std::thread::sleep(held);
        self.key(code, RELEASED);
        self.settle();
    }

    /// Press `code` with Alt held, let go of Alt at once, and keep `code`
    /// down for `held`: a chord whose modifier is released first.
    pub fn alt_then_hold(&mut self, code: u32, held: Duration) {
        self.key(key::ALT, PRESSED);
        self.modifiers(MOD_ALT);
        self.key(code, PRESSED);
        self.key(key::ALT, RELEASED);
        self.modifiers(0);
        self.settle();
        std::thread::sleep(held);
        self.key(code, RELEASED);
        self.settle();
    }

    /// Click the left button at `(x, y)` on an output of `extent` pixels.
    pub fn click(&mut self, x: u32, y: u32, extent: (u32, u32)) {
        self.click_with(BTN_LEFT, x, y, extent);
    }

    /// Click the right button at `(x, y)` on an output of `extent` pixels.
    pub fn right_click(&mut self, x: u32, y: u32, extent: (u32, u32)) {
        self.click_with(BTN_RIGHT, x, y, extent);
    }

    /// Click mouse button `button` (a Linux `BTN_*` code) at `(x, y)` on an
    /// output of `extent` pixels.
    pub fn click_with(&mut self, button: u32, x: u32, y: u32, extent: (u32, u32)) {
        self.move_to(x, y, extent);
        for state in [PRESSED, RELEASED] {
            let time = self.time();
            self.conn.request(
                self.pointer,
                POINTER_BUTTON,
                &[Arg::Uint(time), Arg::Uint(button), Arg::Uint(state)],
            );
            self.conn.request(self.pointer, POINTER_FRAME, &[]);
        }
        self.settle();
    }

    /// Turn the wheel by `notches` with Ctrl held, the pointer at `(x, y)` on
    /// an output of `extent` pixels.
    pub fn ctrl_wheel_at(&mut self, x: u32, y: u32, extent: (u32, u32), notches: i32) {
        self.move_to(x, y, extent);
        self.key(key::CTRL, PRESSED);
        self.modifiers(MOD_CTRL);
        self.settle();
        self.wheel(notches);
        self.key(key::CTRL, RELEASED);
        self.modifiers(0);
        self.settle();
    }

    /// Put the pointer at `(x, y)` on an output of `extent` pixels.
    pub fn move_to(&mut self, x: u32, y: u32, extent: (u32, u32)) {
        let time = self.time();
        self.conn.request(
            self.pointer,
            POINTER_MOTION_ABSOLUTE,
            &[
                Arg::Uint(time),
                Arg::Uint(x),
                Arg::Uint(y),
                Arg::Uint(extent.0),
                Arg::Uint(extent.1),
            ],
        );
        self.conn.request(self.pointer, POINTER_FRAME, &[]);
        self.settle();
    }

    /// Turn the wheel by `notches`, positive scrolling down.
    pub fn wheel(&mut self, notches: i32) {
        let time = self.time();
        self.conn.request(
            self.pointer,
            POINTER_AXIS_SOURCE,
            &[Arg::Uint(AXIS_SOURCE_WHEEL)],
        );
        self.conn.request(
            self.pointer,
            POINTER_AXIS_DISCRETE,
            &[
                Arg::Uint(time),
                Arg::Uint(AXIS_VERTICAL),
                // A wl_fixed: 24.8 fixed point.
                Arg::Int(notches * NOTCH * 256),
                Arg::Int(notches),
            ],
        );
        self.conn.request(self.pointer, POINTER_FRAME, &[]);
        self.settle();
    }

    fn key(&mut self, code: u32, state: u32) {
        let time = self.time();
        self.conn.request(
            self.keyboard,
            KEYBOARD_KEY,
            &[Arg::Uint(time), Arg::Uint(code), Arg::Uint(state)],
        );
    }

    fn modifiers(&mut self, depressed: u32) {
        self.conn.request(
            self.keyboard,
            KEYBOARD_MODIFIERS,
            &[
                Arg::Uint(depressed),
                Arg::Uint(0),
                Arg::Uint(0),
                Arg::Uint(0),
            ],
        );
    }

    /// Wait until the compositor has handled everything sent so far.
    fn settle(&mut self) {
        self.roundtrip(|_, _, _| {});
    }

    fn time(&self) -> u32 {
        self.started.elapsed().as_millis() as u32
    }

    fn new_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn bind(
        &mut self,
        registry: u32,
        globals: &[(u32, String, u32)],
        interface: &str,
        version: u32,
    ) -> u32 {
        let (name, _, _) = globals
            .iter()
            .find(|(_, i, _)| i == interface)
            .unwrap_or_else(|| panic!("the compositor does not offer {interface}"));
        let id = self.new_id();
        self.conn.request(
            registry,
            REGISTRY_BIND,
            &[
                Arg::Uint(*name),
                Arg::Bind {
                    interface,
                    version,
                    new_id: id,
                },
            ],
        );
        id
    }

    /// Send a `wl_display.sync` and handle events until its callback fires.
    fn roundtrip(&mut self, mut on_event: impl FnMut(u32, u16, &[u8])) {
        let callback = self.new_id();
        self.conn
            .request(WL_DISPLAY, DISPLAY_SYNC, &[Arg::NewId(callback)]);
        self.conn.flush().expect("flush the input driver");
        let deadline = Instant::now() + ROUNDTRIP_DEADLINE;
        loop {
            while let Some(msg) = self.conn.next_message().expect("read an event") {
                if msg.object == callback {
                    return;
                }
                if msg.object == WL_DISPLAY && msg.opcode == DISPLAY_EV_ERROR {
                    let mut r = Reader::new(&msg.body);
                    let object = r.u32().unwrap_or(0);
                    let code = r.u32().unwrap_or(0);
                    let text = r.string().unwrap_or("?").to_string();
                    panic!("the compositor refused the input driver: object {object} code {code}: {text}");
                }
                if msg.object == self.output_manager && self.output_manager != 0 {
                    let mut r = Reader::new(&msg.body);
                    match msg.opcode {
                        // Only the first output: a headless rig has one.
                        OUTPUT_MANAGER_EV_HEAD if self.output_head == 0 => {
                            self.output_head = r.u32().expect("output head id");
                        }
                        OUTPUT_MANAGER_EV_DONE => {
                            self.output_serial = r.u32().expect("output serial");
                        }
                        _ => {}
                    }
                }
                on_event(msg.object, msg.opcode, &msg.body);
            }
            let left = deadline
                .checked_duration_since(Instant::now())
                .expect("the compositor stopped answering the input driver");
            if let Fill::Bytes(0) = self
                .conn
                .fill(Some(left))
                .expect("read from the compositor")
            {
                panic!("the compositor closed the input driver's connection");
            }
        }
    }
}
