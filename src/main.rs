mod adnl;
mod image;
mod job;
mod settings;
mod theme;
mod ui;
mod usb;

use eframe::egui;
use image::{AmlImage, human};
use job::{Event, FlashOptions, Level};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let Some(cmd) = args.first().map(String::as_str) {
        if matches!(cmd, "info" | "probe" | "flash" | "devices" | "help" | "--help" | "-h") {
            std::process::exit(cli(&args));
        }
    }
    let initial = args.iter().map(PathBuf::from).find(|p| p.extension().is_some_and(|e| e.eq_ignore_ascii_case("img")) && p.is_file());
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Amlogic Flasher")
            .with_inner_size([1280.0, 800.0])
            .with_min_inner_size([1060.0, 640.0])
            .with_drag_and_drop(true),
        ..Default::default()
    };
    if let Err(e) = eframe::run_native("Amlogic Flasher", options, Box::new(move |cc| Ok(Box::new(ui::App::new(cc, initial))))) {
        eprintln!("failed to start GUI: {e}");
        std::process::exit(1);
    }
}

fn cli(args: &[String]) -> i32 {
    let usage = "usage:
  amlogic-flash                         launch the GUI
  amlogic-flash info  <image.img>       list items and validate CRC / SHA-1
  amlogic-flash devices                 list Amlogic / fastboot / adb USB devices
  amlogic-flash probe                   wait for a box in USB burning mode and identify it
  amlogic-flash flash <image.img> [--part NAME]... [--no-reboot] [--skip-check]
                                        write partitions (default: super) over ADNL";
    let last = Mutex::new((String::new(), -1i32));
    let sink: Box<job::Sink> = Box::new(move |e| {
        let mut last = last.lock().unwrap();
        match e {
            Event::Log(l, m) => {
                if last.1 >= 0 {
                    eprintln!();
                }
                *last = (String::new(), -1);
                let tag = match l {
                    Level::Info => "",
                    Level::Ok => "✓ ",
                    Level::Warn => "! ",
                    Level::Error => "✗ ",
                };
                eprintln!("{tag}{m}");
            }
            Event::Progress { phase, done, total, .. } => {
                let pct = if total > 0 { (done * 100 / total) as i32 } else { 0 };
                let key: String = phase.chars().filter(|c| !c.is_ascii_digit()).collect();
                if key != last.0 || pct != last.1 {
                    if total > 0 {
                        eprint!("\r\x1b[K{phase}: {pct}% ({} / {})", human(done), human(total));
                    } else {
                        eprint!("\r\x1b[K{phase}");
                    }
                    *last = (key, pct);
                }
            }
            _ => {}
        }
    });
    let res = match args[0].as_str() {
        "info" if args.len() == 2 => info(&PathBuf::from(&args[1])),
        "devices" => {
            let devs = usb::scan(args.iter().any(|a| a == "--all"));
            if devs.is_empty() {
                println!("no Amlogic / fastboot / adb devices found");
            }
            for d in devs {
                println!("{}", d.describe());
            }
            Ok(())
        }
        "probe" => job::run(
            Arc::new(AmlImage::empty()),
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
                parts = vec!["super".to_string()];
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

fn info(path: &PathBuf) -> anyhow::Result<()> {
    let img = AmlImage::open(path)?;
    println!("{}: Amlogic image v{}, {} items, {}", path.display(), img.version, img.items.len(), human(img.file_size));
    println!("header CRC: {}", if img.check_crc(|_, _| {})? { "OK" } else { "MISMATCH" });
    println!("{:<10} {:<22} {:>12} {:<7} SHA-1", "TYPE", "NAME", "SIZE", "FORMAT");
    for it in &img.items {
        if it.main_type == "VERIFY" {
            continue;
        }
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
        println!("{:<10} {:<22} {:>12} {:<7} {}", it.main_type, it.sub_type, human(it.size), it.format(), sha);
    }
    Ok(())
}
