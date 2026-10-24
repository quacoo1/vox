//! vox-shot: render a `.slint` component with JSON demo data to a PNG.
//!
//! Usage:
//!   vox-shot <file.slint> [--component Name] [--data data.json] [--size 1280x800]
//!            [--out out.png] [--scale 1.25] [--wait 400] [--set key=value]... [--show]
//!
//! JSON data: top-level keys are root properties (`"title": "Hi"`), or global properties
//! written as `"Global.prop"` (`"Theme.dark": true`). Values convert as:
//!   string -> string            {"$color": "#rrggbb[aa]"} -> brush/color
//!   number -> number/length     {"$image": "path.png"}    -> image (relative to the JSON file)
//!   bool   -> bool              {"$enum": "EnumName.value"} -> enum value
//!   array  -> model             object -> struct
//! A global is only settable when the previewed file re-exports it (`export { Theme } from "...";`).
//! `--set key=value` takes the same forms as JSON (`--set Theme.dark=false`, `--set 'rows=[...]'`); a value
//! that is not valid JSON is used as a plain string.
//! The window is placed off-screen, rendered, snapshotted to PNG, then the tool exits.
//! Compile diagnostics are printed; any error exits with code 2.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use i_slint_core::model::{ModelRc, VecModel};
use i_slint_core::timers::Timer;
use slint_interpreter::{Brush, Color, ComponentHandle, Compiler, Image, SharedString, Struct, Value};

struct Args {
    file: PathBuf,
    component: Option<String>,
    data: Option<PathBuf>,
    size: Option<(f32, f32)>,
    out: PathBuf,
    scale: Option<f32>,
    wait_ms: u64,
    sets: Vec<(String, String)>,
    show: bool,
    style: String,
}

fn usage() -> ! {
    eprintln!(
        "usage: vox-shot <file.slint> [--component Name] [--data data.json] [--size WxH] \
         [--out out.png] [--scale F] [--wait ms] [--set key=json]... [--style fluent] [--show]"
    );
    std::process::exit(64);
}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    let mut file = None;
    let mut a = Args {
        file: PathBuf::new(),
        component: None,
        data: None,
        size: None,
        out: PathBuf::from("shot.png"),
        scale: None,
        wait_ms: 450,
        sets: vec![],
        show: false,
        style: "fluent".into(),
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--component" => a.component = Some(it.next().unwrap_or_else(|| usage())),
            "--data" => a.data = Some(it.next().unwrap_or_else(|| usage()).into()),
            "--out" => a.out = it.next().unwrap_or_else(|| usage()).into(),
            "--scale" => a.scale = it.next().and_then(|s| s.parse().ok()),
            "--wait" => a.wait_ms = it.next().and_then(|s| s.parse().ok()).unwrap_or(450),
            "--style" => a.style = it.next().unwrap_or_else(|| usage()),
            "--show" => a.show = true,
            "--size" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (w, h) = s.split_once('x').unwrap_or_else(|| usage());
                a.size = Some((w.parse().unwrap_or_else(|_| usage()), h.parse().unwrap_or_else(|_| usage())));
            }
            "--set" => {
                let s = it.next().unwrap_or_else(|| usage());
                let (k, v) = s.split_once('=').unwrap_or_else(|| usage());
                a.sets.push((k.to_string(), v.to_string()));
            }
            "-h" | "--help" => usage(),
            _ if file.is_none() => file = Some(PathBuf::from(arg)),
            _ => usage(),
        }
    }
    a.file = file.unwrap_or_else(|| usage());
    a
}

fn parse_color(s: &str) -> Option<Color> {
    let h = s.strip_prefix('#')?;
    let n = u32::from_str_radix(h, 16).ok()?;
    match h.len() {
        6 => Some(Color::from_argb_u8(255, (n >> 16) as u8, (n >> 8) as u8, n as u8)),
        8 => Some(Color::from_argb_u8(n as u8, (n >> 24) as u8, (n >> 16) as u8, (n >> 8) as u8)),
        _ => None,
    }
}

fn to_value(v: &serde_json::Value, base: &Path) -> Value {
    use serde_json::Value as J;
    match v {
        J::Null => Value::Void,
        J::Bool(b) => Value::Bool(*b),
        J::Number(n) => Value::Number(n.as_f64().unwrap_or(0.0)),
        J::String(s) => Value::String(SharedString::from(s.as_str())),
        J::Array(items) => {
            let vals: Vec<Value> = items.iter().map(|i| to_value(i, base)).collect();
            Value::Model(ModelRc::from(Rc::new(VecModel::from(vals))))
        }
        J::Object(o) => {
            if let Some(J::String(c)) = o.get("$color") {
                let color = parse_color(c).unwrap_or_else(|| {
                    eprintln!("warning: bad color {c:?}");
                    Color::default()
                });
                return Value::Brush(Brush::SolidColor(color));
            }
            if let Some(J::String(p)) = o.get("$image") {
                let path = base.join(p);
                return Value::Image(Image::load_from_path(&path).unwrap_or_else(|_| {
                    eprintln!("warning: could not load image {}", path.display());
                    Image::default()
                }));
            }
            if let Some(J::String(e)) = o.get("$enum") {
                let (name, val) = e.split_once('.').unwrap_or(("", e));
                return Value::EnumerationValue(name.to_string(), val.to_string());
            }
            let s: Struct = o.iter().map(|(k, v)| (k.clone(), to_value(v, base))).collect();
            Value::Struct(s)
        }
    }
}

