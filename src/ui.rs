//! Desktop UI (egui): landscape layout with firmware, device, flash and log panels.

use crate::image::{AmlImage, human};
use crate::job::{self, Event, FlashOptions, Level};
use crate::settings::{Settings, ThemeChoice};
use crate::theme::{self, Palette, card, chip, tint};
use crate::usb::{self, Kind, UsbDev};
use eframe::egui::{self, Align, Color32, Key, Layout, Margin, RichText, Rounding, Stroke, ThemePreference, Vec2};
use egui_phosphor::regular as ic;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const DEFAULT_PARTS: &[&str] = &["super"];
const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq)]
enum Check {
    Unknown,
    Running,
    Ok,
    Bad,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Task {
    Idle,
    Verify,
    Probe,
    Flash,
}

struct LogLine {
    time: String,
    level: Level,
    msg: String,
}

pub struct App {
    settings: Settings,
    image: Option<Arc<AmlImage>>,
    image_error: Option<String>,
    selected: BTreeSet<String>,
    checks: HashMap<String, Check>,
    crc: Check,
    sha_short: HashMap<String, String>,

    usb_devs: Arc<Mutex<Vec<UsbDev>>>,
    show_all_usb: Arc<AtomicBool>,
    aux_tx: Sender<(Level, String)>,
    aux_rx: Receiver<(Level, String)>,
    adb: Option<PathBuf>,
    adb_busy: Arc<AtomicBool>,
    identified: Option<String>,

    task: Task,
    cancel: Arc<AtomicBool>,
    rx: Option<Receiver<Event>>,
    phase: String,
    done: u64,
    total: u64,
    bytes: bool,
    phase_start: Instant,
    task_start: Instant,
    result: Option<(Task, Result<(), String>, Duration)>,

