#![cfg_attr(not(test), windows_subsystem = "windows")]

mod clean;
mod fsx;
mod model;
mod scan;
mod win;
mod wsl;

use std::sync::mpsc::{channel, Receiver};
use std::time::Duration;

use eframe::egui::{self, Align, Color32, Layout, RichText, Vec2};

use model::*;

fn main() -> eframe::Result {
    let args: Vec<String> = std::env::args().collect();
    if let Some(i) = args.iter().position(|a| a == "--compact-helper") {
        // Elevated half of WSL compaction; see wsl::compact_all.
        if let Some(dir) = args.get(i + 1) {
            wsl::compact_helper(std::path::Path::new(dir));
        }
        return Ok(());
    }
    if let Some(i) = args.iter().position(|a| a == "--report") {
        let out = args.get(i + 1).cloned().unwrap_or_else(|| "space-sweep-report.tsv".into());
        report(&out, !args.iter().any(|a| a == "--no-wsl"));
        return Ok(());
    }
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Space Sweep")
            .with_inner_size([1060.0, 780.0])
            .with_min_inner_size([760.0, 520.0])
            .with_icon(egui::IconData {
                rgba: include_bytes!("../assets/icon-128.rgba").to_vec(),
                width: 128,
                height: 128,
            }),
        ..Default::default()
    };
    eframe::run_native("Space Sweep", options, Box::new(|cc| Ok(Box::new(App::new(cc)))))
}

/// Headless scan to a TSV file, for checking what the app finds.
fn report(path: &str, wsl: bool) {
    let (tx, rx) = channel();
    std::thread::spawn(move || scan::scan(tx, wsl));
    let mut items = Vec::new();
    let mut notes = Vec::new();
    for m in rx {
        match m {
            Msg::Item(i) => items.push(i),
            Msg::Log(s) => notes.push(s),
            Msg::WslInfo { reclaimable_hint } => notes.push(reclaimable_hint),
            Msg::ScanDone => break,
            _ => {}
        }
    }
    items.sort_by(|a, b| a.group.cmp(&b.group).then(b.size.cmp(&a.size)));
    let mut s = String::from("group\ttier\tsize\tlabel\tlocation\tdetail\n");
    for i in &items {
        s.push_str(&format!(
            "{:?}\t{}\t{}\t{}\t{}\t{}\n",
            i.group,
            i.tier.label(),
            fmt_size(i.size),
            i.label,
            i.location,
            i.detail
        ));
    }
    for n in notes {
        s.push_str(&format!("# {n}\n"));
    }
    let _ = std::fs::write(path, s);
}

#[derive(PartialEq)]
enum Phase {
    Idle,
    Scanning,
    Working,
}

struct App {
    items: Vec<Item>,
    rx: Option<Receiver<Msg>>,
    phase: Phase,
    status: String,
    log: Vec<String>,
    scan_wsl: bool,
    recycle_personal: bool,
    compact_wsl: bool,
    confirm_first: bool,
    show_confirm: bool,
    wsl_hint: String,
    filter: String,
    disk: (u64, u64),
    run_freed: u64,
    /// `--screenshot <file.bmp>`: after the scan, select all SAFE, capture the window, exit.
    screenshot: Option<String>,
    shot_frames: u32,
    shot_requested: bool,
}

