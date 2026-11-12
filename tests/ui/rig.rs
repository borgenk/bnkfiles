//! The rig: a headless compositor of the test's own, the real binary running
//! inside it, and the three ways a test reaches it.
//!
//! ```text
//!   test ──keys, clicks──▶ Input ──virtual devices──▶ labwc ──▶ bnkfiles
//!     ▲                                                 │           │
//!     ├──────────── screenshot (grim) ◀─────────────────┘           │
//!     └──────────── probe lines (stdout) ◀──────────────────────────┘
//! ```
//!
//! Everything a run touches lives under one temp directory: the compositor's
//! socket, its configuration, the app's settings, cache and trash, and the
//! folder of pictures it opens. Nothing reaches the session the tests are
//! started from, and two tests never share a compositor.

use std::fs;
use std::io::{BufRead, BufReader};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::time::{Duration, Instant};

use crate::input::Input;

/// The headless output wlroots creates.
pub const OUTPUT: (u32, u32) = (1280, 720);

/// How long anything is given to happen before the test fails. Generous: a
/// loaded machine should fail a test for being wrong, not for being slow.
const DEADLINE: Duration = Duration::from_secs(15);

/// Compositor key bindings every rig has, all on Super so they cannot collide
/// with the app's own. The window actions a person would do with the mouse.
const RC_XML: &str = r#"<?xml version="1.0"?>
<labwc_config>
  <desktops number="2"/>
  <keyboard>
    <keybind key="W-m"><action name="ToggleMaximize"/></keybind>
    <keybind key="W-f"><action name="ToggleFullscreen"/></keybind>
    <keybind key="W-x"><action name="Close"/></keybind>
    <keybind key="W-d"><action name="ToggleShowDesktop"/></keybind>
    <keybind key="W-Right"><action name="GoToDesktop" to="right" wrap="yes"/></keybind>
    <keybind key="W-1"><action name="ResizeTo" width="520" height="400"/></keybind>
    <keybind key="W-2"><action name="ResizeTo" width="1000" height="640"/></keybind>
    <keybind key="W-3"><action name="ResizeTo" width="700" height="500"/></keybind>
  </keyboard>
</labwc_config>
"#;

/// The app's state as of one presented frame. See `src/app/probe.rs`.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    pub frame: u64,
    pub mode: String,
    pub size: (u32, u32),
    pub rows: usize,
    pub lead: Option<usize>,
    pub selected: usize,
    pub scan_done: bool,
    pub decoding: usize,
    pub retired: usize,
    pub index: usize,
    pub shown: Option<usize>,
    pub prompt: bool,
    pub framed: bool,
    pub back: usize,
    pub forward: usize,
    pub inspector: bool,
    pub sidebar: bool,
    pub filter: bool,
    pub total: usize,
    pub images: usize,
    pub places: usize,
    pub collections: usize,
    pub sort: String,
    /// The thumbnail shape chosen and the one the grid is in, as
    /// `chosen:now`; `now` is `-` while auto has not settled.
    pub shape: String,
    /// How many commands the command list shows, if it is up.
    pub menu: Option<usize>,
    pub focus: String,
    /// Why the folder could not be read (`missing`, `denied`, …), if it
    /// could not.
    pub unreadable: Option<String>,
    /// The level of the notice on the notification line, if one is up.
    pub notice: Option<String>,
    /// How many of the sidebar's places are known to be gone.
    pub gone: usize,
    pub dir: String,
}

