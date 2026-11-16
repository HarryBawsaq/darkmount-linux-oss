//! `~/.config/proton-presence/config.toml`.
//!
//! ```toml
//! default_client_id = "123456789012345678"
//!
//! [games.1245620]                    # Steam app ID
//! client_id = "234567890123456789"   # optional, falls back to the default
//! large_image = "elden_ring"         # optional art asset key
//! ```

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::Path;

use anyhow::{bail, Context, Result};
use serde::Deserialize;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    pub default_client_id: Option<String>,
    pub games: HashMap<u64, GameConfig>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GameConfig {
    pub client_id: Option<String>,
    pub large_image: Option<String>,
}

/// What to use for one game after applying the default fallback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved<'a> {
    pub client_id: &'a str,
    pub large_image: Option<&'a str>,
    /// False when the client ID is the default one.
    pub mapped: bool,
}

/// Discord IDs are 64-bit integers; accept them quoted or bare.
#[derive(Deserialize)]
#[serde(untagged)]
enum RawId {
    Str(String),
    Int(u64),
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawConfig {
    default_client_id: Option<RawId>,
    #[serde(default)]
    games: HashMap<String, RawGame>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawGame {
    client_id: Option<RawId>,
    large_image: Option<String>,
}

/// Normalises a client ID. An empty string means "not set", so the
/// installed template is valid before it is filled in.
fn client_id(raw: Option<RawId>, what: &str) -> Result<Option<String>> {
    let id = match raw {
        None => return Ok(None),
        Some(RawId::Int(n)) => n.to_string(),
        Some(RawId::Str(s)) => s.trim().to_string(),
    };
    if id.is_empty() {
        return Ok(None);
    }
    if !id.bytes().all(|b| b.is_ascii_digit()) {
        bail!("{what} must be a numeric Discord application ID, got {id:?}");
    }
    Ok(Some(id))
}

impl Config {
    pub fn parse(text: &str) -> Result<Config> {
        let raw: RawConfig = toml::from_str(text)?;
        let mut games = HashMap::new();
        for (key, game) in raw.games {
            let app_id: u64 = key
                .trim()
                .parse()
                .with_context(|| format!("[games.{key}]: {key:?} is not a Steam app ID"))?;
            games.insert(
                app_id,
                GameConfig {
                    client_id: client_id(game.client_id, &format!("games.{key}.client_id"))?,
                    large_image: game.large_image.filter(|s| !s.trim().is_empty()),
                },
            );
        }
        Ok(Config {
            default_client_id: client_id(raw.default_client_id, "default_client_id")?,
            games,
        })
    }

    /// Loads the file. A missing file is an empty config, not an error.
    pub fn load(path: &Path) -> Result<Config> {
        match fs::read_to_string(path) {
            Ok(text) => Config::parse(&text).with_context(|| format!("in {}", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Config::default()),
            Err(e) => Err(e).with_context(|| format!("cannot read {}", path.display())),
        }
    }

    /// The client ID and image for a game, or `None` when the game has no
    /// mapping and there is no default client ID.
    pub fn resolve(&self, app_id: u64) -> Option<Resolved<'_>> {
        let game = self.games.get(&app_id);
        let mapped = game.and_then(|g| g.client_id.as_deref());
        Some(Resolved {
            client_id: mapped.or(self.default_client_id.as_deref())?,
            large_image: game.and_then(|g| g.large_image.as_deref()),
            mapped: mapped.is_some(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::TempDir;

    const EXAMPLE: &str = r#"
default_client_id = "100"

[games.1245620]
client_id = "200"
large_image = "elden_ring"

[games.620]
large_image = "portal2"

[games.400]
client_id = 300
"#;

    #[test]
    fn parses_mappings_and_default() {
        let config = Config::parse(EXAMPLE).unwrap();
        assert_eq!(config.default_client_id.as_deref(), Some("100"));
        assert_eq!(config.games.len(), 3);
        assert_eq!(
            config.games[&1245620],
            GameConfig {
                client_id: Some("200".into()),
                large_image: Some("elden_ring".into()),
            }
        );
        assert_eq!(config.games[&400].client_id.as_deref(), Some("300"));
    }

    #[test]
    fn resolves_with_fallback_to_default() {
        let config = Config::parse(EXAMPLE).unwrap();

        let mapped = config.resolve(1245620).unwrap();
        assert_eq!(mapped.client_id, "200");
        assert_eq!(mapped.large_image, Some("elden_ring"));
        assert!(mapped.mapped);

        let image_only = config.resolve(620).unwrap();
        assert_eq!(image_only.client_id, "100");
        assert_eq!(image_only.large_image, Some("portal2"));
        assert!(!image_only.mapped);

        let unmapped = config.resolve(999).unwrap();
        assert_eq!(unmapped.client_id, "100");
        assert_eq!(unmapped.large_image, None);
        assert!(!unmapped.mapped);
    }

    #[test]
    fn nothing_resolves_without_a_default() {
        let config = Config::parse("[games.5]\nclient_id = \"7\"\n").unwrap();
        assert_eq!(config.resolve(5).unwrap().client_id, "7");
        assert_eq!(config.resolve(6), None);
        assert_eq!(Config::parse("").unwrap(), Config::default());
        assert_eq!(Config::default().resolve(5), None);
    }

    #[test]
    fn empty_client_id_counts_as_unset() {
        let config = Config::parse("default_client_id = \"\"\n").unwrap();
        assert_eq!(config.default_client_id, None);
    }

    #[test]
    fn rejects_invalid_configs() {
        assert!(Config::parse("default_client_id = \"abc\"").is_err());
        assert!(Config::parse("[games.eldenring]\nclient_id = \"1\"").is_err());
        assert!(Config::parse("[games.5]\nclient_id = \"x1\"").is_err());
        assert!(Config::parse("[games.5]\nlarge_imgae = \"typo\"").is_err());
        assert!(Config::parse("defualt_client_id = \"1\"").is_err());
        assert!(Config::parse("default_client_id = ").is_err());
    }

    #[test]
    fn missing_file_is_an_empty_config() {
        let tmp = TempDir::new("config");
        let path = tmp.path().join("config.toml");
        assert_eq!(Config::load(&path).unwrap(), Config::default());

        fs::write(&path, EXAMPLE).unwrap();
        assert_eq!(Config::load(&path).unwrap().games.len(), 3);

        fs::write(&path, "default_client_id = [").unwrap();
        let err = format!("{:#}", Config::load(&path).unwrap_err());
        assert!(err.contains("config.toml"), "{err}");
    }
}
