mod adnl;
mod image;
mod job;

use eframe::egui::{self, Color32, RichText};
use image::{AmlImage, human};
use job::{Event, FlashOptions};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_PARTS: &[&str] = &["super"];

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(cmd) = args.first().map(String::as_str) {
        if matches!(cmd, "info" | "probe" | "flash" | "help" | "--help" | "-h") {
            std::process::exit(cli(&args));
        }
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Amlogic Flash")
            .with_inner_size([900.0, 720.0])
            .with_min_inner_size([640.0, 520.0]),
        ..Default::default()
    };
    let initial = args.iter().map(PathBuf::from).find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("img")) && p.is_file());
    if let Err(e) = eframe::run_native(
        "Amlogic Flash",
        options,
        Box::new(move |cc| {
            let mut app = App::new(cc);
            if let Some(p) = initial {
                app.load_image(p);
            }
            Ok(Box::new(app))
        }),
    ) {
        eprintln!("failed to start GUI: {e}");
        std::process::exit(1);
    }
}

// ---------------------------------------------------------------- CLI

fn cli(args: &[String]) -> i32 {
    let usage = "usage:
  amlogic-flash                         launch the GUI
  amlogic-flash info  <image.img>       list items and validate CRC / SHA-1
  amlogic-flash probe                   wait for a box in USB burning mode and identify it
  amlogic-flash flash <image.img> [--part NAME]... [--no-reboot] [--skip-check]
                                        write partitions (default: super) over ADNL";
    let last = Mutex::new((String::new(), -1i32));
    let sink: Box<job::Sink> = Box::new(move |e| {
        let mut last = last.lock().unwrap();
        match e {
            Event::Log(m) => {
                if last.1 >= 0 {
                    eprintln!();
                }
                *last = (String::new(), -1);
                eprintln!("{m}");
            }
            Event::Progress(l, f) => {
                // Only redraw when the label's prefix or the whole-percent value changes.
                let pct = f.map(|f| (f * 100.0) as i32).unwrap_or(0);
                let key: String = l.split(" — ").next().unwrap_or(&l).chars().filter(|c| !c.is_ascii_digit()).collect();
                if key != last.0 || pct != last.1 {
                    match f {
                        Some(_) => eprint!("\r\x1b[K{l}: {pct}%"),
                        None => eprint!("\r\x1b[K{l}"),
                    }
                    *last = (key, pct);
                }
            }
            Event::Done(_) => {}
        }
    });
    let res = match args[0].as_str() {
        "info" if args.len() == 2 => info(&PathBuf::from(&args[1])),
        "probe" => job::run(
            Arc::new(dummy_image()),
            FlashOptions { partitions: vec![], precheck: false, reboot: false, wait_for_device: Duration::from_secs(300), probe_only: true },
            Arc::new(AtomicBool::new(false)),
            &*sink,
        ),
        "flash" if args.len() >= 2 => (|| {
            let img = Arc::new(AmlImage::open(&PathBuf::from(&args[1]))?);
            let mut parts = vec![];
            let mut reboot = true;
            let mut precheck = true;
            let mut it = args[2..].iter();
            while let Some(a) = it.next() {
                match a.as_str() {
                    "--part" => parts.push(it.next().ok_or_else(|| anyhow::anyhow!("--part needs a name"))?.clone()),
                    "--no-reboot" => reboot = false,
                    "--skip-check" => precheck = false,
                    o => anyhow::bail!("unknown option {o}"),
                }
            }
            if parts.is_empty() {
                parts = DEFAULT_PARTS.iter().map(|s| s.to_string()).collect();
            }
            let opts = FlashOptions { partitions: parts, precheck, reboot, wait_for_device: Duration::from_secs(600), probe_only: false };
            job::run(img, opts, Arc::new(AtomicBool::new(false)), &*sink)
        })(),
        _ => {
            println!("{usage}");
            return 2;
        }
    };
    eprintln!();
    match res {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("error: {e:#}");
            1
        }
    }
}

fn dummy_image() -> AmlImage {
    AmlImage { path: PathBuf::new(), version: 0, stored_crc: 0, declared_size: 0, file_size: 0, items: vec![] }
}

