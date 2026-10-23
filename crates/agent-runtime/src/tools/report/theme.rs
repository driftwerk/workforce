//! Named house themes. Blocks reference semantic accent tokens, never raw hex,
//! so the theme owns every colour and font; swapping themes restyles the whole
//! document. Fonts must match the bundled family names in `fonts.rs`.
//!
//! `cover_bg` is the cover/header background; `primary` is a foreground colour
//! (titles, headers). Light themes set them equal; dark themes diverge them so
//! the cover stays dark while text stays light.
//!
//! Themes are owned (`String`) rather than `&'static str` so brand themes loaded
//! from workspace config can be constructed at render time and flow through the
//! same path as the built-ins.

use once_cell::sync::Lazy;

#[derive(Clone)]
pub struct Theme {
    pub name: String,
    pub sans: String,
    pub serif: String,
    pub palette: Palette,
}

#[derive(Clone)]
pub struct Palette {
    pub cover_bg: String,
    pub primary: String,
    pub secondary: String,
    pub accent: String,
    pub ink: String,
    pub paper: String,
    pub panel: String,
    pub muted: String,
    pub hairline: String,
    pub on_dark: String,
    pub series: Vec<String>,
}

/// All-`&'static str` so the built-ins stay const data; converted to owned
/// `Theme`s once in `BUILTINS`.
struct Spec {
    name: &'static str,
    sans: &'static str,
    serif: &'static str,
    cover_bg: &'static str,
    primary: &'static str,
    secondary: &'static str,
    accent: &'static str,
    ink: &'static str,
    paper: &'static str,
    panel: &'static str,
    muted: &'static str,
    hairline: &'static str,
    on_dark: &'static str,
    series: &'static [&'static str],
}

const SPECS: &[Spec] = &[
    Spec {
        name: "espresso",
        sans: "Lato",
        serif: "IBM Plex Serif",
        cover_bg: "#2B1B12",
        primary: "#2B1B12",
        secondary: "#6F4E37",
        accent: "#C8964F",
        ink: "#2B1B12",
        paper: "#FBF7F1",
        panel: "#FFFFFF",
        muted: "#9A8876",
        hairline: "#E7DCCD",
        on_dark: "#EDE3D7",
        series: &[
            "#6F4E37", "#C8964F", "#2B1B12", "#2A9D8F", "#B9A088", "#8C6A9E", "#D9534F", "#5B7F3C",
        ],
    },
    Spec {
        name: "slate",
        sans: "Lato",
        serif: "IBM Plex Serif",
        cover_bg: "#1F2A37",
        primary: "#1F2A37",
        secondary: "#3B5266",
        accent: "#2F7FB5",
        ink: "#1F2A37",
        paper: "#F7F9FB",
        panel: "#FFFFFF",
        muted: "#7A8896",
        hairline: "#DFE6EC",
        on_dark: "#DCE6EF",
        series: &[
            "#3B5266", "#2F7FB5", "#1F2A37", "#3FA796", "#9AAAB8", "#D08A4A", "#7B61B8", "#C25B7A",
        ],
    },
    Spec {
        name: "noir",
        sans: "Lato",
        serif: "IBM Plex Serif",
        cover_bg: "#12161C",
        primary: "#EDEFF2",
        secondary: "#8B97A6",
        accent: "#E2B341",
        ink: "#E6E9ED",
        paper: "#161B22",
        panel: "#1E242C",
        muted: "#7E8895",
        hairline: "#2C333D",
        on_dark: "#E6E9ED",
        series: &[
            "#E2B341", "#5BA8D6", "#5FBF8E", "#C98BDB", "#E0856A", "#F08BA0", "#9ED37B", "#8FA3E8",
        ],
    },
    Spec {
        name: "editorial",
        sans: "Lato",
        serif: "IBM Plex Serif",
        cover_bg: "#2B2622",
        primary: "#2B2622",
        secondary: "#6B5D4F",
        accent: "#B0552F",
        ink: "#2B2622",
        paper: "#FBF8F3",
        panel: "#FFFFFF",
        muted: "#8A7E70",
        hairline: "#E6DFD4",
        on_dark: "#EDE6DB",
        series: &[
            "#6B5D4F", "#B0552F", "#2B2622", "#4E7C6B", "#B89B78", "#8C3F6E", "#3E6E9A", "#C9A227",
        ],
    },
    Spec {
        name: "mint",
        sans: "Lato",
        serif: "IBM Plex Serif",
        cover_bg: "#15302A",
        primary: "#15302A",
        secondary: "#3E7363",
        accent: "#2FB186",
        ink: "#18302A",
        paper: "#F4FAF7",
        panel: "#FFFFFF",
        muted: "#7C9189",
        hairline: "#DCEBE5",
        on_dark: "#DCEFE8",
        series: &[
            "#3E7363", "#2FB186", "#15302A", "#6CC0A0", "#9FB8AE", "#E09A3C", "#6F72C9", "#C65C6F",
        ],
    },
    Spec {
        name: "sunset",
        sans: "Lato",
        serif: "IBM Plex Serif",
        cover_bg: "#3A1E22",
        primary: "#3A1E22",
        secondary: "#9C4A4A",
        accent: "#E8743B",
        ink: "#3A1E22",
        paper: "#FDF6F2",
        panel: "#FFFFFF",
        muted: "#9A8079",
        hairline: "#F0DDD3",
        on_dark: "#F4E2D8",
        series: &[
            "#9C4A4A", "#E8743B", "#3A1E22", "#E0A458", "#C98B86", "#5E8FBF", "#4C9E8A", "#8A6FBF",
        ],
    },
];