impl Probe {
    fn parse(line: &str) -> Option<Probe> {
        let rest = line.strip_prefix("probe ")?;
        let (fields, dir) = rest.split_once(" dir=")?;
        let mut probe = Probe {
            dir: dir.to_string(),
            ..Probe::default()
        };
        for field in fields.split(' ') {
            let (key, value) = field.split_once('=')?;
            match key {
                "frame" => probe.frame = value.parse().ok()?,
                "mode" => probe.mode = value.to_string(),
                "size" => {
                    let (w, h) = value.split_once('x')?;
                    probe.size = (w.parse().ok()?, h.parse().ok()?);
                }
                "rows" => probe.rows = value.parse().ok()?,
                "lead" => probe.lead = value.parse().ok(),
                "selected" => probe.selected = value.parse().ok()?,
                "scan" => probe.scan_done = value == "done",
                "decoding" => probe.decoding = value.parse().ok()?,
                "retired" => probe.retired = value.parse().ok()?,
                "index" => probe.index = value.parse().ok()?,
                "shown" => probe.shown = value.parse().ok(),
                "prompt" => probe.prompt = value == "1",
                "framed" => probe.framed = value == "1",
                "back" => probe.back = value.parse().ok()?,
                "forward" => probe.forward = value.parse().ok()?,
                "inspector" => probe.inspector = value == "1",
                "sidebar" => probe.sidebar = value == "1",
                "filter" => probe.filter = value == "1",
                "total" => probe.total = value.parse().ok()?,
                "images" => probe.images = value.parse().ok()?,
                "places" => probe.places = value.parse().ok()?,
                "collections" => probe.collections = value.parse().ok()?,
                "sort" => probe.sort = value.to_string(),
                "shape" => probe.shape = value.to_string(),
                "menu" => probe.menu = value.parse().ok(),
                "focus" => probe.focus = value.to_string(),
                "unreadable" => probe.unreadable = (value != "-").then(|| value.to_string()),
                "notice" => probe.notice = (value != "-").then(|| value.to_string()),
                "gone" => probe.gone = value.parse().ok()?,
                _ => {}
            }
        }
        Some(probe)
    }

    /// Nothing left for the app to do: the folder is read and no picture is
    /// still being decoded.
    pub fn idle(&self) -> bool {
        self.scan_done && self.decoding == 0
    }
}

pub struct Rig {
    dir: PathBuf,
    compositor: Child,
    app: Child,
    /// Further windows a test opened beside the app under test.
    others: Vec<Child>,
    probes: Receiver<Probe>,
    /// The newest probe line read so far.
    pub last: Probe,
    pub input: Input,
    shots: usize,
}

impl Rig {
    /// Start a compositor and open the app on a folder that `fixture` fills.
    /// `settings` is the text of the app's config file.
    pub fn start(name: &str, settings: &str, fixture: impl FnOnce(&Path)) -> Rig {
        Self::start_on(name, settings, fixture, None)
    }

    /// [`Rig::start`], opening `target` inside the folder rather than the folder
    /// itself when one is named: a file opens straight into the viewer.
    pub fn start_on(
        name: &str,
        settings: &str,
        fixture: impl FnOnce(&Path),
        target: Option<&str>,
    ) -> Rig {
        let dir = std::env::temp_dir().join(format!("bnkfiles-ui-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        for sub in [
            "runtime",
            "config/labwc",
            "config/bnkfiles",
            "cache",
            "data",
            "folder",
        ] {
            fs::create_dir_all(dir.join(sub)).expect("create the rig directory");
        }
        // The runtime directory holds the compositor's socket, and a compositor
        // refuses one that others can read.
        fs::set_permissions(dir.join("runtime"), fs::Permissions::from_mode(0o700))
            .expect("restrict the runtime directory");
        fs::write(dir.join("config/labwc/rc.xml"), RC_XML).expect("write rc.xml");
        fs::write(dir.join("config/bnkfiles/config"), settings).expect("write the app settings");
        fixture(&dir.join("folder"));

        // Compositor output goes to a file, not a pipe: it writes more than a
        // pipe holds and nothing reads it until a test fails.
        let log = fs::File::create(dir.join("labwc.log")).expect("open the compositor log");
        let compositor = Command::new("labwc")
            .arg("-C")
            .arg(dir.join("config/labwc"))
            .envs(Self::env(&dir))
            .env("WLR_BACKENDS", "headless")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            // Started from inside a session it would otherwise nest in it.
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log))
            .spawn()
            .expect("start labwc (is it installed?)");

