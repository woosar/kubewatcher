use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::Block;


/// Built-in themes, written to the themes directory on first start so they can be edited.
const BUILTIN: &[(&str, &str)] = &[
    ("default", DEFAULT_THEME),
    ("violet", VIOLET_THEME),
    ("osaka-jade", OSAKA_JADE_THEME),
];
const DEFAULT_NAME: &str = "default";

const DEFAULT_THEME: &str = r##"# kubewatcher theme: default
# Uses your terminal's own 16-color palette, so it follows your terminal theme.
#
# Every key is optional; missing keys fall back to these defaults.
# Values: "#rrggbb", a terminal color name (black, red, green, yellow, blue,
# magenta, cyan, gray, dark-gray, light-red, ..., white), a 0-255 palette
# index, or "reset" for the terminal's own default.

background   = "reset"      # page background
surface      = "reset"      # dialogs and popups
text         = "reset"      # normal text
muted        = "dark-gray"  # quiet text: automatic refreshes, strip border
border       = "reset"      # box borders
accent       = "cyan"       # headings, active tab, prompt, help
on_accent    = "black"      # text on an accent-colored fill
secondary    = "magenta"    # output viewer, port-forwards
success      = "green"      # ok, running, current context
warning      = "yellow"     # pending, filters, changes in the journal
danger       = "red"        # errors, delete/uninstall confirmations
info         = "blue"       # cluster icon
chip         = "dark-gray"  # key-hint chips in the bottom bar
on_chip      = "black"
selection    = "reverse"    # selected row; "reverse" swaps text and background
on_selection = "reset"
"##;

const VIOLET_THEME: &str = r##"# kubewatcher theme: violet
# From HomeLab ui/src/styles/themes/violet.css (dark), oklch converted to sRGB.
# Near-black page with blue-violet accents.

background   = "#0e0f15"  # --background   oklch(0.17 0.012 280)
surface      = "#191a22"  # --popover      oklch(0.22 0.017 280)
text         = "#f0f1fa"  # --foreground   oklch(0.96 0.012 284)
muted        = "#a2a3b1"  # --muted-foreground
border       = "#35353a"  # --border, white over the page (strengthened for a terminal)
accent       = "#92a1ff"  # --brand        oklch(0.74 0.15 276)
on_accent    = "#0e0f15"
secondary    = "#d292e1"  # --chart-3      oklch(0.75 0.13 320)
success      = "#63d18f"  # --positive     oklch(0.78 0.14 155)
warning      = "#ffe599"  # --toast-warning-text
danger       = "#fa686a"  # --destructive  oklch(0.7 0.18 22)
info         = "#3689dd"  # --chart-2      oklch(0.62 0.15 252)
chip         = "#6a73fb"  # --primary      oklch(0.62 0.2 276)
on_chip      = "#f7f8ff"  # --primary-foreground
selection    = "#2e324d"  # --brand over the page
on_selection = "#9faeff"  # --secondary-foreground
"##;

const OSAKA_JADE_THEME: &str = r##"# kubewatcher theme: osaka-jade
# From HomeLab ui/src/styles/themes/osaka-jade.css (dark), after the Omarchy theme:
# deep green-black page, cream text, jade accents, soft rose for bad news.

background   = "#111c18"  # --background
surface      = "#15221d"  # --popover
text         = "#c1c497"  # --foreground
muted        = "#8e9677"  # --muted-foreground
border       = "#2a4134"  # --input (a touch brighter than --border, #23372b)
accent       = "#509475"  # --brand / --primary (jade)
on_accent    = "#111c18"  # --primary-foreground
secondary    = "#2dd5b7"  # --chart-2
success      = "#63b07a"  # --positive
warning      = "#e5c736"  # --chart-3
danger       = "#db9f9c"  # --destructive (soft rose)
info         = "#acd4cf"  # --secondary-foreground
chip         = "#32473b"  # --secondary
on_chip      = "#acd4cf"  # --secondary-foreground
selection    = "#32473b"  # --secondary
on_selection = "#acd4cf"
"##;