fn info(path: &PathBuf) -> anyhow::Result<()> {
    let img = AmlImage::open(path)?;
    println!("{}: Amlogic image v{}, {} items, {}", path.display(), img.version, img.items.len(), human(img.file_size));
    eprint!("checking CRC… ");
    println!("header CRC: {}", if img.check_crc(|_, _| {})? { "OK" } else { "MISMATCH" });
    println!("{:<10} {:<22} {:>12} {:<7} {}", "TYPE", "NAME", "SIZE", "FORMAT", "SHA-1");
    for it in &img.items {
        let sha = if it.is_partition() {
            match img.expected_sha1(&it.sub_type)? {
                Some(want) => {
                    let got = img.sha1_of(it, |_, _| {})?;
                    if got == want { "OK".to_string() } else { format!("MISMATCH (image says {want}, data is {got})") }
                }
                None => "-".into(),
            }
        } else {
            String::new()
        };
        if it.main_type != "VERIFY" {
            println!("{:<10} {:<22} {:>12} {:<7} {}", it.main_type, it.sub_type, human(it.size), it.format(), sha);
        }
    }
    Ok(())
}

// ---------------------------------------------------------------- GUI

#[derive(Default)]
struct RunState {
    progress_label: String,
    progress: Option<f32>,
    log: Vec<String>,
    result: Option<Result<(), String>>,
}

struct App {
    image: Option<Arc<AmlImage>>,
    image_error: Option<String>,
    selected: BTreeSet<String>,
    precheck: bool,
    reboot: bool,
    device_present: Arc<AtomicBool>,
    running: bool,
    cancel: Arc<AtomicBool>,
    rx: Option<Receiver<Event>>,
    state: RunState,
    confirm: Option<bool>, // Some(probe_only)
    sha_cache: Arc<Mutex<Vec<(String, Option<String>)>>>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> Self {
        let present = Arc::new(AtomicBool::new(false));
        {
            let present = present.clone();
            let ctx = cc.egui_ctx.clone();
            std::thread::spawn(move || {
                loop {
                    let now = adnl::device_present();
                    if present.swap(now, Ordering::SeqCst) != now {
                        ctx.request_repaint();
                    }
                    std::thread::sleep(Duration::from_millis(400));
                }
            });
        }
        Self {
            image: None,
            image_error: None,
            selected: BTreeSet::new(),
            precheck: true,
            reboot: true,
            device_present: present,
            running: false,
            cancel: Arc::new(AtomicBool::new(false)),
            rx: None,
            state: RunState::default(),
            confirm: None,
            sha_cache: Arc::new(Mutex::new(vec![])),
        }
    }

    fn load_image(&mut self, path: PathBuf) {
        match AmlImage::open(&path) {
            Ok(img) => {
                self.selected = img
                    .partitions()
                    .filter(|p| DEFAULT_PARTS.contains(&p.sub_type.as_str()))
                    .map(|p| p.sub_type.clone())
                    .collect();
                let cache: Vec<_> = img.partitions().map(|p| (p.sub_type.clone(), img.expected_sha1(&p.sub_type).ok().flatten())).collect();
                *self.sha_cache.lock().unwrap() = cache;
                self.image = Some(Arc::new(img));
                self.image_error = None;
                self.state = RunState::default();
            }
            Err(e) => {
                self.image = None;
                self.image_error = Some(format!("{e:#}"));
            }
        }
    }

