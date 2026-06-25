//! System appearance probes behind the `host.capabilities` seam: the OS color scheme
//! (light/dark) and accent color, normalized to hex + a contrasting ink color. Pure
//! platform dispatch — each probe degrades to `None` rather than panicking, so the
//! host capability surface stays platform-agnostic.

use super::native::{program_available, windows_dialog_program};
use std::process::{Command, Stdio};

pub(super) fn host_appearance() -> (Option<String>, Option<String>, Option<String>) {
    let scheme = system_color_scheme();
    let accent = system_accent_color();
    let accent_ink = accent.as_deref().and_then(accent_ink_color);
    (scheme, accent, accent_ink)
}

fn system_color_scheme() -> Option<String> {
    if cfg!(target_os = "macos") {
        return macos_color_scheme();
    }
    if cfg!(target_os = "windows") {
        return windows_color_scheme();
    }
    if cfg!(target_os = "linux") {
        return linux_color_scheme();
    }
    None
}

fn system_accent_color() -> Option<String> {
    if cfg!(target_os = "macos") {
        return macos_accent_color();
    }
    if cfg!(target_os = "windows") {
        return windows_accent_color();
    }
    if cfg!(target_os = "linux") {
        return linux_accent_color();
    }
    None
}

fn macos_color_scheme() -> Option<String> {
    if !program_available("defaults") {
        return None;
    }
    let output = Command::new("defaults")
        .args(["read", "-g", "AppleInterfaceStyle"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let value = String::from_utf8_lossy(&output.stdout);
    if output.status.success() && value.trim().eq_ignore_ascii_case("Dark") {
        Some("dark".into())
    } else {
        Some("light".into())
    }
}

fn macos_accent_color() -> Option<String> {
    let value = command_stdout("defaults", &["read", "-g", "AppleAccentColor"])?;
    match value.trim().parse::<i32>().ok()? {
        0 => Some("#ff3b30".into()),
        1 => Some("#ff9500".into()),
        2 => Some("#ffcc00".into()),
        3 => Some("#34c759".into()),
        4 => Some("#007aff".into()),
        5 => Some("#af52de".into()),
        6 => Some("#ff2d55".into()),
        _ => None,
    }
}

fn windows_color_scheme() -> Option<String> {
    let program = windows_dialog_program()?;
    let value = command_stdout(
        program,
        &[
            "-NoProfile",
            "-Command",
            "(Get-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize').AppsUseLightTheme",
        ],
    )?;
    match value.trim() {
        "0" => Some("dark".into()),
        "1" => Some("light".into()),
        _ => None,
    }
}

fn windows_accent_color() -> Option<String> {
    let program = windows_dialog_program()?;
    let value = command_stdout(
        program,
        &[
            "-NoProfile",
            "-Command",
            "(Get-ItemProperty -Path 'HKCU:\\Software\\Microsoft\\Windows\\DWM').AccentColor",
        ],
    )?;
    let raw = value.trim().parse::<i64>().ok()? as u32;
    Some(format!(
        "#{:02x}{:02x}{:02x}",
        raw & 0xff,
        (raw >> 8) & 0xff,
        (raw >> 16) & 0xff
    ))
}

fn linux_color_scheme() -> Option<String> {
    let value = command_stdout(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "color-scheme"],
    )?;
    if value.contains("dark") {
        Some("dark".into())
    } else if value.contains("light") {
        Some("light".into())
    } else {
        None
    }
}

fn linux_accent_color() -> Option<String> {
    let value = command_stdout(
        "gsettings",
        &["get", "org.gnome.desktop.interface", "accent-color"],
    )?;
    match value.trim_matches(['\'', '"', ' ']) {
        "blue" => Some("#3584e4".into()),
        "teal" => Some("#2190a4".into()),
        "green" => Some("#3a944a".into()),
        "yellow" => Some("#c88800".into()),
        "orange" => Some("#ed5b00".into()),
        "red" => Some("#e62d42".into()),
        "pink" => Some("#d56199".into()),
        "purple" => Some("#9141ac".into()),
        "slate" => Some("#6f8396".into()),
        _ => None,
    }
}

fn command_stdout(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    (!text.is_empty()).then_some(text)
}

fn accent_ink_color(color: &str) -> Option<String> {
    let (r, g, b) = parse_hex_color(color)?;
    let luminance = u32::from(r) * 299 + u32::from(g) * 587 + u32::from(b) * 114;
    Some(
        if luminance > 150_000 {
            "#111111"
        } else {
            "#ffffff"
        }
        .into(),
    )
}

fn parse_hex_color(color: &str) -> Option<(u8, u8, u8)> {
    let hex = color.trim().strip_prefix('#')?;
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some((r, g, b))
}
