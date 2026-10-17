//! Debug builds only: plays a script of input events and window screenshots, so the real UI can
//! be driven and checked without a person or OS accessibility permissions. Events go into egui's
//! raw input before each frame, so they take the same path as real keys and clicks from there on.
//!
//! `TABLEPLUS_DEV_SCRIPT=<file>` names the script. One command per line, `#` starts a comment:
//!
//! ```text
//! wait 800                  milliseconds
//! click 640 400             points from the window's top-left
//! dclick 640 400            double-click
//! drag 640 400 640 200     press, move and release
//! move 640 400
//! scroll 0 -120
//! pinch 1.5                 a trackpad pinch, zooming by that factor
//! key cmd+shift+E           also Enter, Escape, Down, Left, … (`cmd` is Ctrl off macOS)
//! type select 1             typed text
//! paste /tmp/query.sql      a file's contents, as if pasted
//! theme dark                or light
//! size 1360 860             inner window size in points
//! shot /tmp/plan.png        saves the next frame as PNG
//! measure 120               prints the CPU time of the next 120 frames
//! quit
//! ```

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Event, Key, Modifiers, PointerButton, Pos2, RawInput, UserData, ViewportCommand, pos2, vec2};

use crate::theme::ThemeChoice;

enum Command {
    Wait(Duration),
    /// A button press or release; each gets a frame of its own, as a real click does.
    Button {
        at: Pos2,
        pressed: bool,
    },
    Move(Pos2),
    Scroll(egui::Vec2),
    Pinch(f32),
    Key(Modifiers, Key),
    Text(String),
    Paste(String),
    Theme(ThemeChoice),
    Size(egui::Vec2),
    Shot(PathBuf),
    Measure(usize),
    Quit,
}

pub struct DevScript {
    commands: VecDeque<Command>,
    resume_at: Instant,
    awaiting_shot: Option<PathBuf>,
    /// Frames still to measure, and the CPU seconds of those measured so far.
    measuring: Option<(usize, Vec<f32>)>,
}

impl DevScript {
    /// Loads the script named by `TABLEPLUS_DEV_SCRIPT`, if set.
    pub fn from_env() -> Option<Self> {
        let path = std::env::var_os("TABLEPLUS_DEV_SCRIPT")?;
        let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("dev script {path:?}: {e}"));
        let commands = text
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .flat_map(|l| parse(l).unwrap_or_else(|| panic!("dev script: cannot read `{l}`")))
            .collect();
        Some(Self { commands, resume_at: Instant::now(), awaiting_shot: None, measuring: None })
    }

    /// Runs every command that is due, adding its events to this frame's input.
    /// Records the CPU time eframe reports for the frame that just ended, while measuring.
    pub fn frame_done(&mut self, cpu_seconds: Option<f32>) {
        let Some((left, samples)) = &mut self.measuring else { return };
        samples.extend(cpu_seconds);
        *left = left.saturating_sub(1);
        if *left == 0 {
            let mut ms: Vec<f32> = samples.iter().map(|s| s * 1000.0).collect();
            ms.sort_by(f32::total_cmp);
            let mean = ms.iter().sum::<f32>() / ms.len().max(1) as f32;
            let p95 = ms.get(ms.len() * 95 / 100).copied().unwrap_or_default();
            let max = ms.last().copied().unwrap_or_default();
            eprintln!(
                "dev script: {} frames, CPU per frame mean {mean:.2} ms, p95 {p95:.2} ms, max {max:.2} ms",
                ms.len()
            );
            self.measuring = None;
        }
    }

    pub fn feed(&mut self, ctx: &egui::Context, input: &mut RawInput, theme: &mut ThemeChoice) {
        ctx.request_repaint();
        if self.measuring.is_some() {
            return;
        }
        if let Some(path) = self.awaiting_shot.take() {
            match input.events.iter().find_map(|e| match e {
                Event::Screenshot { image, .. } => Some(image.clone()),
                _ => None,
            }) {
                Some(image) => save_png(&path, &image),
                None => {
                    self.awaiting_shot = Some(path);
                    return;
                }
            }
        }
        while Instant::now() >= self.resume_at {
            let Some(command) = self.commands.pop_front() else { return };
            match command {
                Command::Wait(d) => self.resume_at = Instant::now() + d,
                Command::Button { at, pressed } => {
                    input.events.push(Event::PointerMoved(at));
                    input.events.push(Event::PointerButton {
                        pos: at,
                        button: PointerButton::Primary,
                        pressed,
                        modifiers: Modifiers::NONE,
                    });
                    return;
                }
                Command::Move(at) => {
                    input.events.push(Event::PointerMoved(at));
                    return;
                }
                Command::Scroll(delta) => input.events.push(Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta,
                    modifiers: Modifiers::NONE,
                    phase: egui::TouchPhase::Move,
                }),
                Command::Pinch(factor) => input.events.push(Event::Zoom(factor)),
                Command::Key(modifiers, key) => {
                    for pressed in [true, false] {
                        input.events.push(Event::Key { key, physical_key: None, pressed, repeat: false, modifiers });
                    }
                    self.resume_at = Instant::now() + Duration::from_millis(60);
                }
                Command::Text(text) => input.events.push(Event::Text(text)),
                Command::Paste(text) => input.events.push(Event::Paste(text)),
                Command::Theme(t) => *theme = t,
                Command::Size(size) => ctx.send_viewport_cmd(ViewportCommand::InnerSize(size)),
                Command::Shot(path) => {
                    ctx.send_viewport_cmd(ViewportCommand::Screenshot(UserData::default()));
                    self.awaiting_shot = Some(path);
                    return;
                }
                Command::Measure(frames) => {
                    self.measuring = Some((frames, Vec::new()));
                    return;
                }
                Command::Quit => ctx.send_viewport_cmd(ViewportCommand::Close),
            }
        }
    }
}