    fn start(&mut self, ctx: &egui::Context, probe_only: bool) {
        let img = self.image.clone().unwrap_or_else(|| Arc::new(dummy_image()));
        let opts = FlashOptions {
            partitions: self.selected.iter().cloned().collect(),
            precheck: self.precheck,
            reboot: self.reboot,
            wait_for_device: Duration::from_secs(600),
            probe_only,
        };
        let (tx, rx) = channel();
        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = cancel.clone();
        self.rx = Some(rx);
        self.running = true;
        self.state = RunState::default();
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let txc = tx.clone();
            let ctxc = ctx.clone();
            let sink = move |e: Event| {
                let _ = txc.send(e);
                ctxc.request_repaint();
            };
            let res = job::run(img, opts, cancel, &sink).map_err(|e| format!("{e:#}"));
            let _ = tx.send(Event::Done(res));
            ctx.request_repaint();
        });
    }

    fn pump(&mut self) {
        let Some(rx) = &self.rx else { return };
        while let Ok(ev) = rx.try_recv() {
            match ev {
                Event::Log(m) => self.state.log.push(m),
                Event::Progress(l, f) => {
                    self.state.progress_label = l;
                    self.state.progress = f;
                }
                Event::Done(r) => {
                    match &r {
                        Ok(()) => self.state.log.push("✔ Finished successfully".into()),
                        Err(e) => self.state.log.push(format!("✖ {e}")),
                    }
                    self.state.result = Some(r);
                    self.running = false;
                }
            }
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if let Some(p) = dropped.into_iter().next() {
            if !self.running {
                self.load_image(p);
            }
        }

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("Amlogic Flash");
                ui.label(RichText::new("USB (ADNL) flasher for Amlogic Android TV boxes").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    let on = self.device_present.load(Ordering::SeqCst);
                    let (c, t) = if on {
                        (Color32::from_rgb(40, 170, 80), "Box connected (burning mode)")
                    } else {
                        (Color32::GRAY, "No box in burning mode")
                    };
                    ui.label(RichText::new(t).color(c));
                    let (rect, _) = ui.allocate_exact_size(egui::vec2(14.0, 14.0), egui::Sense::hover());
                    ui.painter().circle_filled(rect.center(), 6.0, c);
                });
            });
            ui.add_space(6.0);
        });

        egui::CentralPanel::default().show(ctx, |ui| {
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("1. Firmware image").strong());
                ui.horizontal(|ui| {
                    if ui.add_enabled(!self.running, egui::Button::new("Choose .img…")).clicked() {
                        if let Some(p) = rfd::FileDialog::new().add_filter("Amlogic burn image", &["img"]).pick_file() {
                            self.load_image(p);
                        }
                    }
                    match (&self.image, &self.image_error) {
                        (Some(img), _) => {
                            ui.label(format!(
                                "{}  ·  v{}  ·  {} items  ·  {}",
                                img.path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default(),
                                img.version,
                                img.items.len(),
                                human(img.file_size)
                            ));
                        }
                        (None, Some(e)) => {
                            ui.colored_label(Color32::from_rgb(200, 60, 60), e);
                        }
                        _ => {
                            ui.label(RichText::new("or drop an aml_upgrade_package .img file here").weak());
                        }
                    }
                });

                if let Some(img) = self.image.clone() {
                    ui.add_space(4.0);
                    let cache = self.sha_cache.lock().unwrap().clone();
                    let total: u64 = self.selected.iter().filter_map(|p| img.partition(p)).map(|p| p.size).sum();
                    let summary = if self.selected.is_empty() {
                        RichText::new("Nothing selected: tick the partitions to write.").color(Color32::from_rgb(220, 160, 40))
                    } else {
                        RichText::new(format!("Will write: {}  ({})", self.selected.iter().cloned().collect::<Vec<_>>().join(", "), human(total))).strong()
                    };
                    ui.label(summary);
                    egui::ScrollArea::vertical().id_salt("parts").max_height(300.0).show(ui, |ui| {
                        egui::Grid::new("parts_grid").striped(true).num_columns(5).spacing([16.0, 4.0]).show(ui, |ui| {
                            ui.label(RichText::new("Flash").strong());
                            ui.label(RichText::new("Partition").strong());
                            ui.label(RichText::new("Size").strong());
                            ui.label(RichText::new("Format").strong());
                            ui.label(RichText::new("Checksum").strong());
                            ui.end_row();
                            for p in img.partitions() {
                                let name = p.sub_type.clone();
                                let mut on = self.selected.contains(&name);
                                let enabled = !self.running && !p.is_protected();
                                let resp = ui.add_enabled(enabled, egui::Checkbox::without_text(&mut on));
                                if p.is_protected() {
                                    resp.on_disabled_hover_text("Boot loader partitions are not written by this tool (needs a full ROM-stage burn).");
                                }
                                if on {
                                    self.selected.insert(name.clone());
                                } else {
                                    self.selected.remove(&name);
                                }
                                ui.label(if p.is_protected() { RichText::new(format!("{name} (protected)")).weak() } else { RichText::new(&name) });
                                ui.label(human(p.size));
                                ui.label(p.format());
                                let has = cache.iter().find(|(n, _)| n == &name).and_then(|(_, s)| s.clone());
                                ui.label(match has {
                                    Some(s) => RichText::new(format!("sha1 {}…", &s[..10])).monospace(),
                                    None => RichText::new("none").weak(),
                                });
                                ui.end_row();
                            }
                        });
                    });
                }
            });

            ui.add_space(6.0);
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("2. Options").strong());
                ui.add_enabled_ui(!self.running, |ui| {
                    ui.checkbox(&mut self.precheck, "Verify image CRC and SHA-1 on this Mac before flashing");
                    ui.checkbox(&mut self.reboot, "Reboot the box when finished");
                });
                ui.label(RichText::new("Nothing is erased: only the selected partitions are written, then verified by the box.").weak());
            });

            ui.add_space(6.0);
            ui.group(|ui| {
                ui.set_width(ui.available_width());
                ui.label(RichText::new("3. Flash").strong());
                ui.label("Connect a USB-A ↔ USB-C cable from the box's USB port to this Mac, press Flash, then unplug and re-plug the box's power. The box listens for this app for about one second while booting.");
                ui.horizontal(|ui| {
                    let can_flash = !self.running && self.image.is_some() && !self.selected.is_empty();
                    if ui.add_enabled(can_flash, egui::Button::new(RichText::new("⚡ Flash").strong()).min_size(egui::vec2(110.0, 30.0))).clicked() {
                        self.confirm = Some(false);
                    }
                    if ui.add_enabled(!self.running, egui::Button::new("Probe box").min_size(egui::vec2(90.0, 30.0))).on_hover_text("Connect and identify the box without writing anything").clicked() {
                        self.start(ctx, true);
                    }
                    if self.running && ui.button("Cancel").on_hover_text("Only possible before writing starts").clicked() {
                        self.cancel.store(true, Ordering::SeqCst);
                    }
                });
                if self.running || self.state.result.is_some() {
                    ui.add_space(4.0);
                    let bar = match self.state.progress {
                        Some(f) => egui::ProgressBar::new(f).show_percentage(),
                        None => egui::ProgressBar::new(0.0).animate(self.running),
                    };
                    ui.add(bar.text(self.state.progress_label.clone()));
                }
                match &self.state.result {
                    Some(Ok(())) => {
                        ui.colored_label(Color32::from_rgb(40, 170, 80), "✔ Finished successfully");
                    }
                    Some(Err(e)) => {
                        ui.colored_label(Color32::from_rgb(200, 60, 60), format!("✖ {e}"));
                    }
                    None => {}
                }
            });

            ui.add_space(6.0);
            ui.label(RichText::new("Log").strong());
            egui::Frame::group(ui.style()).show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::vertical().id_salt("log").stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
                    for l in &self.state.log {
                        ui.label(RichText::new(l).monospace().size(12.0));
                    }
                });
            });
        });

        if let Some(probe) = self.confirm {
            let mut open = true;
            let mut go = false;
            egui::Window::new("Confirm flash").collapsible(false).resizable(false).anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0]).open(&mut open).show(ctx, |ui| {
                ui.label("These partitions on the box will be overwritten:");
                for p in &self.selected {
                    let size = self.image.as_ref().and_then(|i| i.partition(p)).map(|i| human(i.size)).unwrap_or_default();
                    ui.label(RichText::new(format!("  • {p}  ({size})")).monospace());
                }
                ui.add_space(6.0);
                ui.label("Keep the box powered and the cable connected until it finishes. The image must be built for this exact box model.");
                ui.horizontal(|ui| {
                    if ui.button(RichText::new("Flash now").strong()).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.confirm = None;
                    }
                });
            });
            if go {
                self.confirm = None;
                self.start(ctx, probe);
            } else if !open {
                self.confirm = None;
            }
        }
    }
}