fn save_bmp(path: &str, img: &egui::ColorImage) -> std::io::Result<()> {
    let [w, h] = img.size;
    let row = (w * 3 + 3) & !3;
    let data_len = row * h;
    let mut out = Vec::with_capacity(54 + data_len);
    out.extend_from_slice(b"BM");
    out.extend_from_slice(&((54 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(&[0; 4]);
    out.extend_from_slice(&54u32.to_le_bytes());
    out.extend_from_slice(&40u32.to_le_bytes());
    out.extend_from_slice(&(w as i32).to_le_bytes());
    out.extend_from_slice(&(h as i32).to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&24u16.to_le_bytes());
    out.extend_from_slice(&[0; 24]);
    for y in (0..h).rev() {
        for x in 0..w {
            let c = img.pixels[y * w + x];
            out.extend_from_slice(&[c.b(), c.g(), c.r()]);
        }
        out.extend(std::iter::repeat(0).take(row - w * 3));
    }
    std::fs::write(path, out)
}

const GREEN: Color32 = Color32::from_rgb(76, 175, 80);
const AMBER: Color32 = Color32::from_rgb(230, 160, 40);
const GREY: Color32 = Color32::from_rgb(140, 140, 140);
const RED: Color32 = Color32::from_rgb(198, 40, 40);

impl App {
    fn new(cc: &eframe::CreationContext<'_>) -> App {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());
        let mut app = App {
            items: Vec::new(),
            rx: None,
            phase: Phase::Idle,
            status: String::new(),
            log: Vec::new(),
            scan_wsl: !std::env::args().any(|a| a == "--no-wsl"),
            recycle_personal: true,
            compact_wsl: false,
            confirm_first: true,
            show_confirm: false,
            wsl_hint: String::new(),
            filter: String::new(),
            disk: win::disk_space("C:\\"),
            run_freed: 0,
            screenshot: {
                let a: Vec<String> = std::env::args().collect();
                a.iter().position(|x| x == "--screenshot").and_then(|i| a.get(i + 1).cloned())
            },
            shot_frames: 0,
            shot_requested: false,
        };
        if app.screenshot.is_some() {
            app.compact_wsl = true;
        }
        app.start_scan();
        app
    }

    fn start_scan(&mut self) {
        self.items.clear();
        self.wsl_hint.clear();
        let (tx, rx) = channel();
        let wsl = self.scan_wsl;
        std::thread::spawn(move || scan::scan(tx, wsl));
        self.rx = Some(rx);
        self.phase = Phase::Scanning;
        self.status = "Scanning…".into();
        self.disk = win::disk_space("C:\\");
    }

    fn start_delete(&mut self) {
        let chosen: Vec<Item> = self.items.iter().filter(|i| i.selected && i.deletable()).cloned().collect();
        let (tx, rx) = channel();
        let opts = clean::Options { recycle_personal: self.recycle_personal, compact_wsl: self.compact_wsl };
        self.log.push(format!(
            "Starting: {} item{}{}.",
            chosen.len(),
            if chosen.len() == 1 { "" } else { "s" },
            if self.compact_wsl { ", then WSL compaction" } else { "" }
        ));
        std::thread::spawn(move || clean::run(chosen, opts, tx));
        self.rx = Some(rx);
        self.phase = Phase::Working;
        self.run_freed = 0;
    }

    fn pump(&mut self) {
        let Some(rx) = &self.rx else { return };
        let mut finished = false;
        while let Ok(m) = rx.try_recv() {
            match m {
                Msg::Status(s) => self.status = s,
                Msg::Item(i) => self.items.push(i),
                Msg::Log(s) => self.log.push(s),
                Msg::WslInfo { reclaimable_hint } => self.wsl_hint = reclaimable_hint,
                Msg::ItemResult { id, status, gone, freed } => {
                    self.run_freed += freed;
                    if let Some(i) = self.items.iter_mut().find(|i| i.id == id) {
                        i.status = Some(status);
                        i.gone = gone;
                        i.selected = false;
                    }
                }
                Msg::ScanDone => {
                    finished = true;
                    let total: u64 = self.items.iter().filter(|i| i.deletable()).map(|i| i.size).sum();
                    self.status = format!("Scan done. {} could be freed if you deleted everything listed.", fmt_size(total));
                }
                Msg::WorkDone => {
                    finished = true;
                    self.status = "Done. See the log below.".into();
                }
            }
        }
        if finished {
            self.rx = None;
            self.phase = Phase::Idle;
            self.disk = win::disk_space("C:\\");
        }
    }

    fn selected(&self) -> impl Iterator<Item = &Item> {
        self.items.iter().filter(|i| i.selected && i.deletable())
    }

    fn matches_filter(&self, i: &Item) -> bool {
        if self.filter.trim().is_empty() {
            return true;
        }
        let f = self.filter.to_lowercase();
        i.label.to_lowercase().contains(&f) || i.location.to_lowercase().contains(&f) || i.detail.to_lowercase().contains(&f)
    }

    fn top_bar(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.heading(RichText::new("Space Sweep").strong());
            ui.add_space(12.0);
            let (free, total) = self.disk;
            let used = total.saturating_sub(free);
            let frac = if total > 0 { used as f32 / total as f32 } else { 0.0 };
            let bar = egui::ProgressBar::new(frac)
                .desired_width(320.0)
                .fill(if frac > 0.95 { RED } else if frac > 0.85 { AMBER } else { GREEN })
                .text(format!("C:  {} free of {}", fmt_size(free), fmt_size(total)));
            ui.add(bar);
            ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                let idle = self.phase == Phase::Idle;
                if ui.add_enabled(idle, egui::Button::new("Rescan")).clicked() {
                    self.start_scan();
                }
                ui.add_enabled(idle, egui::Checkbox::new(&mut self.scan_wsl, "Scan inside WSL"));
            });
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let idle = self.phase == Phase::Idle;
            if ui.add_enabled(idle, egui::Button::new("Select all SAFE")).clicked() {
                for i in self.items.iter_mut() {
                    i.selected = i.deletable() && i.tier == Tier::Safe;
                }
            }
            if ui.add_enabled(idle, egui::Button::new("Clear selection")).clicked() {
                for i in self.items.iter_mut() {
                    i.selected = false;
                }
            }
            ui.add_space(12.0);
            ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.filter).hint_text("name or path").desired_width(220.0));
            ui.add_space(12.0);
            ui.label(RichText::new("SAFE").color(GREEN).strong());
            ui.label(RichText::new("rebuilds itself").weak());
            ui.label(RichText::new("YOUR CALL").color(AMBER).strong());
            ui.label(RichText::new("won't come back").weak());
            ui.label(RichText::new("INFO").color(GREY).strong());
            ui.label(RichText::new("shown only").weak());
        });
        ui.add_space(4.0);
    }

    fn list(&mut self, ui: &mut egui::Ui) {
        let busy = self.phase != Phase::Idle;
        let list_w = ui.available_width();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show(ui, |ui| {
            ui.set_max_width(list_w);
            // Room for the collapsing-header indent and the scrollbar.
            let row_w = list_w - ui.spacing().indent - 18.0;
            if self.items.is_empty() && self.phase == Phase::Scanning {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.spinner();
                    ui.label(&self.status);
                });
                return;
            }
            for group in Group::ALL {
                let mut idx: Vec<usize> = (0..self.items.len())
                    .filter(|&k| self.items[k].group == group && self.matches_filter(&self.items[k]))
                    .collect();
                if idx.is_empty() {
                    continue;
                }
                idx.sort_by(|&a, &b| self.items[b].size.cmp(&self.items[a].size));
                let total: u64 = idx.iter().map(|&k| self.items[k].size).sum();
                let sel: u64 = idx.iter().filter(|&&k| self.items[k].selected).map(|&k| self.items[k].size).sum();
                let header = if sel > 0 {
                    format!("{}  —  {}  ({} selected)", group.title(), fmt_size(total), fmt_size(sel))
                } else {
                    format!("{}  —  {}", group.title(), fmt_size(total))
                };
                egui::CollapsingHeader::new(RichText::new(header).strong().size(15.0))
                    .id_salt(format!("{group:?}"))
                    .default_open(group != Group::WhereItGoes)
                    .show(ui, |ui| {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(group.blurb()).weak().italics());
                            if group != Group::WhereItGoes {
                                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                                    if ui.add_enabled(!busy, egui::Button::new("none").small()).clicked() {
                                        for &k in &idx {
                                            self.items[k].selected = false;
                                        }
                                    }
                                    if ui.add_enabled(!busy, egui::Button::new("all safe").small()).clicked() {
                                        for &k in &idx {
                                            let it = &mut self.items[k];
                                            it.selected = it.deletable() && it.tier == Tier::Safe;
                                        }
                                    }
                                });
                            }
                        });
                        ui.add_space(2.0);
                        for &k in &idx {
                            row(ui, &mut self.items[k], busy, row_w);
                        }
                    });
                ui.add_space(6.0);
            }
        });
    }

    fn bottom(&mut self, ui: &mut egui::Ui) {
        ui.add_space(6.0);
        let n = self.selected().count();
        let bytes: u64 = self.selected().map(|i| i.size).sum();
        let idle = self.phase == Phase::Idle;
        let has_wsl_disk = self.items.iter().any(|i| i.group == Group::Wsl && i.tier == Tier::Info && i.label.starts_with("WSL disk"));
        ui.horizontal(|ui| {
            ui.add_enabled(idle, egui::Checkbox::new(&mut self.compact_wsl, RichText::new("Compact WSL disk when deletion finishes").strong()))
                .on_hover_text("Runs fstrim inside Linux, shuts WSL down, then shrinks the .vhdx with diskpart. Windows shows one admin prompt. Close Docker Desktop and WSL terminals first.");
            if !self.scan_wsl {
                ui.label(RichText::new("(WSL scan is off; compaction still finds the disk)").weak());
            } else if !has_wsl_disk && self.phase == Phase::Idle {
                ui.label(RichText::new("(no WSL disk found)").weak());
            }
        });
        if self.compact_wsl && !self.wsl_hint.is_empty() {
            ui.label(RichText::new(format!("   {}", self.wsl_hint)).weak().small());
        }
        ui.horizontal(|ui| {
            ui.add_enabled(idle, egui::Checkbox::new(&mut self.recycle_personal, "Send your own files to the Recycle Bin (space comes back when you empty it)"));
            ui.add_enabled(idle, egui::Checkbox::new(&mut self.confirm_first, "Ask before deleting"));
        });
        ui.add_space(4.0);
        ui.horizontal(|ui| {
            let label = if n > 0 {
                format!("Delete {n} selected  ·  {}", fmt_size(bytes))
            } else if self.compact_wsl {
                "Compact WSL disk now".to_string()
            } else {
                "Select items to delete".to_string()
            };
            let enabled = idle && (n > 0 || self.compact_wsl);
            let btn = egui::Button::new(RichText::new(label).strong().size(16.0).color(Color32::WHITE))
                .fill(if enabled { RED } else { Color32::from_gray(60) })
                .min_size(Vec2::new(300.0, 36.0));
            if ui.add_enabled(enabled, btn).clicked() {
                if self.confirm_first {
                    self.show_confirm = true;
                } else {
                    self.start_delete();
                }
            }
            ui.add_space(10.0);
            if self.phase != Phase::Idle {
                ui.spinner();
            }
            if self.phase == Phase::Working && self.run_freed > 0 {
                ui.label(RichText::new(format!("{} freed so far", fmt_size(self.run_freed))).color(GREEN));
            }
            ui.label(&self.status);
        });
        ui.add_space(4.0);
        egui::CollapsingHeader::new(format!("Log ({})", self.log.len()))
            .id_salt("log")
            .default_open(true)
            .show(ui, |ui| {
                egui::ScrollArea::vertical().max_height(130.0).stick_to_bottom(true).auto_shrink([false, true]).show(ui, |ui| {
                    for l in &self.log {
                        ui.label(RichText::new(l).monospace().size(12.0));
                    }
                });
            });
        ui.add_space(4.0);
    }

    fn confirm(&mut self, ctx: &egui::Context) {
        if !self.show_confirm {
            return;
        }
        let chosen: Vec<&Item> = self.selected().collect();
        let to_bin: u64 = chosen
            .iter()
            .filter(|i| i.personal && self.recycle_personal && matches!(i.action, Action::Remove(_) | Action::Contents { .. }))
            .map(|i| i.size)
            .sum();
        let in_wsl: u64 = chosen
            .iter()
            .filter(|i| matches!(i.action, Action::WslRemove { .. } | Action::WslRoot { .. }))
            .map(|i| i.size)
            .sum();
        let total: u64 = chosen.iter().map(|i| i.size).sum();
        let permanent = total.saturating_sub(to_bin).saturating_sub(in_wsl);
        let admin = chosen.iter().any(|i| matches!(i.action, Action::DisableHibernation)) || self.compact_wsl;
        let review = chosen.iter().filter(|i| i.tier == Tier::Review).count();
        let mut go = false;
        let mut cancel = false;
        egui::Window::new("Delete these?")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
            .show(ctx, |ui| {
                ui.label(RichText::new(format!("{} items, {}", chosen.len(), fmt_size(total))).strong().size(16.0));
                ui.add_space(4.0);
                if permanent > 0 {
                    ui.label(format!("• {} deleted permanently from Windows", fmt_size(permanent)));
                }
                if to_bin > 0 {
                    ui.label(format!("• {} of your files moved to the Recycle Bin", fmt_size(to_bin)));
                }
                if in_wsl > 0 {
                    ui.label(format!("• {} deleted inside WSL{}", fmt_size(in_wsl), if self.compact_wsl { "" } else { " (Windows gets it back only after compaction)" }));
                }
                if self.compact_wsl {
                    ui.label("• then WSL shuts down and its disk is compacted");
                }
                if review > 0 {
                    ui.label(RichText::new(format!("{review} of these are marked YOUR CALL and won't come back on their own.")).color(AMBER));
                }
                if admin {
                    ui.label(RichText::new("Windows will show an admin prompt.").weak());
                }
                ui.label(RichText::new("Close browsers and apps whose caches you picked; files in use are skipped.").weak());
                ui.add_space(8.0);
                ui.horizontal(|ui| {
                    if ui.add(egui::Button::new(RichText::new("Delete").strong().color(Color32::WHITE)).fill(RED)).clicked() {
                        go = true;
                    }
                    if ui.button("Cancel").clicked() {
                        cancel = true;
                    }
                });
            });
        if go {
            self.show_confirm = false;
            self.start_delete();
        } else if cancel || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.show_confirm = false;
        }
    }
}

