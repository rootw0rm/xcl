//! Pure frame matching and virtual-button scheduling; references are supplied by the caller.

pub(super) const WIDTH: usize = 64;
pub(super) const HEIGHT: usize = 36;
pub(super) type Thumbnail = [u8; WIDTH * HEIGHT];

use std::path::Path;
use serde::Deserialize;

const HOLD: f64 = 0.2;
fn yes() -> bool { true }
fn cooldown() -> f64 { 3.0 }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Config { #[serde(default)] rules: Vec<Spec>, fallback: Option<Fallback> }
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Spec {
    name: String, reference: std::path::PathBuf, region: [f64; 4], threshold: f64, sequence: String,
    #[serde(default = "yes")] once: bool,
    #[serde(default = "cooldown")] cooldown: f64,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fallback { button: String, start: f64, interval: f64 }
struct Rule { spec: Spec, reference: Thumbnail, presses: Vec<(f64, u32, String)>, end: f64, last: Option<f64> }
struct Playing { rule: usize, start: f64, frame: u64, pressed: Option<usize> }
pub(super) struct Engine {
    rules: Vec<Rule>, fallback: Option<Fallback>, fallback_press: Option<u64>,
    playing: Option<Playing>, claimed: bool, last_frame: Option<u64>,
}
#[derive(serde::Serialize)]
pub(super) struct Event {
    pub event: &'static str, pub rule: String, pub frame: u64, pub t: f64,
    #[serde(skip_serializing_if = "Option::is_none")] pub diff: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")] pub button: Option<String>,
}

/// Only the uncompressed 24-bit BMPs written by present::write_frame are accepted.
pub(super) fn thumbnail(b: &[u8]) -> Result<Thumbnail, String> {
    let bad = || "expected an intact, uncompressed 24-bit BMP".to_string();
    let word = |at| b.get(at..at + 4).and_then(|s| s.try_into().ok()).map(u32::from_le_bytes).ok_or_else(bad);
    if b.get(..2) != Some(b"BM") || b.get(26..30) != Some(&[1, 0, 24, 0]) || word(30)? != 0 { return Err(bad()); }
    let (off, dib, w, signed_h) = (word(10)? as usize, word(14)? as usize, word(18)? as i32, word(22)? as i32);
    if dib < 40 || off < 14usize.checked_add(dib).ok_or_else(bad)? || w <= 0 || signed_h == 0 { return Err(bad()); }
    let (w, h) = (w as usize, signed_h.checked_abs().ok_or_else(bad)? as usize);
    let row = w.checked_mul(3).and_then(|n| n.checked_add(3)).ok_or_else(bad)? & !3;
    if off.checked_add(row.checked_mul(h).ok_or_else(bad)?).is_none_or(|n| n > b.len()) { return Err(bad()); }
    let mut out = [0; WIDTH * HEIGHT];
    for y in 0..HEIGHT {
        let sy = y * h / HEIGHT;
        let sy = if signed_h > 0 { h - 1 - sy } else { sy };
        for x in 0..WIDTH {
            let at = off + sy * row + (x * w / WIDTH) * 3;
            out[y * WIDTH + x] = ((11 * b[at] as u16 + 59 * b[at + 1] as u16 + 30 * b[at + 2] as u16) / 100) as u8;
        }
    }
    Ok(out)
}

fn valid_region(r: [f64; 4]) -> bool {
    r.iter().all(|v| v.is_finite()) && r[0] >= 0.0 && r[1] >= 0.0 && r[2] > 0.0 && r[3] > 0.0
        && r[0] + r[2] <= 1.0 && r[1] + r[3] <= 1.0
        && ((r[0] + r[2]) * WIDTH as f64).round() > (r[0] * WIDTH as f64).round()
        && ((r[1] + r[3]) * HEIGHT as f64).round() > (r[1] * HEIGHT as f64).round()
}
fn difference(a: &Thumbnail, b: &Thumbnail, r: [f64; 4]) -> f64 {
    let (x0, y0) = ((r[0] * WIDTH as f64).round() as usize, (r[1] * HEIGHT as f64).round() as usize);
    let (x1, y1) = (((r[0] + r[2]) * WIDTH as f64).round() as usize, ((r[1] + r[3]) * HEIGHT as f64).round() as usize);
    let sum: u32 = (y0..y1).flat_map(|y| (x0..x1).map(move |x| a[y * WIDTH + x].abs_diff(b[y * WIDTH + x]) as u32)).sum();
    sum as f64 / ((x1 - x0) * (y1 - y0)) as f64
}
fn sequence(text: &str) -> Result<(Vec<(f64, u32, String)>, f64), String> {
    let (mut presses, mut next, mut last) = (Vec::new(), 0.0, None);
    for token in text.split(',').map(str::trim) {
        if let Some(wait) = token.strip_prefix("wait ") {
            let wait: f64 = wait.parse().map_err(|_| "bad wait seconds")?;
            if !wait.is_finite() || !(HOLD..=600.0).contains(&wait) { return Err("wait must be 0.2..600 seconds".into()); }
            next = last.take().unwrap_or(next) + wait;
        } else {
            let button = super::gameinput::button_named(token).ok_or_else(|| format!("unknown button {token:?}"))?;
            presses.push((next, button, token.to_ascii_uppercase()));
            last = Some(next);
            next += 0.8;
        }
        if next > 3600.0 || presses.len() > 128 { return Err("sequence too long".into()); }
    }
    if presses.is_empty() { return Err("empty sequence".into()); }
    Ok((presses, last.map_or(next, |t| t + HOLD)))
}

impl Engine {
    pub fn parse(text: &str, mut reference: impl FnMut(&Path) -> Result<Thumbnail, String>) -> Result<(Self, Vec<(String, String)>), String> {
        let config: Config = toml::from_str(text).map_err(|e| e.to_string())?;
        if config.rules.is_empty() || config.rules.len() > 32 { return Err("expected 1..32 rules".into()); }
        if let Some(f) = &config.fallback {
            if super::gameinput::button_named(&f.button).is_none() || !f.start.is_finite() || f.start < 0.0
                || !f.interval.is_finite() || f.interval <= HOLD { return Err("invalid fallback button/start/interval".into()); }
        }
        let (mut rules, mut warnings, mut names) = (Vec::new(), Vec::new(), std::collections::HashSet::new());
        for s in config.rules {
            if s.name.is_empty() || s.name.len() > 128 || !names.insert(s.name.clone()) || !valid_region(s.region)
                || !s.threshold.is_finite() || !(0.0..=255.0).contains(&s.threshold) || !s.cooldown.is_finite() || s.cooldown < HOLD {
                return Err(format!("invalid or duplicate rule {:?}", s.name));
            }
            let (presses, end) = sequence(&s.sequence).map_err(|e| format!("rule {:?}: {e}", s.name))?;
            match reference(&s.reference) {
                Ok(reference) => rules.push(Rule { spec: s, reference, presses, end, last: None }),
                Err(e) => warnings.push((s.name, e)),
            }
        }
        let fallback = if warnings.is_empty() { config.fallback } else { None };
        Ok((Self { rules, fallback, fallback_press: None, playing: None, claimed: false, last_frame: None }, warnings))
    }
    pub fn frame(&mut self, image: &Thumbnail, frame: u64, t: f64) -> Option<Event> {
        if !t.is_finite() || self.last_frame.is_some_and(|last| frame <= last) { return None; }
        self.last_frame = Some(frame);
        if self.playing.is_some() { return None; }
        for (index, r) in self.rules.iter_mut().enumerate() {
            if r.last.is_some_and(|last| r.spec.once || t - last < r.spec.cooldown) { continue; }
            let diff = difference(image, &r.reference, r.spec.region);
            if diff <= r.spec.threshold {
                r.last = Some(t);
                self.claimed = true;
                self.playing = Some(Playing { rule: index, start: t, frame, pressed: None });
                return Some(Event { event: "matched", rule: r.spec.name.clone(), frame, t, diff: Some(diff), button: None });
            }
        }
        None
    }
    pub fn buttons(&mut self, t: f64) -> (u32, Option<Event>) {
        if !t.is_finite() { return (0, None); }
        if let Some(p) = &mut self.playing {
            let r = &self.rules[p.rule];
            let elapsed = t - p.start;
            let event = |event, button| Event { event, rule: r.spec.name.clone(), frame: p.frame, t, diff: None, button };
            if elapsed >= r.end {
                let e = event("completed", None);
                self.playing = None;
                return (0, Some(e));
            }
            if let Some((i, (_, button, name))) = r.presses.iter().enumerate().find(|(_, (at, _, _))| elapsed >= *at && elapsed < *at + HOLD) {
                let e = (p.pressed != Some(i)).then(|| event("pressed", Some(name.clone())));
                p.pressed = Some(i);
                return (*button, e);
            }
            return (0, None);
        }
        if let Some(f) = self.fallback.as_ref().filter(|f| !self.claimed && t >= f.start) {
            let elapsed = t - f.start;
            if elapsed % f.interval < HOLD {
                let n = (elapsed / f.interval).floor() as u64;
                let e = (self.fallback_press != Some(n)).then(|| Event { event: "pressed", rule: "$fallback".into(), frame: self.last_frame.unwrap_or(0), t, diff: None, button: Some(f.button.clone()) });
                self.fallback_press = Some(n);
                return (super::gameinput::button_named(&f.button).unwrap_or(0), e);
            }
        }
        (0, None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    const RULE: &str = "[[rules]]\nname='menu'\nreference='local.bmp'\nregion=[0.0,0.0,0.5,0.5]\nthreshold=5\nsequence='DOWN, wait 5, A'\n";
    fn engine(text: &str) -> Engine { Engine::parse(text, |_| Ok([0; WIDTH * HEIGHT])).unwrap().0 }
    fn bmp(top_down: bool) -> Vec<u8> {
        let mut b = vec![0; 70];
        b[..2].copy_from_slice(b"BM");
        for (at, v) in [(10, 54u32), (14, 40), (18, 2), (22, if top_down { (-2i32) as u32 } else { 2 })] {
            b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        }
        b[26..28].copy_from_slice(&1u16.to_le_bytes());
        b[28..30].copy_from_slice(&24u16.to_le_bytes());
        let (top, bottom) = if top_down { (54, 62) } else { (62, 54) };
        b[top..top + 6].fill(30);
        b[bottom..bottom + 6].fill(210);
        b
    }
    #[test]
    fn synthetic_bmp_orientation_padding_and_bad_headers() {
        let t = thumbnail(&bmp(false)).unwrap();
        assert_eq!(t, thumbnail(&bmp(true)).unwrap());
        assert_eq!((t[0], t[WIDTH * HEIGHT - 1]), (30, 210));
        assert!(thumbnail(&bmp(false)[..60]).is_err());
        let mut b = bmp(false); b[30] = 1;
        assert!(thumbnail(&b).is_err(), "compressed BMP cannot trigger input");
        b[30] = 0; b[10] = 0;
        assert!(thumbnail(&b).is_err(), "pixel data cannot overlap the header");
    }
    #[test]
    fn identical_different_and_region_limited_frames() {
        let a = [0; WIDTH * HEIGHT]; let mut b = a;
        assert_eq!(difference(&a, &b, [0.0, 0.0, 0.5, 0.5]), 0.0);
        b[WIDTH * HEIGHT - 1] = 255;
        assert_eq!(difference(&a, &b, [0.0, 0.0, 0.5, 0.5]), 0.0);
        b.fill(30);
        assert_eq!(difference(&a, &b, [0.0, 0.0, 0.5, 0.5]), 30.0);
    }
    #[test]
    fn parser_rejects_unknown_fields_bad_regions_times_and_buttons() {
        for bad in [RULE.replace("threshold=5", "threshold=nan"), RULE.replace("0.5,0.5", "1.1,0.5"),
            RULE.replace("DOWN, wait 5, A", "NOPE"), RULE.replace("wait 5", "wait -1"),
            format!("{RULE}typo=true\n"), format!("{RULE}{RULE}")] {
            assert!(Engine::parse(&bad, |_| Ok([0; WIDTH * HEIGHT])).is_err(), "{bad}");
        }
    }
    #[test]
    fn missing_reference_disables_rule_and_fallback_without_pressing() {
        let text = format!("[fallback]\nbutton='A'\nstart=0\ninterval=3\n{RULE}");
        let (mut e, warnings) = Engine::parse(&text, |p| { assert_eq!(p, Path::new("local.bmp")); Err("missing".into()) }).unwrap();
        assert_eq!(warnings.len(), 1);
        assert!(e.frame(&[0; WIDTH * HEIGHT], 1, 0.0).is_none());
        assert_eq!(e.buttons(0.0).0, 0);
    }
    #[test]
    fn sequence_timing_once_and_fallback_handoff() {
        let mut e = engine(&format!("[fallback]\nbutton='A'\nstart=57\ninterval=3\n{RULE}"));
        assert_eq!(e.buttons(57.0).0, super::super::gameinput::A);
        assert_eq!(e.buttons(57.3).0, 0);
        assert_eq!(e.frame(&[0; WIDTH * HEIGHT], 20, 58.0).unwrap().event, "matched");
        assert_eq!(e.buttons(58.0).0, super::super::gameinput::DPAD_DOWN);
        assert_eq!(e.buttons(58.3).0, 0);
        assert_eq!(e.buttons(60.0).0, 0, "fallback stopped before the sequence");
        assert_eq!(e.buttons(63.0).0, super::super::gameinput::A);
        assert_eq!(e.buttons(63.3).1.unwrap().event, "completed");
        assert!(e.frame(&[0; WIDTH * HEIGHT], 21, 64.0).is_none(), "once");
    }
    #[test]
    fn repeated_rules_need_a_new_frame_cooldown_and_no_active_sequence() {
        let text = RULE.replace("sequence='DOWN, wait 5, A'", "sequence='A'\nonce=false\ncooldown=3");
        let mut e = engine(&text);
        assert!(e.frame(&[0; WIDTH * HEIGHT], 5, 0.0).is_some());
        assert!(e.frame(&[0; WIDTH * HEIGHT], 6, 0.1).is_none());
        e.buttons(0.3);
        assert!(e.frame(&[0; WIDTH * HEIGHT], 7, 2.9).is_none());
        assert!(e.frame(&[0; WIDTH * HEIGHT], 7, 3.1).is_none(), "same sampled frame");
        assert!(e.frame(&[0; WIDTH * HEIGHT], 8, 3.1).is_some());
    }
    #[test]
    fn multi_step_sequence_and_events_are_json() {
        let mut e = engine(&RULE.replace("DOWN, wait 5, A", "DOWN, wait 5, DOWN, A, wait 1.8, A, DOWN, A, DOWN, A"));
        assert!(e.frame(&[20; WIDTH * HEIGHT], 1, 57.0).is_none(), "different screen");
        let event = e.frame(&[0; WIDTH * HEIGHT], 2, 58.0).unwrap();
        let json: serde_json::Value = serde_json::from_str(&serde_json::to_string(&event).unwrap()).unwrap();
        assert_eq!(json["event"], "matched");
        assert_eq!(json["rule"], "menu");
        assert_eq!(e.buttons(58.0).1.unwrap().event, "pressed");
        assert!(e.buttons(58.1).1.is_none(), "one event per pulse");
        for (t, button) in [(63.0, "DOWN"), (63.8, "A"), (65.6, "A"), (66.4, "DOWN"),
            (67.2, "A"), (68.0, "DOWN"), (68.8, "A")] {
            assert_eq!(e.buttons(t + 0.01).1.unwrap().button.as_deref(), Some(button));
        }
        assert_eq!(e.buttons(69.1).1.unwrap().event, "completed");
        assert_eq!(e.buttons(72.0).0, 0, "fallback stays stopped");
    }
}