/// One script line as commands; a click is a move, a press and a release.
fn parse(line: &str) -> Option<Vec<Command>> {
    let (word, rest) = line.split_once(' ').unwrap_or((line, ""));
    let numbers = || rest.split_whitespace().map(str::parse::<f32>).collect::<Result<Vec<_>, _>>().ok();
    let xy = || numbers().filter(|n| n.len() == 2).map(|n| pos2(n[0], n[1]));
    let clicks = |n: usize| {
        let at = xy()?;
        let mut out = vec![Command::Move(at)];
        for _ in 0..n {
            out.extend([Command::Button { at, pressed: true }, Command::Button { at, pressed: false }]);
        }
        Some(out)
    };
    Some(vec![match word {
        "click" => return clicks(1),
        "dclick" => return clicks(2),
        "wait" => Command::Wait(Duration::from_millis(rest.trim().parse().ok()?)),
        "move" => Command::Move(xy()?),
        "drag" => {
            let n = numbers().filter(|n| n.len() == 4)?;
            let (from, to) = (pos2(n[0], n[1]), pos2(n[2], n[3]));
            return Some(vec![
                Command::Move(from),
                Command::Button { at: from, pressed: true },
                Command::Move(pos2((from.x + to.x) / 2.0, (from.y + to.y) / 2.0)),
                Command::Move(to),
                Command::Button { at: to, pressed: false },
            ]);
        }
        "scroll" => Command::Scroll(xy()?.to_vec2()),
        "pinch" => Command::Pinch(rest.trim().parse().ok()?),
        "size" => Command::Size(xy().map(|p| vec2(p.x, p.y))?),
        "key" => {
            let mut modifiers = Modifiers::NONE;
            let mut key = None;
            for part in rest.trim().split('+') {
                match part {
                    "cmd" if cfg!(target_os = "macos") => modifiers |= Modifiers::MAC_CMD | Modifiers::COMMAND,
                    "cmd" => modifiers |= Modifiers::CTRL | Modifiers::COMMAND,
                    "shift" => modifiers |= Modifiers::SHIFT,
                    "alt" => modifiers |= Modifiers::ALT,
                    "ctrl" => modifiers |= Modifiers::CTRL,
                    name => key = Key::from_name(name),
                }
            }
            Command::Key(modifiers, key?)
        }
        "type" => Command::Text(rest.to_string()),
        "paste" => Command::Paste(std::fs::read_to_string(rest.trim()).ok()?.trim_end().to_string()),
        "theme" => Command::Theme(match rest.trim() {
            "dark" => ThemeChoice::Dark,
            "light" => ThemeChoice::Light,
            _ => return None,
        }),
        "shot" => Command::Shot(PathBuf::from(rest.trim())),
        "measure" => Command::Measure(rest.trim().parse().ok()?),
        "quit" => Command::Quit,
        _ => return None,
    }])
}

fn save_png(path: &std::path::Path, image: &egui::ColorImage) {
    let bytes: Vec<u8> = image.pixels.iter().flat_map(|c| c.to_srgba_unmultiplied()).collect();
    let file = std::fs::File::create(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), image.size[0] as u32, image.size[1] as u32);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(&bytes).expect("png data");
    eprintln!("dev script: saved {}", path.display());
}