/// Named colors for every role in the UI, loaded from
/// `~/.config/kubewatcher/themes/<name>.toml`.
#[derive(Clone)]
pub struct Theme {
    pub name: String,
    pub background: Color,
    pub surface: Color,
    pub text: Color,
    pub muted: Color,
    pub border: Color,
    pub accent: Color,
    pub on_accent: Color,
    pub secondary: Color,
    pub success: Color,
    pub warning: Color,
    pub danger: Color,
    pub info: Color,
    pub chip: Color,
    pub on_chip: Color,
    /// None = reverse video.
    pub selection: Option<Color>,
    pub on_selection: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            name: DEFAULT_NAME.into(),
            background: Color::Reset,
            surface: Color::Reset,
            text: Color::Reset,
            muted: Color::DarkGray,
            border: Color::Reset,
            accent: Color::Cyan,
            on_accent: Color::Black,
            secondary: Color::Magenta,
            success: Color::Green,
            warning: Color::Yellow,
            danger: Color::Red,
            info: Color::Blue,
            chip: Color::DarkGray,
            on_chip: Color::Black,
            selection: None,
            on_selection: Color::Reset,
        }
    }
}

impl Theme {
    // ── styles used across the UI ─────────────────────────────────────────

    /// Page background + text.
    pub fn base(&self) -> Style {
        Style::new().bg(self.background).fg(self.text)
    }

    /// Dialogs and popups (drawn after `Clear`, so they need their own background).
    pub fn popup(&self) -> Style {
        Style::new().bg(self.surface).fg(self.text)
    }

    pub fn border(&self) -> Style {
        Style::new().fg(self.border)
    }

