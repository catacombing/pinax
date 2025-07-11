//! Configuration options.

use std::fmt::{self, Display, Formatter};
use std::ops::Deref;
use std::path::PathBuf;
use std::time::Duration;

use calloop::LoopHandle;
use calloop::channel::{self, Event, Sender};
use configory::EventHandler;
use configory::docgen::{DocType, Docgen, Leaf};
use serde::de::Visitor;
use serde::{Deserialize, Deserializer};
use skia_safe::Color4f;
use tracing::{error, info};

use crate::State;

/// # Pinax
///
/// ## Syntax
///
/// Pinax's configuration file uses the TOML format. The format's specification
/// can be found at _https://toml.io/en/v1.0.0_.
///
/// ## Location
///
/// Pinax doesn't create the configuration file for you, but it looks for one at
/// <br> `${XDG_CONFIG_HOME:-$HOME/.config}/pinax/pinax.toml`.
///
/// ## Fields
#[derive(Docgen, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    /// This section documents the `[general]` table.
    pub general: General,
    /// This section documents the `[font]` table.
    pub font: Font,
    /// This section documents the `[color]` table.
    pub colors: Colors,
    /// This section documents the `[input]` table.
    pub input: Input,
}

/// General configuration.
#[derive(Docgen, Deserialize, Default, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct General {
    /// Location the notes are saved to.
    #[docgen(default = "${XDG_DATA_HOME:-$HOME/.local/share}/pinax/notes")]
    path: Option<PathBuf>,
}

impl General {
    /// Get the storage path.
    pub fn storage_path(&self) -> PathBuf {
        self.path.clone().unwrap_or_else(|| dirs::data_dir().unwrap().join("pinax/notes"))
    }
}

/// Font configuration.
#[derive(Docgen, Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Font {
    /// Font family.
    pub family: String,
    /// Font size.
    pub size: f64,
}

impl Default for Font {
    fn default() -> Self {
        Self { family: String::from("sans"), size: 18. }
    }
}

/// Color configuration.
#[derive(Docgen, Deserialize, Copy, Clone, Hash, PartialEq, Eq, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Colors {
    /// Primary foreground color.
    #[serde(alias = "fg")]
    pub foreground: Color,
    /// Primary background color.
    #[serde(alias = "bg")]
    pub background: Color,
    /// Primary accent color.
    #[serde(alias = "hl")]
    pub highlight: Color,
}

impl Default for Colors {
    fn default() -> Self {
        Self {
            foreground: Color::new(255, 255, 255),
            background: Color::new(24, 24, 24),
            highlight: Color::new(117, 42, 42),
        }
    }
}

/// Input configuration.
#[derive(Docgen, Deserialize, Debug)]
#[serde(default, deny_unknown_fields)]
pub struct Input {
    /// Square of the maximum distance before touch input is considered a drag.
    pub max_tap_distance: f64,
    /// Maximum interval between taps to be considered a double/trible-tap.
    #[docgen(doc_type = "integer (milliseconds)", default = "300")]
    pub max_multi_tap: MillisDuration,
}

impl Default for Input {
    fn default() -> Self {
        Self { max_multi_tap: Duration::from_millis(300).into(), max_tap_distance: 400. }
    }
}

/// RGB color.
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Color {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }

    pub const fn as_color4f(&self) -> Color4f {
        Color4f { r: self.r as f32 / 255., g: self.g as f32 / 255., b: self.b as f32 / 255., a: 1. }
    }
}

impl Docgen for Color {
    fn doc_type() -> DocType {
        DocType::Leaf(Leaf::new("color"))
    }

    fn format(&self) -> String {
        format!("\"#{:0>2x}{:0>2x}{:0>2x}\"", self.r, self.g, self.b)
    }
}

/// Deserialize rgb color from a hex string.
impl<'de> Deserialize<'de> for Color {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ColorVisitor;

        impl Visitor<'_> for ColorVisitor {
            type Value = Color;

            fn expecting(&self, f: &mut Formatter<'_>) -> fmt::Result {
                f.write_str("hex color like #ff00ff")
            }

            fn visit_str<E>(self, value: &str) -> Result<Color, E>
            where
                E: serde::de::Error,
            {
                let channels = match value.strip_prefix('#') {
                    Some(channels) => channels,
                    None => {
                        return Err(E::custom(format!("color {value:?} is missing leading '#'")));
                    },
                };

                let digits = channels.len();
                if digits != 6 {
                    let msg = format!("color {value:?} has {digits} digits; expected 6");
                    return Err(E::custom(msg));
                }

                match u32::from_str_radix(channels, 16) {
                    Ok(mut color) => {
                        let b = (color & 0xFF) as u8;
                        color >>= 8;
                        let g = (color & 0xFF) as u8;
                        color >>= 8;
                        let r = color as u8;

                        Ok(Color::new(r, g, b))
                    },
                    Err(_) => Err(E::custom(format!("color {value:?} contains non-hex digits"))),
                }
            }
        }

        deserializer.deserialize_str(ColorVisitor)
    }
}

impl Display for Color {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "#{:0>2x}{:0>2x}{:0>2x}", self.r, self.g, self.b)
    }
}

/// Config wrapper for millisecond-precision durations.
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub struct MillisDuration(Duration);

impl Deref for MillisDuration {
    type Target = Duration;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl<'de> Deserialize<'de> for MillisDuration {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let ms = u64::deserialize(deserializer)?;
        Ok(Duration::from_millis(ms).into())
    }
}

impl From<Duration> for MillisDuration {
    fn from(duration: Duration) -> Self {
        Self(duration)
    }
}

impl Display for MillisDuration {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), fmt::Error> {
        write!(f, "{}", self.0.as_millis())
    }
}

/// Event handler for configuration manager updates.
pub struct ConfigEventHandler {
    tx: Sender<Config>,
}

impl ConfigEventHandler {
    pub fn new(event_loop: &LoopHandle<'static, State>) -> Self {
        // Create calloop channel to apply config updates.
        let (tx, rx) = channel::channel();
        let _ = event_loop
            .insert_source(rx, |event, _, state| {
                if let Event::Msg(config) = event {
                    state.window.update_config(&config);
                }
            })
            .inspect_err(|err| error!("Failed to insert config source: {err}"));

        Self { tx }
    }

    /// Reload the configuration file.
    fn reload_config(&self, config: &configory::Config) {
        info!("Reloading configuration file");

        // Parse config or fall back to the default.
        let parsed = config
            .get::<&str, Config>(&[])
            .inspect_err(|err| error!("Config error: {err}"))
            .ok()
            .flatten()
            .unwrap_or_default();

        // Update the config.
        if let Err(err) = self.tx.send(parsed) {
            error!("Failed to send on config channel: {err}");
        }
    }
}

impl EventHandler<()> for ConfigEventHandler {
    fn file_changed(&self, config: &configory::Config) {
        self.reload_config(config);
    }

    fn ipc_changed(&self, config: &configory::Config) {
        self.reload_config(config);
    }

    fn file_error(&self, _config: &configory::Config, err: configory::Error) {
        error!("Configuration file error: {err}");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use configory::docgen::markdown::Markdown;

    use super::*;

    #[test]
    fn config_docs() {
        let mut formatter = Markdown::new();
        formatter.set_heading_size(3);
        let expected = formatter.format::<Config>();

        // Uncomment to update config documentation.
        // fs::write("./docs/config.md", &expected).unwrap();

        // Ensure documentation is up to date.
        let docs = fs::read_to_string("./docs/config.md").unwrap();
        assert_eq!(docs, expected);
    }
}