    log: Vec<LogLine>,
    autoscroll: bool,
    confirm: bool,
}

fn now() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

fn fmt_dur(d: Duration) -> String {
    let s = d.as_secs();
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn notify(title: &str, body: &str, sound: Option<&str>) {
    let script = format!("display notification {:?} with title {:?}", body, title);
    let _ = std::process::Command::new("osascript").args(["-e", &script]).spawn();
    if let Some(s) = sound {
        let _ = std::process::Command::new("afplay").arg(format!("/System/Library/Sounds/{s}.aiff")).spawn();
    }
}

impl App {
    pub fn new(cc: &eframe::CreationContext<'_>, initial: Option<PathBuf>) -> Self {
        theme::install(&cc.egui_ctx);
        let settings = Settings::load();
        cc.egui_ctx.set_theme(match settings.theme {
            ThemeChoice::System => ThemePreference::System,
            ThemeChoice::Light => ThemePreference::Light,
            ThemeChoice::Dark => ThemePreference::Dark,
        });

        let (aux_tx, aux_rx) = channel();
        let usb_devs = Arc::new(Mutex::new(vec![]));
        let show_all_usb = Arc::new(AtomicBool::new(settings.show_all_usb));
        {
            // USB watcher: rescans every 250 ms and reports plug / unplug events.
            let devs = usb_devs.clone();
            let show_all = show_all_usb.clone();
            let tx = aux_tx.clone();
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                let mut prev: Vec<UsbDev> = vec![];
                loop {
                    let cur = usb::scan(show_all.load(Ordering::Relaxed));
                    if cur != prev {
                        for d in cur.iter().filter(|d| !prev.iter().any(|p| p.key() == d.key() && p.kind == d.kind)) {
                            let _ = tx.send((if d.kind == Kind::Adnl { Level::Ok } else { Level::Info }, format!("USB connected: {}", d.describe())));
                        }
                        for d in prev.iter().filter(|p| !cur.iter().any(|d| d.key() == p.key() && p.kind == d.kind)) {
                            let _ = tx.send((Level::Info, format!("USB disconnected: {} [{}]", d.kind.title(), d.id())));
                        }
                        *devs.lock().unwrap() = cur.clone();
                        prev = cur;
                        ctx.request_repaint();
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
            });
        }

        let mut app = Self {
            settings,
            image: None,
            image_error: None,
            selected: BTreeSet::new(),
            checks: HashMap::new(),
            crc: Check::Unknown,
            sha_short: HashMap::new(),
            usb_devs,
            show_all_usb,
            aux_tx,
            aux_rx,
            adb: usb::find_adb(),
            adb_busy: Arc::new(AtomicBool::new(false)),
            identified: None,
            task: Task::Idle,
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
            phase: String::new(),
            done: 0,
            total: 0,
            bytes: false,
            phase_start: Instant::now(),
            task_start: Instant::now(),
            result: None,
            log: vec![],
            autoscroll: true,
            confirm: false,
        };
        app.push_log(Level::Info, format!("Amlogic Flasher {VERSION} · {}", usb::libusb_version()));
        if let Some(p) = initial {
            app.load_image(p);
        }
        app
    }

    fn push_log(&mut self, level: Level, msg: impl Into<String>) {
        self.log.push(LogLine { time: now(), level, msg: msg.into() });
        if self.log.len() > 5000 {
            self.log.drain(..1000);
        }
    }

    fn busy(&self) -> bool {
        self.task != Task::Idle
    }

    fn load_image(&mut self, path: PathBuf) {
        match AmlImage::open(&path) {
            Ok(img) => {
                self.selected = img.partitions().filter(|p| DEFAULT_PARTS.contains(&p.sub_type.as_str())).map(|p| p.sub_type.clone()).collect();
                self.sha_short = img
                    .partitions()
                    .filter_map(|p| img.expected_sha1(&p.sub_type).ok().flatten().map(|s| (p.sub_type.clone(), s[..12].to_string())))
                    .collect();
                self.checks.clear();
                self.crc = Check::Unknown;
                self.push_log(Level::Info, format!("Opened {} (v{}, {} items, {})", path.display(), img.version, img.items.len(), human(img.file_size)));
                self.settings.push_recent(&path.to_string_lossy());
                self.image = Some(Arc::new(img));
                self.image_error = None;
                self.result = None;
            }
            Err(e) => {
                self.push_log(Level::Error, format!("Cannot open {}: {e:#}", path.display()));
                self.image = None;
                self.image_error = Some(format!("{e:#}"));
            }
        }
    }

    fn pick_file(&mut self) {
        let mut dlg = rfd::FileDialog::new().add_filter("Amlogic burn image", &["img"]);
        if let Some(dir) = self.settings.recent.first().and_then(|r| PathBuf::from(r).parent().map(|p| p.to_path_buf())) {
            dlg = dlg.set_directory(dir);
        }
        if let Some(p) = dlg.pick_file() {
            self.load_image(p);
        }
    }

    fn start(&mut self, ctx: &egui::Context, task: Task) {
        let img = self.image.clone();
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.rx = Some(rx);
        self.task = task;
        self.result = None;
        self.phase = "Starting…".into();
        self.done = 0;
        self.total = 0;
        self.bytes = false;
        self.task_start = Instant::now();
        self.phase_start = Instant::now();

        let selected: Vec<String> = self.selected.iter().cloned().collect();
        let all_parts: Vec<String> = img.as_ref().map(|i| i.partitions().map(|p| p.sub_type.clone()).collect()).unwrap_or_default();
        match task {
            Task::Verify => {
                for p in &all_parts {
                    self.checks.insert(p.clone(), Check::Running);
                }
                self.crc = Check::Running;
            }
            Task::Flash if self.settings.precheck => {
                for p in &selected {
                    self.checks.insert(p.clone(), Check::Running);
                }
                self.crc = Check::Running;
            }
            _ => {}
        }
        let opts = FlashOptions {
            partitions: selected,
            precheck: self.settings.precheck,
            reboot: self.settings.reboot,
            wait_for_device: Duration::from_secs(self.settings.wait_minutes as u64 * 60),
            probe_only: task == Task::Probe,
        };
        self.push_log(Level::Info, match task {
            Task::Verify => "Verifying image…".to_string(),
            Task::Probe => "Probing for a box in burning mode…".to_string(),
            Task::Flash => format!("Flashing {}…", opts.partitions.join(", ")),
            Task::Idle => String::new(),
        });
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let txc = tx.clone();
            let ctxc = ctx.clone();
            let sink = move |e: Event| {
                let _ = txc.send(e);
                ctxc.request_repaint();
            };
            let res = match task {
                Task::Verify => job::verify_image(img.as_ref().unwrap(), &all_parts, &cancel, &sink),
                _ => job::run(img.unwrap_or_else(|| Arc::new(AmlImage::empty())), opts, cancel, &sink),
            };
            let _ = tx.send(Event::Done(res.map_err(|e| format!("{e:#}"))));
            ctx.request_repaint();
        });
    }

    fn reboot_to_burning(&mut self) {
        let Some(adb) = self.adb.clone() else { return };
        if self.adb_busy.swap(true, Ordering::SeqCst) {
            return;
        }
        self.push_log(Level::Info, "Running: adb reboot update (the box restarts into USB burning mode)");
        let tx = self.aux_tx.clone();
        let busy = self.adb_busy.clone();
        std::thread::spawn(move || {
            let out = std::process::Command::new(&adb).args(["reboot", "update"]).output();
            let msg = match out {
                Ok(o) if o.status.success() => (Level::Ok, "adb: reboot requested — watch for \"Burning mode\" in the Device panel".to_string()),
                Ok(o) => (Level::Error, format!("adb failed: {}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr)).trim().to_string()),
                Err(e) => (Level::Error, format!("cannot run adb: {e}")),
            };
            let _ = tx.send(msg);
            busy.store(false, Ordering::SeqCst);
        });
    }

    fn pump(&mut self) {
        while let Ok((l, m)) = self.aux_rx.try_recv() {
            self.push_log(l, m);
        }
        let Some(rx) = self.rx.take() else { return };
        let mut finished = false;
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Log(l, m) => self.push_log(l, m),
                Event::Progress { phase, done, total, bytes } => {
                    let key = |s: &str| s.split('…').next().unwrap_or(s).to_string();
                    if key(&phase) != key(&self.phase) {
                        self.phase_start = Instant::now();
                    }
                    self.phase = phase;
                    self.done = done;
                    self.total = total;
                    self.bytes = bytes;
                }
                Event::PartCheck(p, ok) => {
                    self.checks.insert(p, if ok { Check::Ok } else { Check::Bad });
                }
                Event::CrcCheck(ok) => self.crc = if ok { Check::Ok } else { Check::Bad },
                Event::Identified(s) => self.identified = Some(s),
                Event::Done(r) => {
                    let task = self.task;
                    let took = self.task_start.elapsed();
                    for c in self.checks.values_mut() {
                        if *c == Check::Running {
                            *c = Check::Unknown;
                        }
                    }
                    if self.crc == Check::Running {
                        self.crc = Check::Unknown;
                    }
                    match &r {
                        Ok(()) => self.push_log(Level::Ok, format!("{task:?} finished in {}", fmt_dur(took))),
                        Err(e) => self.push_log(Level::Error, e.clone()),
                    }
                    if task == Task::Flash || (task == Task::Verify && r.is_err()) {
                        let (title, body, snd) = match &r {
                            Ok(()) => ("Flash complete", "The box was written and verified.".to_string(), "Glass"),
                            Err(e) => ("Flash failed", e.clone(), "Basso"),
                        };
                        if self.settings.notify {
                            notify(&format!("Amlogic Flasher — {title}"), &body, self.settings.sound.then_some(snd));
                        } else if self.settings.sound {
                            notify_sound(snd);
                        }
                    }
                    self.result = Some((task, r, took));
                    self.task = Task::Idle;
                    finished = true;
                }
            }
        }
        if !finished {
            self.rx = Some(rx);
        }
    }

