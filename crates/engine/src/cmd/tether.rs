//! Tether commands. The menu entries sit in the File menu; the loupe bar calls the same ids.

use lightcraft_ptp::Dial;
use serde_json::{Value, json};

use super::{always, bad, cmd, ok};
use crate::{Result, Session};

pub fn specs() -> Vec<crate::cmd::CommandSpec> {
    vec![
        cmd!("tether.start", "Start Tethered Capture", ["File"], None, "{ preset?: preset id applied to each shot }", start_enabled, start),
        cmd!("tether.stop", "Stop Tethered Capture", ["File"], None, "{}", stop_enabled, stop),
        cmd!("tether.capture", "Take Picture", [], None, "{}", always, capture),
        cmd!(
            "tether.set",
            "Set Camera",
            [],
            None,
            "{ dial: aperture|shutter|iso|whiteBalance, step?: -1|1, value?: number, magnify?: bool, focus?: bool, nudge?: up|down|left|right }",
            always,
            set
        ),
        cmd!(query "tether.status", "Tether Status", [], None, "{}", always, |s, _| Ok(crate::tether::status_json(s))),
        cmd!(query "tether.frame", "Tether Frame", [], None, "{}", always, frame),
    ]
}

fn start_enabled(s: &Session) -> std::result::Result<(), String> {
    if s.tether.is_running() { Err("already tethered".into()) } else { Ok(()) }
}

fn stop_enabled(s: &Session) -> std::result::Result<(), String> {
    if s.tether.is_running() || s.tether.status().error.is_some() { Ok(()) } else { Err("not tethered".into()) }
}

fn start(s: &mut Session, p: &Value) -> Result<Value> {
    let preset = p.get("preset").and_then(Value::as_str).map(str::to_string);
    if p.get("preset").is_some() && preset.is_none() {
        return Err(bad("tether.start", "`preset` must be a string"));
    }
    let activity = s.activity.clone();
    s.tether.begin(activity, preset)?;
    Ok(json!({"started": true}))
}

fn stop(s: &mut Session, _: &Value) -> Result<Value> {
    s.tether.stop();
    ok()
}

fn capture(s: &mut Session, _: &Value) -> Result<Value> {
    if let Some(file) = s.tether.take_script_capture()? {
        let preset = s.tether.preset_id();
        let id = crate::tether::import_capture(s, &file, preset.as_deref())?;
        return Ok(json!({"photo": id, "name": file.name}));
    }
    s.tether.queue_shoot()?;
    Ok(json!({"queued": true}))
}

fn set(s: &mut Session, p: &Value) -> Result<Value> {
    if let Some(on) = p.get("magnify").and_then(Value::as_bool) {
        if s.tether.apply_script_magnify(on)? {
            return Ok(json!({"magnified": on}));
        }
        s.tether.queue_magnify(on)?;
        return Ok(json!({"queued": true}));
    }
    if let Some(direction) = p.get("nudge").and_then(Value::as_str) {
        if s.tether.apply_script_nudge(direction)? {
            return Ok(json!({"nudge": direction}));
        }
        s.tether.queue_nudge(direction)?;
        return Ok(json!({"queued": true}));
    }
    if p.get("focus").and_then(Value::as_bool) == Some(true) {
        if s.tether.apply_script_focus()? {
            return Ok(json!({"focused": true}));
        }
        s.tether.queue_focus()?;
        return Ok(json!({"queued": true}));
    }
    let name = p.get("dial").and_then(Value::as_str).ok_or_else(|| bad("tether.set", "missing `dial`, `magnify`, or `focus`"))?;
    let dial = match name {
        "aperture" => Dial::Aperture,
        "shutter" => Dial::Shutter,
        "iso" => Dial::Iso,
        "whiteBalance" => Dial::WhiteBalance,
        _ => return Err(bad("tether.set", "dial is aperture, shutter, iso, or whiteBalance")),
    };
    let step = p.get("step").and_then(Value::as_i64).unwrap_or(0);
    // One click, whatever number arrived. A larger step is how a speed got sent as a repeat count.
    let step = i32::try_from(step).unwrap_or(0).clamp(-1, 1);
    let absolute = match p.get("value") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| bad("tether.set", "`value` must be a non-negative integer"))?),
    };
    if absolute.is_none() && step == 0 {
        return Err(bad("tether.set", "give `step` or `value`"));
    }
    if s.tether.apply_script_dial(dial, step, absolute)? {
        return Ok(crate::tether::status_json(s));
    }
    s.tether.queue_dial(dial, step, absolute)?;
    Ok(json!({"queued": true}))
}

fn frame(s: &mut Session, _: &Value) -> Result<Value> {
    let Some(frame) = s.tether.frame() else {
        return Ok(json!({"generation": 0}));
    };
    Ok(json!({"generation": frame.generation, "width": frame.width, "height": frame.height}))
}