fn apply(inst: &slint_interpreter::ComponentInstance, key: &str, val: Value) {
    let res = match key.split_once('.') {
        Some((global, prop)) => inst
            .set_global_property(global, prop, val)
            .map_err(|e| format!("{e:?}")),
        None => inst.set_property(key, val).map_err(|e| format!("{e:?}")),
    };
    if let Err(e) = res {
        eprintln!("warning: could not set {key}: {e}");
    }
}

fn main() -> ExitCode {
    let args = parse_args();
    if let Some(scale) = args.scale {
        std::env::set_var("SLINT_SCALE_FACTOR", scale.to_string());
    }

    let file = std::fs::canonicalize(&args.file).unwrap_or_else(|_| args.file.clone());
    let mut compiler = Compiler::default();
    compiler.set_style(args.style.clone());
    // Resolve imports relative to the file and to the nearest `ui/` ancestor.
    let mut includes = vec![];
    if let Some(dir) = file.parent() {
        includes.push(dir.to_path_buf());
        let mut d = Some(dir);
        while let Some(p) = d {
            if p.file_name().map(|n| n == "ui").unwrap_or(false) {
                includes.push(p.to_path_buf());
                break;
            }
            d = p.parent();
        }
    }
    compiler.set_include_paths(includes);

    let result = spin_on::spin_on(compiler.build_from_path(&file));
    let mut had_error = false;
    for d in result.diagnostics() {
        if d.level() == slint_interpreter::DiagnosticLevel::Error {
            had_error = true;
        }
        eprintln!("{d}");
    }
    if had_error {
        return ExitCode::from(2);
    }

    let def = match &args.component {
        Some(name) => result.component(name),
        None => result.components().last(),
    };
    let Some(def) = def else {
        eprintln!(
            "error: component not found. exported components: {:?}",
            result.components().map(|c| c.name().to_string()).collect::<Vec<_>>()
        );
        return ExitCode::from(2);
    };

    let inst = match def.create() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: could not create component: {e}");
            return ExitCode::from(3);
        }
    };

    if let Some(data) = &args.data {
        let text = match std::fs::read_to_string(data) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("error: cannot read {}: {e}", data.display());
                return ExitCode::from(2);
            }
        };
        let json: serde_json::Value = match serde_json::from_str(&text) {
            Ok(j) => j,
            Err(e) => {
                eprintln!("error: bad JSON in {}: {e}", data.display());
                return ExitCode::from(2);
            }
        };
        let base = data.parent().map(Path::to_path_buf).unwrap_or_default();
        if let serde_json::Value::Object(map) = json {
            for (k, v) in map.iter() {
                apply(&inst, k, to_value(v, &base));
            }
        }
    }
    for (k, v) in &args.sets {
        let json: serde_json::Value =
            serde_json::from_str(v).unwrap_or(serde_json::Value::String(v.clone()));
        apply(&inst, k, to_value(&json, Path::new(".")));
    }

    // Log callbacks instead of failing when the component invokes them.
    let names: HashMap<String, ()> = def.callbacks().map(|c| (c, ())).collect();
    for name in names.keys() {
        let n = name.clone();
        let _ = inst.set_callback(name, move |_| {
            eprintln!("callback: {n}");
            Value::Void
        });
    }

    if let Some((w, h)) = args.size {
        inst.window().set_size(slint_interpreter::LogicalSize::new(w, h));
    }
    if !args.show {
        inst.window().set_position(slint_interpreter::PhysicalPosition::new(-32000, -32000));
    }
    if let Err(e) = inst.show() {
        eprintln!("error: show failed: {e}");
        return ExitCode::from(3);
    }

    let out = args.out.clone();
    let weak = inst.as_weak();
    let show = args.show;
    let code = Rc::new(std::cell::Cell::new(0u8));
    let code2 = code.clone();
    Timer::single_shot(std::time::Duration::from_millis(args.wait_ms), move || {
        let inst = weak.upgrade().unwrap();
        match inst.window().take_snapshot() {
            Ok(buf) => {
                let img = image::RgbaImage::from_raw(buf.width(), buf.height(), buf.as_bytes().to_vec());
                match img.map(|i| i.save(&out)) {
                    Some(Ok(())) => println!("saved {} ({}x{})", out.display(), buf.width(), buf.height()),
                    _ => {
                        eprintln!("error: could not write {}", out.display());
                        code2.set(4);
                    }
                }
            }
            Err(e) => {
                eprintln!("error: snapshot failed: {e}");
                code2.set(4);
            }
        }
        if !show {
            let _ = slint_interpreter::quit_event_loop();
        }
    });
    if let Err(e) = slint_interpreter::run_event_loop() {
        eprintln!("error: event loop: {e}");
        return ExitCode::from(3);
    }
    ExitCode::from(code.get())
}