impl Theme {
    fn from_spec(s: &Spec) -> Theme {
        Theme {
            name: s.name.into(),
            sans: s.sans.into(),
            serif: s.serif.into(),
            palette: Palette {
                cover_bg: s.cover_bg.into(),
                primary: s.primary.into(),
                secondary: s.secondary.into(),
                accent: s.accent.into(),
                ink: s.ink.into(),
                paper: s.paper.into(),
                panel: s.panel.into(),
                muted: s.muted.into(),
                hairline: s.hairline.into(),
                on_dark: s.on_dark.into(),
                series: s.series.iter().map(|c| c.to_string()).collect(),
            },
        }
    }
}

static BUILTINS: Lazy<Vec<Theme>> = Lazy::new(|| SPECS.iter().map(Theme::from_spec).collect());

/// A built-in theme by name (case-insensitive), if one exists.
pub fn builtin(name: &str) -> Option<&'static Theme> {
    BUILTINS.iter().find(|t| t.name.eq_ignore_ascii_case(name))
}

/// The default theme (`espresso`).
pub fn default_theme() -> &'static Theme {
    &BUILTINS[0]
}

/// Resolve a built-in by name, falling back to the default for an unknown or
/// absent name. (Brand themes are resolved by the tool, which has the config.)
pub fn resolve(name: Option<&str>) -> &'static Theme {
    name.map(str::trim)
        .filter(|n| !n.is_empty())
        .and_then(builtin)
        .unwrap_or_else(default_theme)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_known_themes_case_insensitively() {
        for name in [
            "slate",
            "SLATE",
            "espresso",
            "noir",
            "mint",
            "sunset",
            "editorial",
        ] {
            assert_eq!(resolve(Some(name)).name, name.to_ascii_lowercase());
        }
    }

    #[test]
    fn falls_back_to_default_for_unknown_or_absent() {
        assert_eq!(resolve(None).name, "espresso");
        assert_eq!(resolve(Some("")).name, "espresso");
        assert_eq!(resolve(Some("neon")).name, "espresso");
    }

    #[test]
    fn every_palette_colour_is_a_six_digit_hex() {
        let is_hex = |c: &str| {
            c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|d| d.is_ascii_hexdigit())
        };
        for theme in BUILTINS.iter() {
            let p = &theme.palette;
            for c in [
                &p.cover_bg,
                &p.primary,
                &p.secondary,
                &p.accent,
                &p.ink,
                &p.paper,
                &p.panel,
                &p.muted,
                &p.hairline,
                &p.on_dark,
            ] {
                assert!(is_hex(c), "{} has bad colour {c}", theme.name);
            }
            assert_eq!(
                p.series.len(),
                8,
                "{} needs one colour per chart series",
                theme.name
            );
            let distinct: std::collections::BTreeSet<&str> =
                p.series.iter().map(|c| c.as_str()).collect();
            assert_eq!(distinct.len(), 8, "{} series colours repeat", theme.name);
            assert!(
                p.series.iter().all(|c| is_hex(c)),
                "{} series not all hex",
                theme.name
            );
        }
    }
}
