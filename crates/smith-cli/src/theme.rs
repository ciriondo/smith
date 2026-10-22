//! Terminal colour roles and the theme rules.
//!
//! The terminal follows hax: semantic roles, the `auto` detection from `NO_COLOR`, `TERM`,
//! `COLORTERM` and `COLORFGBG`, and the `dark`, `light`, `ansi` and `off` presets. The palette
//! values are provisional until the colour review; the roles and the detection do not depend on
//! them.

use smith_core::config::ThemeName;

/// A semantic style. The palette values are per theme; the renderers ask for the role.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Accent,
    Chrome,
    Dim,
    Heading,
    Code,
    CodeBlock,
    Link,
    Add,
    Remove,
    Ok,
    Warn,
    Error,
}

/// The environment inputs the automatic selection reads. Keeping them as a value makes the
/// rules testable without touching the process environment.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ThemeEnv {
    pub no_color: Option<String>,
    pub term: Option<String>,
    pub colorterm: Option<String>,
    pub color_fgbg: Option<String>,
}

impl ThemeEnv {
    pub fn from_env() -> Self {
        Self {
            no_color: std::env::var("NO_COLOR").ok(),
            term: std::env::var("TERM").ok(),
            colorterm: std::env::var("COLORTERM").ok(),
            color_fgbg: std::env::var("COLORFGBG").ok(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThemeKind {
    Dark,
    Light,
    Ansi,
    Off,
}

/// The active terminal theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Theme {
    kind: ThemeKind,
}

impl Theme {
    /// Resolve the configured name. `auto` reads the environment.
    pub fn from_setting(name: ThemeName) -> Self {
        select(name, &ThemeEnv::from_env())
    }

    /// Build a theme directly. Only tests need this; the CLI resolves `ThemeName` through
    /// [`Theme::from_setting`].
    #[cfg(test)]
    pub fn from_name(name: ThemeName) -> Self {
        Self {
            kind: match name {
                ThemeName::Auto => ThemeKind::Dark,
                ThemeName::Dark => ThemeKind::Dark,
                ThemeName::Light => ThemeKind::Light,
                ThemeName::Ansi => ThemeKind::Ansi,
                ThemeName::Off => ThemeKind::Off,
            },
        }
    }

    #[cfg(test)]
    pub fn off() -> Self {
        Self {
            kind: ThemeKind::Off,
        }
    }

    pub fn open(self, role: Role) -> &'static str {
        style(self.kind, role).0
    }

    pub fn close(self, role: Role) -> &'static str {
        style(self.kind, role).1
    }
}

/// Resolve a configured theme name to a preset.
pub fn select(setting: ThemeName, env: &ThemeEnv) -> Theme {
    let kind = match setting {
        ThemeName::Auto => auto(env),
        ThemeName::Dark => ThemeKind::Dark,
        ThemeName::Light => ThemeKind::Light,
        ThemeName::Ansi => ThemeKind::Ansi,
        ThemeName::Off => ThemeKind::Off,
    };
    Theme { kind }
}

/// The hax automatic rules.
fn auto(env: &ThemeEnv) -> ThemeKind {
    if env
        .no_color
        .as_deref()
        .is_some_and(|value| !value.is_empty())
    {
        return ThemeKind::Off;
    }
    let term = env.term.as_deref().unwrap_or("");
    if term.is_empty() || term == "dumb" {
        return ThemeKind::Off;
    }
    let colorterm = env.colorterm.as_deref().unwrap_or("");
    if !term.contains("256color") && colorterm.is_empty() {
        return ThemeKind::Ansi;
    }
    let light_background = env
        .color_fgbg
        .as_deref()
        .and_then(|value| value.rsplit(';').next())
        .is_some_and(|background| background == "7" || background == "15");
    if light_background {
        ThemeKind::Light
    } else {
        ThemeKind::Dark
    }
}

const RESET: &str = "\x1b[0m";
const BOLD: &str = "\x1b[1m";
const BOLD_OFF: &str = "\x1b[22m";
const DIM: &str = "\x1b[2m";
const UNDERLINE: &str = "\x1b[4m";
const UNDERLINE_OFF: &str = "\x1b[24m";

fn style(kind: ThemeKind, role: Role) -> (&'static str, &'static str) {
    use Role::*;
    use ThemeKind::*;
    match (kind, role) {
        (Off, Heading) => (BOLD, BOLD_OFF),
        (Off, CodeBlock) => (DIM, BOLD_OFF),
        (Off, Link) => (UNDERLINE, UNDERLINE_OFF),
        (Off, Dim) => (DIM, RESET),
        (Off, _) => ("", ""),
        (Ansi, Accent) => ("\x1b[94m", RESET),
        (Ansi, Chrome) => ("\x1b[36m", RESET),
        (Ansi, Dim) => ("\x1b[2m\x1b[36m", RESET),
        (Ansi, Heading) => ("\x1b[1m", BOLD_OFF),
        (Ansi, Code) => ("\x1b[36m", RESET),
        (Ansi, CodeBlock) => ("\x1b[2m", BOLD_OFF),
        (Ansi, Link) => (UNDERLINE, UNDERLINE_OFF),
        (Ansi, Add) | (Ansi, Ok) => ("\x1b[32m", RESET),
        (Ansi, Remove) | (Ansi, Error) => ("\x1b[31m", RESET),
        (Ansi, Warn) => ("\x1b[33m", RESET),
        (Dark, Accent) => ("\x1b[38;5;111m", RESET),
        (Dark, Chrome) => ("\x1b[38;5;117m", RESET),
        (Dark, Dim) => ("\x1b[2m\x1b[38;5;245m", RESET),
        (Dark, Heading) => ("\x1b[1m\x1b[38;5;141m", RESET),
        (Dark, Code) => ("\x1b[38;5;117m", RESET),
        (Dark, CodeBlock) => ("\x1b[38;5;110m", RESET),
        (Dark, Link) => ("\x1b[4m\x1b[38;5;117m", RESET),
        (Dark, Add) | (Dark, Ok) => ("\x1b[38;5;114m", RESET),
        (Dark, Remove) | (Dark, Error) => ("\x1b[38;5;204m", RESET),
        (Dark, Warn) => ("\x1b[38;5;179m", RESET),
        (Light, Accent) => ("\x1b[38;5;26m", RESET),
        (Light, Chrome) => ("\x1b[38;5;30m", RESET),
        (Light, Dim) => ("\x1b[2m\x1b[38;5;243m", RESET),
        (Light, Heading) => ("\x1b[1m\x1b[38;5;91m", RESET),
        (Light, Code) => ("\x1b[38;5;30m", RESET),
        (Light, CodeBlock) => ("\x1b[38;5;66m", RESET),
        (Light, Link) => ("\x1b[4m\x1b[38;5;30m", RESET),
        (Light, Add) | (Light, Ok) => ("\x1b[38;5;28m", RESET),
        (Light, Remove) | (Light, Error) => ("\x1b[38;5;160m", RESET),
        (Light, Warn) => ("\x1b[38;5;136m", RESET),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(
        no_color: Option<&str>,
        term: Option<&str>,
        colorterm: Option<&str>,
        color_fgbg: Option<&str>,
    ) -> ThemeEnv {
        ThemeEnv {
            no_color: no_color.map(str::to_string),
            term: term.map(str::to_string),
            colorterm: colorterm.map(str::to_string),
            color_fgbg: color_fgbg.map(str::to_string),
        }
    }

    fn selected(setting: ThemeName, env: &ThemeEnv) -> Theme {
        select(setting, env)
    }

    #[test]
    fn auto_follows_the_hax_rules() {
        let off = Theme::off();
        assert_eq!(
            selected(
                ThemeName::Auto,
                &env(Some("1"), Some("xterm-256color"), None, None)
            ),
            off
        );
        assert_eq!(
            selected(ThemeName::Auto, &env(None, Some("dumb"), None, None)),
            off
        );
        assert_eq!(selected(ThemeName::Auto, &env(None, None, None, None)), off);
        assert_eq!(
            selected(
                ThemeName::Auto,
                &env(None, Some("xterm"), None, Some("15;0"))
            ),
            Theme {
                kind: ThemeKind::Ansi
            }
        );
        assert_eq!(
            selected(
                ThemeName::Auto,
                &env(None, Some("xterm-256color"), None, None)
            ),
            Theme {
                kind: ThemeKind::Dark
            }
        );
        assert_eq!(
            selected(
                ThemeName::Auto,
                &env(None, Some("screen-256color"), None, Some("15;7"))
            ),
            Theme {
                kind: ThemeKind::Light
            }
        );
        assert_eq!(
            selected(
                ThemeName::Auto,
                &env(None, Some("xterm"), Some("truecolor"), Some("0;15"))
            ),
            Theme {
                kind: ThemeKind::Light
            }
        );
    }

    #[test]
    fn an_explicit_setting_skips_the_environment() {
        let env = env(Some("1"), Some("dumb"), None, None);
        for (name, kind) in [
            (ThemeName::Dark, ThemeKind::Dark),
            (ThemeName::Light, ThemeKind::Light),
            (ThemeName::Ansi, ThemeKind::Ansi),
            (ThemeName::Off, ThemeKind::Off),
        ] {
            assert_eq!(selected(name, &env), Theme { kind });
        }
    }

    #[test]
    fn off_keeps_attributes_but_no_colour() {
        let theme = Theme::off();
        assert_eq!(theme.open(Role::Heading), BOLD);
        assert_eq!(theme.open(Role::CodeBlock), DIM);
        assert_eq!(theme.open(Role::Link), UNDERLINE);
        assert_eq!(theme.open(Role::Dim), DIM);
        assert_eq!(theme.open(Role::Accent), "");
        assert_eq!(theme.open(Role::Error), "");
        assert_eq!(theme.close(Role::Link), UNDERLINE_OFF);
    }

    #[test]
    fn the_dark_and_light_palettes_set_the_roles() {
        let dark = Theme::from_name(ThemeName::Dark);
        assert_eq!(dark.open(Role::Accent), "\x1b[38;5;111m");
        assert_eq!(dark.open(Role::Remove), "\x1b[38;5;204m");
        let light = Theme::from_name(ThemeName::Light);
        assert_eq!(light.open(Role::Accent), "\x1b[38;5;26m");
        assert_eq!(light.open(Role::Add), "\x1b[38;5;28m");
        let ansi = Theme::from_name(ThemeName::Ansi);
        assert_eq!(ansi.open(Role::Accent), "\x1b[94m");
        assert_eq!(ansi.open(Role::Heading), BOLD);
    }
}