/// One list row. `row_w` is the usable width, measured once at the top of the list.
fn row(ui: &mut egui::Ui, item: &mut Item, busy: bool, row_w: f32) {
    let (badge, color) = match item.tier {
        Tier::Safe => ("SAFE", GREEN),
        Tier::Review => ("YOUR CALL", AMBER),
        Tier::Info => ("INFO", GREY),
    };
    egui::Frame::new()
        .inner_margin(egui::Margin::symmetric(6, 3))
        .corner_radius(4.0)
        .fill(if item.selected { Color32::from_rgba_unmultiplied(198, 40, 40, 28) } else { Color32::TRANSPARENT })
        .show(ui, |ui| {
            let inner_w = row_w - 12.0;
            ui.set_width(inner_w);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing = Vec2::new(8.0, 1.0);
                let can = item.deletable();
                let check_w = 22.0;
                ui.allocate_ui_with_layout(Vec2::new(check_w, 18.0), Layout::left_to_right(Align::Min), |ui| {
                    ui.set_width(check_w);
                    if can {
                        ui.add_enabled(!busy, egui::Checkbox::without_text(&mut item.selected));
                    }
                });
                let size_w = 86.0;
                let badge_w = 74.0;
                let text_w = (inner_w - check_w - size_w - badge_w - 3.0 * 8.0).max(160.0);
                ui.allocate_ui_with_layout(Vec2::new(text_w, 0.0), Layout::top_down(Align::Min), |ui| {
                    ui.set_width(text_w);
                    let mut title = RichText::new(&item.label).strong();
                    if item.gone {
                        title = title.strikethrough().weak();
                    }
                    ui.add(egui::Label::new(title).truncate()).on_hover_text(&item.label);
                    let mut detail = item.detail.clone();
                    if item.needs_admin {
                        detail.push_str("  (Some files need admin rights.)");
                    }
                    ui.add(egui::Label::new(RichText::new(detail).small()).truncate()).on_hover_text(&item.detail);
                    ui.add(egui::Label::new(RichText::new(&item.location).small().monospace().weak()).truncate())
                        .on_hover_text(&item.location);
                    if let Some(s) = &item.status {
                        let c = if item.gone { GREEN } else { AMBER };
                        ui.add(egui::Label::new(RichText::new(s).small().color(c)).truncate()).on_hover_text(s);
                    }
                });
                ui.allocate_ui_with_layout(Vec2::new(size_w, 20.0), Layout::right_to_left(Align::Min), |ui| {
                    ui.set_width(size_w);
                    ui.label(RichText::new(fmt_size(item.size)).strong().size(15.0));
                });
                ui.allocate_ui_with_layout(Vec2::new(badge_w, 20.0), Layout::right_to_left(Align::Min), |ui| {
                    ui.set_width(badge_w);
                    ui.label(RichText::new(badge).color(color).small().strong());
                });
            });
        });
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.pump();
        if self.phase != Phase::Idle {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
        egui::TopBottomPanel::top("top").show(ctx, |ui| self.top_bar(ui));
        egui::TopBottomPanel::bottom("bottom").show(ctx, |ui| self.bottom(ui));
        egui::CentralPanel::default().show(ctx, |ui| self.list(ui));
        self.confirm(ctx);
        self.screenshot_step(ctx);
    }
}

impl App {
    fn screenshot_step(&mut self, ctx: &egui::Context) {
        let Some(path) = self.screenshot.clone() else { return };
        if self.phase != Phase::Idle || self.items.is_empty() {
            return;
        }
        if self.shot_frames == 0 {
            for i in self.items.iter_mut() {
                i.selected = i.deletable() && i.tier == Tier::Safe;
            }
        }
        self.shot_frames += 1;
        ctx.request_repaint();
        if self.shot_frames == 5 && !self.shot_requested {
            self.shot_requested = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::UserData::default()));
        }
        let shot = ctx.input(|i| {
            i.raw.events.iter().find_map(|e| match e {
                egui::Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            })
        });
        if let Some(img) = shot {
            let _ = save_bmp(&path, &img);
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
    }
}