    // ------------------------------------------------------------------ panels

    fn header(&mut self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(38.0), egui::Sense::hover());
            ui.painter().rect_filled(rect, Rounding::same(10.0), p.accent);
            ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, ic::TELEVISION, egui::FontId::proportional(22.0), p.on_accent);
            ui.add_space(4.0);
            ui.vertical(|ui| {
                ui.add_space(1.0);
                ui.label(RichText::new("Amlogic Flasher").size(19.0).strong());
                ui.label(RichText::new("USB firmware flasher for Amlogic Android TV boxes").size(12.0).color(p.muted));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let ctx = ui.ctx().clone();
                egui::Frame::none().fill(p.card_alt).stroke(Stroke::new(1.0_f32, p.stroke)).rounding(Rounding::same(9.0)).inner_margin(Margin::same(3.0)).show(ui, |ui| {
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = 2.0;
                        for (choice, icon, label) in [(ThemeChoice::System, ic::MONITOR, "Auto"), (ThemeChoice::Light, ic::SUN, "Light"), (ThemeChoice::Dark, ic::MOON, "Dark")] {
                            let on = self.settings.theme == choice;
                            let txt = RichText::new(format!("{icon} {label}")).size(13.0).color(if on { p.on_accent } else { p.text });
                            let b = egui::Button::new(txt).fill(if on { p.accent } else { Color32::TRANSPARENT }).stroke(Stroke::NONE).rounding(Rounding::same(7.0));
                            if ui.add(b).clicked() {
                                self.settings.theme = choice;
                                ctx.set_theme(match choice {
                                    ThemeChoice::System => ThemePreference::System,
                                    ThemeChoice::Light => ThemePreference::Light,
                                    ThemeChoice::Dark => ThemePreference::Dark,
                                });
                            }
                        }
                    });
                });
            });
        });
    }

    fn firmware(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let has_image = self.image.is_some();
        let busy = self.busy();
        let mut verify_clicked = false;
        let mut reveal_clicked = false;
        card(
            ui,
            p,
            ic::HARD_DRIVES,
            "Firmware image",
            |ui| {
                if has_image {
                    if ui.add_enabled(!busy, egui::Button::new(format!("{} Verify image", ic::SHIELD_CHECK))).on_hover_text("Check header CRC and every partition's SHA-1 on this Mac").clicked() {
                        verify_clicked = true;
                    }
                    if ui.button(format!("{} Show in Finder", ic::ARROW_SQUARE_OUT)).clicked() {
                        reveal_clicked = true;
                    }
                }
            },
            |ui| {
                ui.set_min_height(ui.available_height());
                match self.image.clone() {
                    None => self.drop_zone(ui, p),
                    Some(img) => self.image_view(ui, p, &img),
                }
            },
        );
        if verify_clicked {
            self.start(ui.ctx(), Task::Verify);
        }
        if reveal_clicked {
            if let Some(img) = &self.image {
                let _ = std::process::Command::new("open").arg("-R").arg(&img.path).spawn();
            }
        }
    }

    fn drop_zone(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        egui::Frame::none()
            .fill(if hovering { p.accent_soft } else { p.card_alt })
            .stroke(Stroke::new(if hovering { 2.0_f32 } else { 1.0 }, if hovering { p.accent } else { p.stroke }))
            .rounding(Rounding::same(12.0))
            .inner_margin(Margin::symmetric(20.0, 36.0))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                ui.vertical_centered(|ui| {
                    ui.label(RichText::new(ic::FILE).size(44.0).color(p.accent));
                    ui.add_space(6.0);
                    ui.label(RichText::new("Drop an aml_upgrade_package .img here").size(16.0).strong());
                    ui.label(RichText::new("Amlogic USB Burning Tool images (v1 / v2)").color(p.muted));
                    ui.add_space(10.0);
                    let b = egui::Button::new(RichText::new(format!("{} Choose file…   ⌘O", ic::FOLDER_OPEN)).color(p.on_accent)).fill(p.accent).min_size(Vec2::new(180.0, 34.0));
                    if ui.add_enabled(!self.busy(), b).clicked() {
                        self.pick_file();
                    }
                });
            });
        if let Some(e) = &self.image_error {
            ui.add_space(8.0);
            ui.colored_label(p.err, format!("{} {e}", ic::WARNING));
        }
        if !self.settings.recent.is_empty() {
            ui.add_space(14.0);
            ui.label(RichText::new(format!("{} Recent", ic::CLOCK_COUNTER_CLOCKWISE)).color(p.muted));
            let recent = self.settings.recent.clone();
            for r in recent {
                let path = PathBuf::from(&r);
                let exists = path.is_file();
                let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(r.clone());
                ui.horizontal(|ui| {
                    let resp = ui.add_enabled(exists && !self.busy(), egui::Link::new(RichText::new(format!("{} {name}", ic::FILE)))).on_hover_text(&r);
                    if resp.clicked() {
                        self.load_image(path.clone());
                    }
                    ui.label(RichText::new(path.parent().map(|d| d.display().to_string()).unwrap_or_default()).size(11.5).color(p.muted));
                    if !exists {
                        ui.label(RichText::new("(missing)").size(11.5).color(p.warn));
                    }
                });
            }
        }
    }

    fn image_view(&mut self, ui: &mut egui::Ui, p: &Palette, img: &Arc<AmlImage>) {
        // file row
        ui.horizontal(|ui| {
            ui.label(RichText::new(ic::FILE).size(26.0).color(p.accent));
            ui.vertical(|ui| {
                ui.label(RichText::new(img.path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()).size(15.0).strong());
                ui.label(RichText::new(img.path.parent().map(|d| d.display().to_string()).unwrap_or_default()).size(11.5).color(p.muted));
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let busy = self.busy();
                ui.add_enabled_ui(!busy, |ui| {
                    ui.menu_button(format!("{} Recent", ic::CLOCK_COUNTER_CLOCKWISE), |ui| {
                        let recent = self.settings.recent.clone();
                        for r in recent {
                            let path = PathBuf::from(&r);
                            if ui.add_enabled(path.is_file(), egui::Button::new(path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or(r.clone()))).on_hover_text(&r).clicked() {
                                self.load_image(path);
                                ui.close_menu();
                            }
                        }
                    });
                    if ui.button(format!("{} Open…", ic::FOLDER_OPEN)).clicked() {
                        self.pick_file();
                    }
                });
            });
        });
        ui.add_space(6.0);

        // chips
        ui.horizontal_wrapped(|ui| {
            chip(ui, format!("Image v{}", img.version), p.muted, p.card_alt);
            chip(ui, format!("{} items", img.items.len()), p.muted, p.card_alt);
            chip(ui, format!("{} partitions", img.partitions().count()), p.muted, p.card_alt);
            chip(ui, human(img.file_size), p.muted, p.card_alt);
            match self.crc {
                Check::Ok => chip(ui, format!("{} CRC OK", ic::CHECK_CIRCLE), p.ok, tint(p.ok, 28)),
                Check::Bad => chip(ui, format!("{} CRC mismatch", ic::X_CIRCLE), p.err, tint(p.err, 28)),
                Check::Running => chip(ui, format!("{} checking CRC…", ic::CIRCLE_NOTCH), p.accent, p.accent_soft),
                Check::Unknown => chip(ui, "CRC not checked", p.muted, p.card_alt),
            }
        });
        ui.add_space(8.0);

        // quick select + summary
        let busy = self.busy();
        ui.horizontal(|ui| {
            ui.label(RichText::new("Select").color(p.muted));
            ui.add_enabled_ui(!busy, |ui| {
                if ui.small_button("super only").clicked() {
                    self.selected = img.partition("super").map(|i| i.sub_type.clone()).into_iter().collect();
                }
                if ui.small_button("all writable").clicked() {
                    self.selected = img.partitions().filter(|i| !i.is_protected()).map(|i| i.sub_type.clone()).collect();
                }
                if ui.small_button("none").clicked() {
                    self.selected.clear();
                }
            });
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let total: u64 = self.selected.iter().filter_map(|n| img.partition(n)).map(|i| i.size).sum();
                if self.selected.is_empty() {
                    ui.label(RichText::new("Nothing selected").color(p.warn));
                } else {
                    ui.label(RichText::new(format!("{} selected · {}", self.selected.len(), human(total))).strong().color(p.accent));
                }
            });
        });
        ui.add_space(4.0);

        // table
        egui::Frame::none().fill(p.card_alt).rounding(Rounding::same(9.0)).inner_margin(Margin::symmetric(10.0, 8.0)).show(ui, |ui| {
            ui.set_width(ui.available_width());
            egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                egui::Grid::new("parts").num_columns(6).spacing([18.0, 9.0]).min_col_width(20.0).show(ui, |ui| {
                    for h in ["", "Partition", "Size", "Format", "SHA-1 (image)", "Check"] {
                        ui.label(RichText::new(h).size(12.0).color(p.muted));
                    }
                    ui.end_row();
                    for item in img.partitions() {
                        let name = item.sub_type.clone();
                        let mut on = self.selected.contains(&name);
                        let resp = ui.add_enabled(!busy && !item.is_protected(), egui::Checkbox::without_text(&mut on));
                        if item.is_protected() {
                            resp.on_disabled_hover_text("Boot loader partitions are never written by this app (needs a full ROM-stage burn).");
                        } else if resp.changed() {
                            if on {
                                self.selected.insert(name.clone());
                            } else {
                                self.selected.remove(&name);
                            }
                        }
                        if item.is_protected() {
                            ui.label(RichText::new(format!("{} {name}", ic::LOCK)).color(p.muted));
                        } else if on {
                            ui.label(RichText::new(&name).strong());
                        } else {
                            ui.label(&name);
                        }
                        ui.label(RichText::new(human(item.size)).color(p.muted));
                        if item.is_sparse() {
                            chip(ui, "sparse", p.accent, p.accent_soft);
                        } else {
                            ui.label(RichText::new("normal").color(p.muted));
                        }
                        ui.label(RichText::new(self.sha_short.get(&name).cloned().unwrap_or_else(|| "—".into())).monospace().color(p.muted));
                        match self.checks.get(&name).copied().unwrap_or(Check::Unknown) {
                            Check::Ok => ui.label(RichText::new(ic::CHECK_CIRCLE).size(17.0).color(p.ok)).on_hover_text("SHA-1 matches"),
                            Check::Bad => ui.label(RichText::new(ic::X_CIRCLE).size(17.0).color(p.err)).on_hover_text("SHA-1 mismatch: image is corrupt"),
                            Check::Running => ui.add(egui::Spinner::new().size(15.0)),
                            Check::Unknown => ui.label(RichText::new("·").color(p.muted)),
                        };
                        ui.end_row();
                    }
                });
            });
        });
    }

    fn device(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let devs = self.usb_devs.lock().unwrap().clone();
        let mut show_all = self.settings.show_all_usb;
        let busy = self.busy();
        let mut probe = false;
        let mut adb_reboot = false;
        card(
            ui,
            p,
            ic::USB,
            "Device",
            |ui| {
                ui.checkbox(&mut show_all, RichText::new("all USB").size(12.0)).on_hover_text("Also list non-Amlogic USB devices");
            },
            |ui| {
                let best = devs.first().cloned();
                let (color, icon, title, hint) = match &best {
                    Some(d) => {
                        let c = match d.kind {
                            Kind::Adnl => p.ok,
                            Kind::Fastboot | Kind::Adb => p.accent,
                            Kind::Optimus => p.warn,
                            Kind::Other => p.muted,
                        };
                        let i = match d.kind {
                            Kind::Adnl => ic::LIGHTNING,
                            Kind::Fastboot => ic::TERMINAL_WINDOW,
                            Kind::Adb => ic::TELEVISION,
                            Kind::Optimus => ic::WARNING,
                            Kind::Other => ic::PLUG,
                        };
                        (c, i, d.kind.title().to_string(), d.kind.hint().to_string())
                    }
                    None => (p.muted, ic::PLUG, "No box detected".to_string(), "Connect the box and power-cycle it to enter burning mode.".to_string()),
                };
                egui::Frame::none().fill(tint(color, 22)).stroke(Stroke::new(1.0_f32, tint(color, 90))).rounding(Rounding::same(10.0)).inner_margin(Margin::same(12.0)).show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.horizontal(|ui| {
                        let (rect, _) = ui.allocate_exact_size(Vec2::splat(44.0), egui::Sense::hover());
                        ui.painter().circle_filled(rect.center(), 22.0, tint(color, 40));
                        let pulse = if best.as_ref().is_some_and(|d| d.kind == Kind::Adnl) { 1.0 } else { 0.0 };
                        if pulse > 0.0 {
                            let t = ui.input(|i| i.time) as f32;
                            let r = 16.0 + 6.0 * (t * 3.0).sin().abs();
                            ui.painter().circle_stroke(rect.center(), r, Stroke::new(1.5_f32, tint(color, 120)));
                            ui.ctx().request_repaint_after(Duration::from_millis(50));
                        }
                        ui.painter().text(rect.center(), egui::Align2::CENTER_CENTER, icon, egui::FontId::proportional(22.0), color);
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&title).size(15.0).strong().color(color));
                            ui.label(RichText::new(&hint).size(12.5).color(p.muted));
                        });
                    });
                });

                if let Some(d) = &best {
                    ui.add_space(6.0);
                    egui::Grid::new("devinfo").num_columns(2).spacing([14.0, 4.0]).show(ui, |ui| {
                        ui.label(RichText::new("USB ID").color(p.muted));
                        ui.label(RichText::new(d.id()).monospace());
                        ui.end_row();
                        ui.label(RichText::new("Location").color(p.muted));
                        ui.label(format!("bus {} · port {} · addr {}", d.bus, d.port_path, d.addr));
                        ui.end_row();
                        ui.label(RichText::new("Link").color(p.muted));
                        ui.label(d.speed);
                        ui.end_row();
                        if let Some(id) = &self.identified {
                            ui.label(RichText::new("Identify").color(p.muted));
                            ui.label(id);
                            ui.end_row();
                        }
                    });
                    if devs.len() > 1 {
                        ui.add_space(4.0);
                        for d in devs.iter().skip(1) {
                            ui.label(RichText::new(format!("{} {}", ic::PLUG, d.describe())).size(11.5).color(p.muted));
                        }
                    }
                } else {
                    ui.add_space(6.0);
                    for (n, s) in [
                        "USB-A ↔ USB-C cable from the box's USB 2.0 port to this Mac.",
                        "Unplug and re-plug the box's power (no reset button).",
                        "It shows up here as \"Burning mode\" while booting.",
                    ]
                    .iter()
                    .enumerate()
                    {
                        ui.horizontal_top(|ui| {
                            chip(ui, (n + 1).to_string(), p.accent, p.accent_soft);
                            ui.add(egui::Label::new(RichText::new(*s).size(12.5)).wrap());
                        });
                    }
                }

                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add_enabled(!busy, egui::Button::new(format!("{} Probe", ic::MAGNIFYING_GLASS))).on_hover_text("Wait for the box in burning mode and identify it — writes nothing").clicked() {
                        probe = true;
                    }
                    let adb_dev = devs.iter().any(|d| d.kind == Kind::Adb);
                    let adb_ok = self.adb.is_some();
                    let b = ui
                        .add_enabled(!busy && adb_dev && adb_ok && !self.adb_busy.load(Ordering::SeqCst), egui::Button::new(format!("{} Reboot to burning mode", ic::POWER)))
                        .on_hover_text("Runs `adb reboot update`. Needs USB debugging enabled and authorised on the box.");
                    if !adb_ok {
                        b.on_disabled_hover_text("adb not found (install Android platform-tools: brew install android-platform-tools)");
                    } else if b.clicked() {
                        adb_reboot = true;
                    }
                });
            },
        );
        if show_all != self.settings.show_all_usb {
            self.settings.show_all_usb = show_all;
            self.show_all_usb.store(show_all, Ordering::Relaxed);
        }
        if probe {
            self.start(ui.ctx(), Task::Probe);
        }
        if adb_reboot {
            self.reboot_to_burning();
        }
    }

    fn flash(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let busy = self.busy();
        let mut start_flash = false;
        card(ui, p, ic::LIGHTNING, "Flash", |_| {}, |ui| {
            let w = ui.available_width();
            if busy {
                let stoppable = self.phase.starts_with("Waiting") || self.phase.starts_with("Checking") || self.phase.starts_with("Starting");
                let label = if stoppable { format!("{} Stop", ic::STOP) } else { format!("{} Writing — do not unplug", ic::LOCK) };
                let b = egui::Button::new(RichText::new(label).size(15.0).color(if stoppable { p.err } else { p.muted })).fill(p.card_alt).stroke(Stroke::new(1.0_f32, if stoppable { p.err } else { p.stroke })).min_size(Vec2::new(w, 44.0));
                if ui.add_enabled(stoppable, b).clicked() {
                    self.cancel.store(true, Ordering::SeqCst);
                }
            } else {
                let n = self.selected.len();
                let ready = self.image.is_some() && n > 0;
                let label = if self.image.is_none() { "Open an image first".to_string() } else if n == 0 { "Select a partition".to_string() } else { format!("{}  Flash {n} partition{}", ic::LIGHTNING, if n == 1 { "" } else { "s" }) };
                let b = egui::Button::new(RichText::new(label).size(16.0).strong().color(if ready { p.on_accent } else { p.muted }))
                    .fill(if ready { p.accent } else { p.card_alt })
                    .min_size(Vec2::new(w, 46.0))
                    .rounding(Rounding::same(10.0));
                if ui.add_enabled(ready, b).clicked() {
                    start_flash = true;
                }
            }

            if busy || self.result.is_some() {
                ui.add_space(10.0);
                if busy {
                    ui.horizontal(|ui| {
                        ui.add(egui::Spinner::new().size(14.0));
                        ui.label(RichText::new(&self.phase).strong());
                    });
                    let frac = if self.total > 0 { Some(self.done as f32 / self.total as f32) } else { None };
                    let bar = match frac {
                        Some(f) => egui::ProgressBar::new(f).show_percentage().fill(p.accent),
                        None => egui::ProgressBar::new(0.0).animate(true).fill(p.accent),
                    };
                    ui.add(bar.desired_height(10.0).rounding(Rounding::same(5.0)));
                    let mut parts = vec![format!("elapsed {}", fmt_dur(self.task_start.elapsed()))];
                    if self.bytes && self.total > 0 {
                        let secs = self.phase_start.elapsed().as_secs_f64().max(0.001);
                        let rate = self.done as f64 / secs;
                        parts.insert(0, format!("{} / {}", human(self.done), human(self.total)));
                        if rate > 0.0 && self.done > 0 {
                            parts.insert(1, format!("{}/s", human(rate as u64)));
                            let eta = (self.total - self.done) as f64 / rate;
                            parts.insert(2, format!("ETA {}", fmt_dur(Duration::from_secs_f64(eta))));
                        }
                    }
                    ui.label(RichText::new(parts.join("  ·  ")).size(12.0).color(p.muted));
                    ui.ctx().request_repaint_after(Duration::from_millis(250));
                }
                if let Some((task, r, took)) = &self.result {
                    let (c, icon, text) = match r {
                        Ok(()) => (p.ok, ic::CHECK_CIRCLE, match task {
                            Task::Flash => format!("Flashed and verified in {}", fmt_dur(*took)),
                            Task::Verify => "Image verified — all checksums match".to_string(),
                            Task::Probe => "Box found and ready".to_string(),
                            Task::Idle => String::new(),
                        }),
                        Err(e) => (p.err, ic::X_CIRCLE, e.clone()),
                    };
                    egui::Frame::none().fill(tint(c, 26)).stroke(Stroke::new(1.0_f32, tint(c, 110))).rounding(Rounding::same(9.0)).inner_margin(Margin::same(10.0)).show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.horizontal_wrapped(|ui| {
                            ui.label(RichText::new(icon).size(18.0).color(c));
                            ui.label(RichText::new(text).color(c));
                        });
                    });
                }
            }

            ui.add_space(12.0);
            ui.separator();
            ui.add_space(4.0);
            ui.add_enabled_ui(!busy, |ui| {
                ui.checkbox(&mut self.settings.precheck, "Verify image on this Mac first (CRC + SHA-1)");
                ui.checkbox(&mut self.settings.reboot, "Reboot the box when done");
                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.settings.notify, format!("{} Notify", ic::BELL));
                    ui.checkbox(&mut self.settings.sound, format!("{} Sound", ic::SPEAKER_HIGH));
                });
                ui.horizontal(|ui| {
                    ui.label(format!("{} Wait for box", ic::CLOCK));
                    ui.add(egui::Slider::new(&mut self.settings.wait_minutes, 1..=30).suffix(" min"));
                });
            });
            ui.add(egui::Label::new(RichText::new(format!("{} Nothing is erased — only the selected partitions are written, then verified by the box.", ic::INFO)).size(12.0).color(p.muted)).wrap());
        });
        if start_flash {
            self.confirm = true;
        }
    }

    fn log_panel(&mut self, ui: &mut egui::Ui, p: &Palette) {
        let mut clear = false;
        ui.horizontal(|ui| {
            ui.label(RichText::new(ic::TERMINAL_WINDOW).color(p.accent));
            ui.label(RichText::new("Activity").strong());
            ui.label(RichText::new(format!("{} lines", self.log.len())).size(11.5).color(p.muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                if ui.small_button(format!("{} Clear", ic::BROOM)).clicked() {
                    clear = true;
                }
                if ui.small_button(format!("{} Save…", ic::FLOPPY_DISK)).clicked() {
                    if let Some(path) = rfd::FileDialog::new().set_file_name(format!("amlogic-flasher-{}.log", chrono::Local::now().format("%Y%m%d-%H%M%S"))).save_file() {
                        let _ = std::fs::write(&path, self.log_text());
                        self.push_log(Level::Info, format!("Log saved to {}", path.display()));
                    }
                }
                if ui.small_button(format!("{} Copy", ic::COPY)).clicked() {
                    ui.ctx().copy_text(self.log_text());
                }
                ui.checkbox(&mut self.autoscroll, RichText::new("follow").size(12.0));
            });
        });
        if clear {
            self.log.clear();
        }
        egui::Frame::none().fill(p.card).stroke(Stroke::new(1.0_f32, p.stroke)).rounding(Rounding::same(10.0)).inner_margin(Margin::symmetric(12.0, 8.0)).show(ui, |ui| {
            egui::ScrollArea::vertical().auto_shrink([false, false]).stick_to_bottom(self.autoscroll).show(ui, |ui| {
                for l in &self.log {
                    let (icon, c) = match l.level {
                        Level::Info => (ic::INFO, p.muted),
                        Level::Ok => (ic::CHECK_CIRCLE, p.ok),
                        Level::Warn => (ic::WARNING, p.warn),
                        Level::Error => (ic::X_CIRCLE, p.err),
                    };
                    ui.horizontal(|ui| {
                        ui.label(RichText::new(&l.time).monospace().size(11.5).color(p.muted));
                        ui.label(RichText::new(icon).size(13.0).color(c));
                        let t = RichText::new(&l.msg).monospace().size(12.0);
                        ui.label(if l.level == Level::Info { t } else { t.color(c) });
                    });
                }
            });
        });
    }

    fn log_text(&self) -> String {
        self.log
            .iter()
            .map(|l| format!("{} [{}] {}", l.time, match l.level { Level::Info => "info", Level::Ok => " ok ", Level::Warn => "warn", Level::Error => "FAIL" }, l.msg))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn status_bar(&self, ui: &mut egui::Ui, p: &Palette) {
        ui.horizontal(|ui| {
            let devs = self.usb_devs.lock().unwrap();
            let (c, t) = match devs.first() {
                Some(d) if d.kind == Kind::Adnl => (p.ok, "Box in burning mode".to_string()),
                Some(d) => (p.accent, d.kind.title().to_string()),
                None => (p.muted, "No box".to_string()),
            };
            let (rect, _) = ui.allocate_exact_size(Vec2::splat(10.0), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), 4.5, c);
            ui.label(RichText::new(t).size(12.0).color(p.muted));
            ui.separator();
            ui.label(RichText::new(match self.task {
                Task::Idle => "Idle".to_string(),
                t => format!("{t:?}…"),
            }).size(12.0).color(p.muted));
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                ui.label(RichText::new(format!("v{VERSION}")).size(12.0).color(p.muted));
                ui.separator();
                ui.label(RichText::new(usb::libusb_version()).size(12.0).color(p.muted));
                ui.separator();
                ui.label(RichText::new(if self.adb.is_some() { "adb available" } else { "adb not found" }).size(12.0).color(p.muted));
            });
        });
    }

    fn confirm_dialog(&mut self, ctx: &egui::Context, p: &Palette) {
        if !self.confirm {
            return;
        }
        let mut go = false;
        let mut close = false;
        egui::Area::new(egui::Id::new("dim")).order(egui::Order::Middle).fixed_pos(egui::Pos2::ZERO).show(ctx, |ui| {
            let r = ctx.screen_rect();
            ui.painter().rect_filled(r, Rounding::ZERO, Color32::from_black_alpha(110));
            ui.allocate_rect(r, egui::Sense::click());
        });
        egui::Window::new("confirm")
            .title_bar(false)
            .order(egui::Order::Foreground)
            .collapsible(false)
            .resizable(false)
            .fixed_size([420.0, 0.0])
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(ic::WARNING).size(26.0).color(p.warn));
                    ui.label(RichText::new("Overwrite partitions on the box?").size(17.0).strong());
                });
                ui.add_space(6.0);
                if let Some(img) = &self.image {
                    for n in &self.selected {
                        let it = img.partition(n);
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(ic::HARD_DRIVES).color(p.accent));
                            ui.label(RichText::new(n).strong());
                            ui.label(RichText::new(it.map(|i| format!("{} · {}", human(i.size), i.format())).unwrap_or_default()).color(p.muted));
                        });
                    }
                    ui.add_space(4.0);
                    ui.label(RichText::new(format!("from {}", img.path.file_name().unwrap_or_default().to_string_lossy())).color(p.muted));
                }
                ui.add_space(8.0);
                ui.label(RichText::new("The image must be built for this exact box model. Keep the box powered and the cable connected until it finishes.").size(12.5));
                ui.add_space(12.0);
                ui.horizontal(|ui| {
                    let b = egui::Button::new(RichText::new(format!("{} Flash now", ic::LIGHTNING)).strong().color(p.on_accent)).fill(p.accent).min_size(Vec2::new(130.0, 34.0));
                    if ui.add(b).clicked() || ui.input(|i| i.key_pressed(Key::Enter)) {
                        go = true;
                    }
                    if ui.add(egui::Button::new("Cancel").min_size(Vec2::new(90.0, 34.0))).clicked() || ui.input(|i| i.key_pressed(Key::Escape)) {
                        close = true;
                    }
                });
            });
        if go {
            self.confirm = false;
            self.start(ctx, Task::Flash);
        } else if close {
            self.confirm = false;
        }
    }
}