        let socket = dir.join("runtime/wayland-0");
        let deadline = Instant::now() + DEADLINE;
        while !socket.exists() {
            assert!(
                Instant::now() < deadline,
                "labwc never opened its socket; see {}",
                dir.join("labwc.log").display()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        let input = Input::connect(&socket, &dir);

        let errors = fs::File::create(dir.join("app.log")).expect("open the app log");
        let mut app = Command::new(env!("CARGO_BIN_EXE_bnkfiles"))
            .arg(match target {
                Some(file) => dir.join("folder").join(file),
                None => dir.join("folder"),
            })
            .envs(Self::env(&dir))
            .env("WAYLAND_DISPLAY", "wayland-0")
            .env("BNKFILES_PROBE", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(errors))
            .spawn()
            .expect("start bnkfiles");

        let stdout = app.stdout.take().expect("the app's stdout");
        let (tx, probes) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                if let Some(probe) = Probe::parse(&line) {
                    if tx.send(probe).is_err() {
                        break;
                    }
                }
            }
        });

        Rig {
            dir,
            compositor,
            app,
            others: Vec::new(),
            probes,
            last: Probe::default(),
            input,
            shots: 0,
        }
    }

    fn env(dir: &Path) -> [(&'static str, PathBuf); 4] {
        [
            ("XDG_RUNTIME_DIR", dir.join("runtime")),
            ("XDG_CONFIG_HOME", dir.join("config")),
            ("XDG_CACHE_HOME", dir.join("cache")),
            ("XDG_DATA_HOME", dir.join("data")),
        ]
    }

    /// The folder the app was opened on.
    pub fn folder(&self) -> PathBuf {
        self.dir.join("folder")
    }

    /// The app's settings file as it is on disk now.
    pub fn settings(&self) -> String {
        fs::read_to_string(self.dir.join("config/bnkfiles/config")).unwrap_or_default()
    }

    /// Where the app would put trashed files.
    pub fn trash(&self) -> PathBuf {
        self.dir.join("data/Trash")
    }

    /// Where the app would save thumbnails.
    pub fn thumbnail_cache(&self) -> PathBuf {
        self.dir.join("cache/bnkfiles")
    }

    /// Open a second window of the app, on an empty folder. The compositor
    /// gives a new window the focus and puts it on top.
    pub fn open_second_window(&mut self) {
        let folder = self.dir.join("second");
        fs::create_dir_all(&folder).expect("create the second folder");
        let child = Command::new(env!("CARGO_BIN_EXE_bnkfiles"))
            .arg(folder)
            .envs(Self::env(&self.dir))
            .env("WAYLAND_DISPLAY", "wayland-0")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("start a second bnkfiles");
        self.others.push(child);
    }

    /// Read probe lines until one satisfies `done`, and return it. Starts with
    /// the newest line already read, so a state the app is already in counts.
    pub fn wait(&mut self, what: &str, done: impl Fn(&Probe) -> bool) -> Probe {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if done(&self.last) {
                return self.last.clone();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            match self.probes.recv_timeout(left) {
                Ok(probe) => self.last = probe,
                Err(RecvTimeoutError::Timeout) => {
                    self.fail(&format!("timed out waiting for {what}"))
                }
                Err(RecvTimeoutError::Disconnected) => {
                    self.fail(&format!("the app exited while waiting for {what}"))
                }
            }
        }
    }

    /// Read whatever probe lines have arrived, without waiting for more than
    /// `quiet` of silence. For checking that something did *not* change.
    pub fn drain(&mut self, quiet: Duration) -> Probe {
        while let Ok(probe) = self.probes.recv_timeout(quiet) {
            self.last = probe;
        }
        self.last.clone()
    }

    /// Wait for the app to open its window, read the folder and finish
    /// decoding.
    pub fn ready(&mut self) -> Probe {
        self.wait("the folder to load", |p| p.frame > 0 && p.idle())
    }

    /// Capture the compositor's output to a PNG in the rig's directory and
    /// return its path and pixels.
    pub fn screenshot(&mut self, tag: &str) -> Shot {
        self.shots += 1;
        let path = self.dir.join(format!("{:02}-{tag}.png", self.shots));
        let out = Command::new("grim")
            .arg(&path)
            .envs(Self::env(&self.dir))
            .env("WAYLAND_DISPLAY", "wayland-0")
            .output()
            .expect("run grim (is it installed?)");
        if !out.status.success() {
            self.fail(&format!(
                "grim failed: {}",
                String::from_utf8_lossy(&out.stderr)
            ));
        }
        Shot::load(path)
    }

    /// Take screenshots until one differs from `baseline`: something new is on
    /// the output, such as a window that has just opened.
    pub fn wait_for_screen_to_change(&mut self, baseline: &Shot) {
        let deadline = Instant::now() + DEADLINE;
        while self.screenshot("waiting").difference(baseline) == 0.0 {
            if Instant::now() >= deadline {
                self.fail("the screen never changed");
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Wait for the app to exit by itself and return how it went.
    pub fn exit_status(&mut self) -> ExitStatus {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Some(status) = self.app.try_wait().expect("wait for the app") {
                return status;
            }
            if Instant::now() >= deadline {
                self.fail("the app did not exit");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// What the app wrote to stderr so far.
    pub fn app_log(&self) -> String {
        fs::read_to_string(self.dir.join("app.log")).unwrap_or_default()
    }

    fn fail(&self, why: &str) -> ! {
        panic!(
            "{why}\nlast probe: {:?}\napp stderr:\n{}\nrig kept at {}",
            self.last,
            self.app_log(),
            self.dir.display()
        );
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = self.app.kill();
        let _ = self.app.wait();
        for other in &mut self.others {
            let _ = other.kill();
            let _ = other.wait();
        }
        let _ = self.compositor.kill();
        let _ = self.compositor.wait();
        // A failed test keeps its directory: the screenshots and logs in it
        // are what explain the failure.
        if !std::thread::panicking() && std::env::var_os("BNKFILES_UI_KEEP").is_none() {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }
}

/// A screenshot: where it was saved, and its pixels as RGB.
pub struct Shot {
    pub path: PathBuf,
    pub width: usize,
    pub height: usize,
    pixels: Vec<[u8; 3]>,
}

impl Shot {
    fn load(path: PathBuf) -> Shot {
        let bytes = fs::read(&path).expect("read the screenshot");
        let options = zune_core::options::DecoderOptions::default().png_set_strip_to_8bit(true);
        let mut decoder =
            zune_png::PngDecoder::new_with_options(std::io::Cursor::new(bytes), options);
        let samples = match decoder.decode().expect("decode the screenshot") {
            zune_core::result::DecodingResult::U8(samples) => samples,
            _ => panic!("the screenshot is not 8 bits a channel"),
        };
        let (width, height) = decoder.dimensions().expect("the screenshot's size");
        let channels = samples.len() / (width * height);
        let pixels = samples
            .chunks_exact(channels)
            .map(|p| [p[0], p[1], p[2]])
            .collect();
        Shot {
            path,
            width,
            height,
            pixels,
        }
    }

    pub fn pixel(&self, x: usize, y: usize) -> [u8; 3] {
        self.pixels[y * self.width + x]
    }

    /// How many pixels are within `tolerance` of `colour` on every channel.
    pub fn count_near(&self, colour: [u8; 3], tolerance: u8) -> usize {
        self.pixels
            .iter()
            .filter(|p| {
                p.iter()
                    .zip(colour)
                    .all(|(a, b)| a.abs_diff(b) <= tolerance)
            })
            .count()
    }

    /// How many pixels in the rectangle `(x, y, w, h)` are within
    /// `tolerance` of `colour` on every channel.
    pub fn count_near_in(
        &self,
        (x, y, w, h): (usize, usize, usize, usize),
        colour: [u8; 3],
        tolerance: u8,
    ) -> usize {
        (y..(y + h).min(self.height))
            .flat_map(|yy| (x..(x + w).min(self.width)).map(move |xx| (xx, yy)))
            .filter(|&(xx, yy)| {
                self.pixel(xx, yy)
                    .iter()
                    .zip(colour)
                    .all(|(a, b)| a.abs_diff(b) <= tolerance)
            })
            .count()
    }

    /// The smallest rectangle `(x, y, w, h)` holding every pixel within
    /// `tolerance` of `colour`, or `None` if there is none.
    pub fn bounds_of(
        &self,
        colour: [u8; 3],
        tolerance: u8,
    ) -> Option<(usize, usize, usize, usize)> {
        let mut found: Option<(usize, usize, usize, usize)> = None;
        for (i, p) in self.pixels.iter().enumerate() {
            if !p
                .iter()
                .zip(colour)
                .all(|(a, b)| a.abs_diff(b) <= tolerance)
            {
                continue;
            }
            let (x, y) = (i % self.width, i / self.width);
            found = Some(match found {
                None => (x, y, x, y),
                Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
            });
        }
        found.map(|(x0, y0, x1, y1)| (x0, y0, x1 - x0 + 1, y1 - y0 + 1))
    }

    /// The darkest and lightest value of the first channel in the rectangle.
    pub fn range_in(&self, x: usize, y: usize, w: usize, h: usize) -> (u8, u8) {
        let mut range = (255, 0);
        for yy in y..(y + h).min(self.height) {
            for xx in x..(x + w).min(self.width) {
                let v = self.pixel(xx, yy)[0];
                range = (range.0.min(v), range.1.max(v));
            }
        }
        range
    }

    /// The fraction of pixels that differ between two screenshots of the same
    /// size.
    pub fn difference(&self, other: &Shot) -> f64 {
        assert_eq!((self.width, self.height), (other.width, other.height));
        let differing = self
            .pixels
            .iter()
            .zip(&other.pixels)
            .filter(|(a, b)| a != b)
            .count();
        differing as f64 / self.pixels.len() as f64
    }
}
