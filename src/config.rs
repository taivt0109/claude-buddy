//! Settings kept between runs, in %APPDATA%\claude-buddy-rs\config.json.

use serde_json::{json, Value};
use std::fs;
use std::path::PathBuf;

/// Not the original Claude Buddy's 47321, so both can run side by side.
pub const DEFAULT_PORT: u16 = 47329;
const OLD_CHARACTERS: [&str; 3] = ["mochi", "meo-muop", "gau-bo"];
/// What the small, normal and large sizes scale the character by.
pub const SIZE_FACTORS: [f32; 3] = [0.75, 1.0, 1.25];
/// Wheel zoom never goes below this; the top is wherever the character fills the screen.
pub const MIN_ZOOM: f32 = 0.5;
/// The skills' keys, in config.json under these names: Q teleport, W clones, R full screen.
pub const SKILL_NAMES: [&str; 3] = ["blink", "clones", "ultimate"];
pub const DEFAULT_SKILL_KEYS: [u8; 3] = [b'Q', b'W', b'R'];

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub port: u16,
    pub position: Option<(i32, i32)>,
    pub muted: bool,
    pub toasts: bool,
    pub character: String,
    /// How far the character sits from its usual spot inside the window, in logical pixels: past a screen
    /// edge the window stops and the character keeps going, so it can reach every edge.
    pub shift: f32,
    pub lift: f32,
    /// Character size: 0 small, 1 normal, 2 large.
    pub size: u8,
    /// What the character is actually scaled by: one of the sizes above, or wherever the wheel left it.
    pub zoom: f32,
    /// Get out of the way of full-screen apps and presentations.
    pub auto_hide: bool,
    /// See-through window backgrounds, where Windows offers them.
    pub translucent: bool,
    /// The first-run "connect to Claude Code?" question has been asked.
    pub asked_hooks: bool,
    /// "vi", "en", or "auto" to follow Windows.
    pub language: String,
    /// Skills going off by themselves as the sessions' state changes. None follows the character.
    pub auto_skills: Option<bool>,
    /// How long the sessions work before shadow clones come out to help.
    pub auto_clone_secs: u32,
    /// Install new releases by itself.
    pub auto_update: bool,
    /// The letter or digit each skill is pressed with, as in SKILL_NAMES; 0 leaves that skill to the menu.
    pub skill_keys: [u8; 3],
}

impl Default for Config {
    fn default() -> Self {
        Config {
            port: DEFAULT_PORT,
            position: None,
            muted: false,
            toasts: true,
            character: "mochi".into(),
            shift: 0.0,
            lift: 0.0,
            size: 1,
            zoom: 1.0,
            auto_hide: true,
            translucent: true,
            asked_hooks: false,
            language: "auto".into(),
            auto_skills: None,
            auto_clone_secs: 20,
            auto_update: true,
            skill_keys: DEFAULT_SKILL_KEYS,
        }
    }
}

/// What the settings page can change.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Setting {
    Hooks,
    Sound,
    Toasts,
    Autostart,
    AutoHide,
    Translucent,
    AutoUpdate,
    /// Skills going off by themselves as the sessions change; Windows only so far.
    AutoSkills,
    Size(u8),
    /// 0 follows the system, 1 Vietnamese, 2 English.
    Language(u8),
    ResetPosition,
    Folder,
}

#[derive(Clone, Copy, PartialEq, Default)]
pub struct SettingsView {
    pub hooks: bool,
    pub sound: bool,
    pub toasts: bool,
    pub autostart: bool,
    pub auto_hide: bool,
    pub translucent: bool,
    pub auto_update: bool,
    pub size: u8,
    pub language: u8,
    pub auto_skills: bool,
    pub skill_keys: [u8; 3],
}

pub fn dir() -> PathBuf {
    if let Some(d) = std::env::var_os("CLAUDE_BUDDY_RS_HOME") {
        return PathBuf::from(d);
    }
    let base = if cfg!(windows) {
        PathBuf::from(std::env::var_os("APPDATA").unwrap_or_default())
    } else if cfg!(target_os = "macos") {
        home().join("Library").join("Application Support")
    } else {
        std::env::var_os("XDG_CONFIG_HOME").map_or_else(|| home().join(".config"), PathBuf::from)
    };
    base.join("claude-buddy-rs")
}

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" }).unwrap_or_default())
}

pub fn user_root() -> PathBuf {
    dir().join("characters")
}

fn parse(text: &str) -> Config {
    let d = Config::default();
    let Ok(j) = serde_json::from_str::<Value>(text) else { return d };
    let flag = |k: &str, def| j.get(k).and_then(Value::as_bool).unwrap_or(def);
    let int = |k: &str| j.get(k).and_then(Value::as_i64).and_then(|v| i32::try_from(v).ok());
    let size = j.get("size").and_then(Value::as_u64).map_or(d.size, |v| v.min(2) as u8);
    Config {
        port: j.get("port").and_then(Value::as_u64).and_then(|p| u16::try_from(p).ok()).filter(|p| *p != 0).unwrap_or(d.port),
        position: int("x").zip(int("y")),
        muted: flag("muted", d.muted),
        toasts: flag("toasts", d.toasts),
        character: match j.get("character") {
            Some(Value::String(id)) if !id.is_empty() => id.clone(),
            // Before custom characters this was an index into the built-in list.
            Some(Value::Number(n)) => OLD_CHARACTERS.get(n.as_u64().unwrap_or(0) as usize).unwrap_or(&OLD_CHARACTERS[0]).to_string(),
            _ => d.character,
        },
        shift: j.get("shift").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        lift: j.get("lift").and_then(Value::as_f64).unwrap_or(0.0) as f32,
        size,
        // Files from before wheel zoom only have the size.
        zoom: j.get("zoom").and_then(Value::as_f64).map(|z| z as f32).filter(|z| z.is_finite() && *z >= MIN_ZOOM).unwrap_or(SIZE_FACTORS[size as usize]),
        auto_hide: flag("autoHide", d.auto_hide),
        translucent: flag("translucent", d.translucent),
        asked_hooks: flag("askedHooks", d.asked_hooks),
        language: j.get("language").and_then(Value::as_str).unwrap_or(&d.language).to_string(),
        auto_skills: j.get("autoSkills").and_then(Value::as_bool),
        auto_clone_secs: j.get("autoCloneSeconds").and_then(Value::as_u64).map_or(d.auto_clone_secs, |s| s.clamp(1, 3600) as u32),
        auto_update: flag("autoUpdate", d.auto_update),
        skill_keys: skill_keys(j.get("skillKeys")),
    }
}