    /// A bordered box in the theme's border color.
    pub fn block(&self) -> Block<'static> {
        Block::bordered().border_style(self.border())
    }

    pub fn fg(&self, color: Color) -> Style {
        Style::new().fg(color)
    }

    /// Column headers.
    pub fn header(&self) -> Style {
        Style::new().fg(self.accent).add_modifier(Modifier::BOLD)
    }

    pub fn selected(&self) -> Style {
        match self.selection {
            None => Style::new().add_modifier(Modifier::REVERSED),
            Some(bg) => Style::new().bg(bg).fg(self.on_selection).add_modifier(Modifier::BOLD),
        }
    }

    /// Key-hint chips (" q ").
    pub fn chip(&self) -> Style {
        Style::new().bg(self.chip).fg(self.on_chip)
    }

    /// A badge filled with `color` (" watch 2s ", " loading… ", the y/e/n keys).
    pub fn badge(&self, color: Color) -> Style {
        Style::new().bg(color).fg(self.on_accent)
    }

    // ── loading ───────────────────────────────────────────────────────────

    /// Loads every theme in `<config_dir>/themes` (normally `~/.config/kubewatcher/themes`),
    /// writing the built-in ones there first if missing. Returns the themes, the index
    /// of the one chosen in `config.toml`, and any problems found in the files.
    pub fn load_all(config_dir: Option<&Path>) -> (Vec<Theme>, usize, Vec<String>) {
        let mut problems = Vec::new();
        let mut themes = Vec::new();

        if let Some(dir) = config_dir.map(|d| d.join("themes")) {
            if fs::create_dir_all(&dir).is_ok() {
                for (name, content) in BUILTIN {
                    let path = dir.join(format!("{name}.toml"));
                    if !path.exists() {
                        let _ = fs::write(&path, content);
                    }
                }
            }
            let mut paths: Vec<PathBuf> = fs::read_dir(&dir)
                .map(|entries| entries.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default();
            paths.retain(|p| p.extension().is_some_and(|e| e == "toml"));
            paths.sort();
            for path in paths {
                let name = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                match fs::read_to_string(&path) {
                    Ok(text) => {
                        let (theme, mut issues) = Self::parse(&name, &text);
                        problems.extend(issues.drain(..).map(|i| format!("{}: {i}", path.display())));
                        themes.push(theme);
                    }
                    Err(e) => problems.push(format!("{}: {e}", path.display())),
                }
            }
        }
        // No usable config directory: fall back to the built-ins in memory.
        if themes.is_empty() {
            themes = BUILTIN.iter().map(|(name, text)| Self::parse(name, text).0).collect();
        }
        // Default first, the rest alphabetically.
        themes.sort_by_key(|t| (t.name != DEFAULT_NAME, t.name.clone()));

        let chosen = config_dir
            .and_then(|dir| fs::read_to_string(dir.join("config.toml")).ok())
            .and_then(|text| Self::pairs(&text).find(|(k, _)| *k == "theme").map(|(_, v)| v.to_string()));
        let index = match chosen {
            Some(name) => themes.iter().position(|t| t.name == name).unwrap_or_else(|| {
                problems.push(format!("config.toml: theme \"{name}\" not found"));
                0
            }),
            None => 0,
        };
        (themes, index, problems)
    }

    /// Remembers the chosen theme in `<config_dir>/config.toml`.
    pub fn save_choice(config_dir: Option<&Path>, name: &str) -> Result<PathBuf, String> {
        let dir = config_dir.ok_or("no home directory")?;
        let path = dir.join("config.toml");
        let existing = fs::read_to_string(&path).unwrap_or_default();
        let mut lines: Vec<String> = existing
            .lines()
            .filter(|l| !l.trim_start().starts_with("theme"))
            .map(String::from)
            .collect();
        if lines.is_empty() {
            lines.push("# kubewatcher settings".into());
        }
        lines.push(format!("theme = \"{name}\""));
        fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        fs::write(&path, lines.join("\n") + "\n").map_err(|e| e.to_string())?;
        Ok(path)
    }

    /// `key = "value"` pairs of a flat TOML file, ignoring comments.
    fn pairs(text: &str) -> impl Iterator<Item = (&str, &str)> {
        text.lines().filter_map(|line| {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let value = value.trim();
            // Strip a trailing comment after the closing quote.
            let value = match value.strip_prefix('"') {
                Some(rest) => rest.split_once('"').map_or(rest, |(v, _)| v),
                None => value.split('#').next().unwrap_or(value).trim(),
            };
            Some((key.trim(), value))
        })
    }

    fn parse(name: &str, text: &str) -> (Theme, Vec<String>) {
        let mut theme = Theme { name: name.to_string(), ..Theme::default() };
        let mut problems = Vec::new();
        for (key, value) in Self::pairs(text) {
            if key == "selection" && value == "reverse" {
                theme.selection = None;
                continue;
            }
            let Ok(color) = Color::from_str(value) else {
                problems.push(format!("{key}: \"{value}\" is not a color"));
                continue;
            };
            let slot = match key {
                "background" => &mut theme.background,
                "surface" => &mut theme.surface,
                "text" => &mut theme.text,
                "muted" => &mut theme.muted,
                "border" => &mut theme.border,
                "accent" => &mut theme.accent,
                "on_accent" => &mut theme.on_accent,
                "secondary" => &mut theme.secondary,
                "success" => &mut theme.success,
                "warning" => &mut theme.warning,
                "danger" => &mut theme.danger,
                "info" => &mut theme.info,
                "chip" => &mut theme.chip,
                "on_chip" => &mut theme.on_chip,
                "on_selection" => &mut theme.on_selection,
                "selection" => {
                    theme.selection = Some(color);
                    continue;
                }
                other => {
                    problems.push(format!("unknown key \"{other}\""));
                    continue;
                }
            };
            *slot = color;
        }
        (theme, problems)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_themes_parse_cleanly() {
        for (name, text) in BUILTIN {
            let (theme, problems) = Theme::parse(name, text);
            assert!(problems.is_empty(), "{name}: {problems:?}");
            assert_eq!(theme.name, *name);
        }
        let (jade, _) = Theme::parse("osaka-jade", OSAKA_JADE_THEME);
        assert_eq!(jade.accent, Color::Rgb(0x50, 0x94, 0x75));
        assert_eq!(jade.selection, Some(Color::Rgb(0x32, 0x47, 0x3b)));
        let (default, _) = Theme::parse("default", DEFAULT_THEME);
        assert_eq!(default.selection, None);
        assert_eq!(default.muted, Color::DarkGray);
    }

    #[test]
    fn reports_bad_values_and_keeps_defaults() {
        let (theme, problems) = Theme::parse("mine", "accent = \"#ff0000\"\nborder = \"nope\"\ncolour = \"red\"\n");
        assert_eq!(theme.accent, Color::Rgb(255, 0, 0));
        assert_eq!(theme.border, Color::Reset);
        assert_eq!(problems.len(), 2);

        let (theme, problems) = Theme::parse("loose", "no equals sign\naccent = cyan  # unquoted\ntext = \"#00ff00\n");
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(theme.accent, Color::Cyan);
        assert_eq!(theme.text, Color::Rgb(0, 255, 0));
    }

    #[test]
    fn style_helpers_use_the_theme_colors() {
        let mut theme = Theme::default();
        assert_eq!(theme.base(), Style::new().bg(Color::Reset).fg(Color::Reset));
        assert_eq!(theme.popup(), Style::new().bg(theme.surface).fg(theme.text));
        assert_eq!(theme.border(), Style::new().fg(theme.border));
        assert_eq!(theme.block(), Block::bordered().border_style(theme.border()));
        assert_eq!(theme.header(), Style::new().fg(Color::Cyan).add_modifier(Modifier::BOLD));
        assert_eq!(theme.chip(), Style::new().bg(theme.chip).fg(theme.on_chip));
        assert_eq!(theme.badge(Color::Red), Style::new().bg(Color::Red).fg(theme.on_accent));
        assert_eq!(theme.selected(), Style::new().add_modifier(Modifier::REVERSED));
        theme.selection = Some(Color::Blue);
        assert_eq!(theme.selected(), Style::new().bg(Color::Blue).fg(theme.on_selection).add_modifier(Modifier::BOLD));
    }

    #[test]
    fn loads_seeds_and_remembers_themes() {
        let dir = crate::helpers::test_dir("themes");
        let (themes, index, problems) = Theme::load_all(Some(&dir));
        let names: Vec<&str> = themes.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, ["default", "osaka-jade", "violet"]);
        assert_eq!(index, 0);
        assert!(problems.is_empty(), "{problems:?}");

        // A broken theme file, an unreadable one and a remembered choice.
        fs::write(dir.join("themes").join("mine.toml"), "accent = \"nope\"\n").unwrap();
        fs::create_dir_all(dir.join("themes").join("unreadable.toml")).unwrap();
        let saved = Theme::save_choice(Some(&dir), "violet").unwrap();
        assert_eq!(saved, dir.join("config.toml"));
        Theme::save_choice(Some(&dir), "violet").unwrap();
        assert_eq!(fs::read_to_string(&saved).unwrap(), "# kubewatcher settings\ntheme = \"violet\"\n");
        let (themes, index, problems) = Theme::load_all(Some(&dir));
        assert_eq!(themes[index].name, "violet");
        assert_eq!(problems.len(), 2, "{problems:?}");

        Theme::save_choice(Some(&dir), "gone").unwrap();
        let (_, index, problems) = Theme::load_all(Some(&dir));
        assert_eq!(index, 0);
        assert!(problems.iter().any(|p| p.contains("\"gone\" not found")));
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn falls_back_to_builtins_without_a_usable_directory() {
        let (themes, index, _) = Theme::load_all(None);
        assert_eq!((themes.len(), index), (3, 0));
        assert_eq!(Theme::save_choice(None, "violet"), Err("no home directory".into()));

        // The config "directory" is a file, so nothing can be created in it.
        let file = crate::helpers::test_dir("themes-file");
        fs::write(&file, "").unwrap();
        let (themes, _, _) = Theme::load_all(Some(&file));
        assert_eq!(themes.len(), 3);
        assert!(Theme::save_choice(Some(&file), "violet").is_err());
        fs::remove_file(&file).unwrap();

        // config.toml is a directory, so it cannot be written.
        let dir = crate::helpers::test_dir("themes-config-dir");
        fs::create_dir_all(dir.join("config.toml")).unwrap();
        assert!(Theme::save_choice(Some(&dir), "violet").is_err());
        fs::remove_dir_all(&dir).unwrap();
    }
}