fn notify_sound(s: &str) {
    let _ = std::process::Command::new("afplay").arg(format!("/System/Library/Sounds/{s}.aiff")).spawn();
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        let before = self.settings.clone();
        self.pump();
        let p = theme::palette(ctx);

        // file drop + shortcuts
        let dropped: Option<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().find_map(|f| f.path.clone()));
        if let Some(path) = dropped {
            if !self.busy() {
                self.load_image(path);
            }
        }
        if ctx.input(|i| i.modifiers.command && i.key_pressed(Key::O)) && !self.busy() {
            self.pick_file();
        }

        egui::TopBottomPanel::top("header")
            .frame(egui::Frame::none().fill(p.card).inner_margin(Margin::symmetric(20.0, 12.0)).stroke(Stroke::new(1.0_f32, p.stroke)))
            .show(ctx, |ui| self.header(ui, &p));
        egui::TopBottomPanel::bottom("status")
            .frame(egui::Frame::none().fill(p.card).inner_margin(Margin::symmetric(20.0, 5.0)).stroke(Stroke::new(1.0_f32, p.stroke)))
            .show(ctx, |ui| self.status_bar(ui, &p));
        egui::TopBottomPanel::bottom("log")
            .resizable(true)
            .default_height(140.0)
            .min_height(90.0)
            .max_height(480.0)
            .frame(egui::Frame::none().fill(p.bg).inner_margin(Margin { left: 20.0, right: 20.0, top: 8.0, bottom: 10.0 }))
            .show(ctx, |ui| self.log_panel(ui, &p));
        egui::SidePanel::right("right")
            .resizable(false)
            .exact_width(420.0)
            .frame(egui::Frame::none().fill(p.bg).inner_margin(Margin { left: 6.0, right: 20.0, top: 16.0, bottom: 8.0 }))
            .show(ctx, |ui| {
                egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
                    self.device(ui, &p);
                    ui.add_space(12.0);
                    self.flash(ui, &p);
                });
            });
        egui::CentralPanel::default()
            .frame(egui::Frame::none().fill(p.bg).inner_margin(Margin { left: 20.0, right: 10.0, top: 16.0, bottom: 8.0 }))
            .show(ctx, |ui| self.firmware(ui, &p));

        self.confirm_dialog(ctx, &p);

        if self.settings != before {
            self.settings.save();
        }
    }
}