/// One letter or digit each; "" turns a skill's key off. Anything else keeps the usual key, and a key
/// already taken by an earlier skill is left off rather than doing two things at once.
fn skill_keys(v: Option<&Value>) -> [u8; 3] {
    let mut keys = DEFAULT_SKILL_KEYS;
    for (i, name) in SKILL_NAMES.iter().enumerate() {
        match v.and_then(|v| v.get(name)).and_then(Value::as_str).map(str::trim) {
            Some("") => keys[i] = 0,
            Some(s) if s.len() == 1 && s.as_bytes()[0].is_ascii_alphanumeric() => keys[i] = s.as_bytes()[0].to_ascii_uppercase(),
            _ => {}
        }
        if keys[i] != 0 && keys[..i].contains(&keys[i]) {
            keys[i] = 0;
        }
    }
    keys
}

fn serialize(c: &Config) -> String {
    let mut j = json!({
        "port": c.port,
        "muted": c.muted,
        "toasts": c.toasts,
        "character": c.character,
        "shift": c.shift,
        "lift": c.lift,
        "size": c.size,
        "zoom": c.zoom,
        "autoHide": c.auto_hide,
        "translucent": c.translucent,
        "askedHooks": c.asked_hooks,
        "language": c.language,
        "autoCloneSeconds": c.auto_clone_secs,
        "autoUpdate": c.auto_update,
        // Always written, so the file shows what can be changed.
        "skillKeys": SKILL_NAMES.iter().zip(c.skill_keys).map(|(n, k)| (n.to_string(), json!(if k == 0 { String::new() } else { (k as char).to_string() }))).collect::<serde_json::Map<_, _>>(),
    });
    if let Some(on) = c.auto_skills {
        j["autoSkills"] = json!(on);
    }
    if let Some((x, y)) = c.position {
        j["x"] = json!(x);
        j["y"] = json!(y);
    }
    serde_json::to_string_pretty(&j).unwrap_or_default()
}

impl Config {
    pub fn load() -> Config {
        fs::read_to_string(dir().join("config.json")).map_or_else(|_| Config::default(), |t| parse(&t))
    }

    /// Losing a setting is not worth interrupting the user for.
    pub fn save(&self) {
        let d = dir();
        let _ = fs::create_dir_all(&d);
        let tmp = d.join("config.json.tmp");
        if fs::write(&tmp, serialize(self)).is_ok() {
            let _ = fs::rename(&tmp, d.join("config.json"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips() {
        let c = Config { port: 5000, position: Some((-20, 300)), muted: true, toasts: false, character: "bear".into(), shift: -12.0, lift: 40.0, size: 2, zoom: 3.5, auto_hide: false, translucent: false, asked_hooks: true, language: "en".into(), auto_skills: Some(false), auto_clone_secs: 45, auto_update: false, skill_keys: [b'A', 0, b'7'] };
        assert_eq!(parse(&serialize(&c)), c);
        let c = Config { auto_skills: None, ..c };
        assert_eq!(parse(&serialize(&c)), c);
    }

    #[test]
    fn skill_keys_take_one_letter_or_digit_each() {
        assert_eq!(parse("{}").skill_keys, DEFAULT_SKILL_KEYS);
        assert_eq!(parse(r#"{"skillKeys":{"blink":"a","clones":"","ultimate":"5"}}"#).skill_keys, [b'A', 0, b'5']);
        // Too long, not a letter, or not text: the usual key stays.
        assert_eq!(parse(r#"{"skillKeys":{"blink":"F1","clones":"!","ultimate":3}}"#).skill_keys, DEFAULT_SKILL_KEYS);
        // Taken by an earlier skill: off, not two skills on one key.
        assert_eq!(parse(r#"{"skillKeys":{"blink":"W"}}"#).skill_keys, [b'W', 0, b'R']);
    }

    #[test]
    fn zoom_falls_back_to_the_size() {
        assert_eq!(parse(r#"{"size":2}"#).zoom, 1.25);
        assert_eq!(parse(r#"{"size":0,"zoom":0.1}"#).zoom, 0.75);
        assert_eq!(parse(r#"{"size":0,"zoom":4}"#).zoom, 4.0);
    }

    #[test]
    fn broken_or_partial_files_fall_back_to_defaults() {
        assert_eq!(parse("not json"), Config::default());
        let c = parse(r#"{"muted":true,"port":0}"#);
        assert!(c.muted);
        assert_eq!(c.port, DEFAULT_PORT);
    }

    #[test]
    fn old_numeric_characters_keep_working() {
        assert_eq!(parse(r#"{"character":2}"#).character, "gau-bo");
        assert_eq!(parse(r#"{"character":99}"#).character, "mochi");
    }
}
