//! # game-data
//!
//! Loads static game data (zone definitions, etc.) from config files.
//!
//! This crate is entirely synchronous — no Tokio, no async. Zone and world
//! configuration is loaded once at startup and then handed to the simulation
//! layer. Validation catches config mistakes early so the server never starts
//! with bad data.

use serde::{Deserialize, Serialize};

// ─────────────────────────────────────────────────────────────────────────────
// ZoneConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Static definition of a single zone, loaded from the server config file.
///
/// Each zone has a numeric id that becomes its [`common::ZoneId`] at runtime,
/// a human-readable name, an area-of-interest radius that controls how large a
/// player's visible bubble is, and simple rectangular world bounds.
///
/// The id namespace is server-global: duplicate ids across zones are rejected
/// by [`validate_world_config`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ZoneConfig {
    /// Numeric zone identifier (becomes [`common::ZoneId`]).
    pub id: u32,
    /// Human-readable zone name, e.g. `"Elwynn Forest"`.
    pub name: String,
    /// Area-of-interest radius for this zone in world units.
    ///
    /// The zone uses this value when building per-player [`WorldSnapshot`]s:
    /// only entities within `aoi_radius` of a player are included.
    ///
    /// Must be strictly greater than zero.
    pub aoi_radius: f32,
    /// Width of the zone's playable area in world units.
    pub width: f32,
    /// Height of the zone's playable area in world units.
    pub height: f32,
}

// ─────────────────────────────────────────────────────────────────────────────
// WorldConfig
// ─────────────────────────────────────────────────────────────────────────────

/// Top-level configuration containing all zones in the world.
///
/// The server loads exactly one `WorldConfig` at startup. The set of zones is
/// fixed for the lifetime of the process — there is no dynamic zone spawning.
///
/// # TOML format
/// ```toml
/// [[zones]]
/// id = 1
/// name = "Elwynn Forest"
/// aoi_radius = 150.0
/// width = 2000.0
/// height = 2000.0
///
/// [[zones]]
/// id = 2
/// name = "Stormwind City"
/// aoi_radius = 100.0
/// width = 500.0
/// height = 500.0
/// ```
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WorldConfig {
    /// All zones in the world. Order is stable and reflects config file order.
    pub zones: Vec<ZoneConfig>,
}

// ─────────────────────────────────────────────────────────────────────────────
// ConfigError
// ─────────────────────────────────────────────────────────────────────────────

/// Errors produced by [`validate_world_config`].
///
/// The server should treat any of these as a fatal startup error — bad config
/// should never silently produce a degraded world.
#[derive(thiserror::Error, Debug, PartialEq)]
pub enum ConfigError {
    /// Two or more zones share the same numeric id.
    #[error("duplicate zone id: {0}")]
    DuplicateZoneId(u32),

    /// A zone's `aoi_radius` is zero or negative.
    #[error("invalid aoi_radius {0} for zone '{1}': must be > 0")]
    InvalidAoiRadius(f32, String),

    /// A zone's name is the empty string.
    #[error("zone name is empty")]
    EmptyZoneName,
}

// ─────────────────────────────────────────────────────────────────────────────
// Loading
// ─────────────────────────────────────────────────────────────────────────────

/// Parse a [`WorldConfig`] from a TOML string.
///
/// This function only deserializes; it does **not** validate the result.
/// Call [`validate_world_config`] after loading if you want to catch semantic
/// errors (duplicate ids, bad radii, etc.).
///
/// # Errors
/// Returns an error if `toml_str` is not valid TOML or does not conform to
/// the [`WorldConfig`] schema.
pub fn load_world_config(toml_str: &str) -> anyhow::Result<WorldConfig> {
    let config: WorldConfig = toml::from_str(toml_str)?;
    Ok(config)
}

/// Load a [`WorldConfig`] from a file on disk.
///
/// Reads the entire file into memory and delegates to [`load_world_config`].
///
/// # Errors
/// Returns an error if the file cannot be read or its contents cannot be
/// parsed as a valid `WorldConfig`.
pub fn load_world_config_file(path: &std::path::Path) -> anyhow::Result<WorldConfig> {
    let contents = std::fs::read_to_string(path)?;
    load_world_config(&contents)
}

// ─────────────────────────────────────────────────────────────────────────────
// Default config
// ─────────────────────────────────────────────────────────────────────────────

/// Return a hardcoded [`WorldConfig`] suitable for local development.
///
/// Contains four starter zones that are representative of Classic WoW-style
/// starting regions. Zone ids are stable: renaming is fine, but reusing an id
/// for a different zone would break persisted player positions.
///
/// The returned config always passes [`validate_world_config`].
pub fn default_world_config() -> WorldConfig {
    WorldConfig {
        zones: vec![
            ZoneConfig {
                id: 1,
                name: "Elwynn Forest".to_string(),
                aoi_radius: 150.0,
                width: 2000.0,
                height: 2000.0,
            },
            ZoneConfig {
                id: 2,
                name: "Stormwind City".to_string(),
                aoi_radius: 100.0,
                width: 500.0,
                height: 500.0,
            },
            ZoneConfig {
                id: 3,
                name: "Westfall".to_string(),
                aoi_radius: 175.0,
                width: 2200.0,
                height: 2200.0,
            },
            ZoneConfig {
                id: 4,
                name: "The Barrens".to_string(),
                aoi_radius: 200.0,
                width: 4000.0,
                height: 4000.0,
            },
        ],
    }
}

// ─────────────────────────────────────────────────────────────────────────────
// Validation
// ─────────────────────────────────────────────────────────────────────────────

/// Validate a [`WorldConfig`] for semantic correctness.
///
/// Checks performed, in order:
/// 1. No zone has an empty name.
/// 2. Every zone's `aoi_radius` is strictly greater than zero.
/// 3. No two zones share the same numeric id.
///
/// Returns the **first** error found. Fix-and-retry is the expected workflow
/// during development.
///
/// # Errors
/// Returns a [`ConfigError`] describing the first validation failure found.
pub fn validate_world_config(config: &WorldConfig) -> Result<(), ConfigError> {
    let mut seen_ids = std::collections::HashSet::new();

    for zone in &config.zones {
        if zone.name.is_empty() {
            return Err(ConfigError::EmptyZoneName);
        }

        if zone.aoi_radius <= 0.0 {
            return Err(ConfigError::InvalidAoiRadius(
                zone.aoi_radius,
                zone.name.clone(),
            ));
        }

        if !seen_ids.insert(zone.id) {
            return Err(ConfigError::DuplicateZoneId(zone.id));
        }
    }

    Ok(())
}

// ─────────────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const VALID_TOML_2_ZONES: &str = r#"
[[zones]]
id = 1
name = "Elwynn Forest"
aoi_radius = 150.0
width = 2000.0
height = 2000.0

[[zones]]
id = 2
name = "Stormwind City"
aoi_radius = 100.0
width = 500.0
height = 500.0
"#;

    // ── 1. load_world_config parses valid TOML with 2 zones ──────────────────

    #[test]
    fn load_world_config_parses_valid_toml() {
        let config = load_world_config(VALID_TOML_2_ZONES).expect("should parse");
        assert_eq!(config.zones.len(), 2);

        let elwynn = &config.zones[0];
        assert_eq!(elwynn.id, 1);
        assert_eq!(elwynn.name, "Elwynn Forest");
        assert!((elwynn.aoi_radius - 150.0).abs() < f32::EPSILON);
        assert!((elwynn.width - 2000.0).abs() < f32::EPSILON);
        assert!((elwynn.height - 2000.0).abs() < f32::EPSILON);

        let stormwind = &config.zones[1];
        assert_eq!(stormwind.id, 2);
        assert_eq!(stormwind.name, "Stormwind City");
        assert!((stormwind.aoi_radius - 100.0).abs() < f32::EPSILON);
        assert!((stormwind.width - 500.0).abs() < f32::EPSILON);
        assert!((stormwind.height - 500.0).abs() < f32::EPSILON);
    }

    // ── 2. load_world_config returns error on malformed TOML ─────────────────

    #[test]
    fn load_world_config_rejects_malformed_toml() {
        let bad = "[[zones\nid = ???";
        assert!(
            load_world_config(bad).is_err(),
            "malformed TOML should produce an error"
        );
    }

    // ── 3. validate_world_config passes for default_world_config() ───────────

    #[test]
    fn validate_default_world_config_passes() {
        let config = default_world_config();
        validate_world_config(&config).expect("default config should be valid");
    }

    // ── 4. DuplicateZoneId when two zones share an id ─────────────────────────

    #[test]
    fn validate_detects_duplicate_zone_id() {
        let config = WorldConfig {
            zones: vec![
                ZoneConfig {
                    id: 1,
                    name: "Zone A".to_string(),
                    aoi_radius: 100.0,
                    width: 1000.0,
                    height: 1000.0,
                },
                ZoneConfig {
                    id: 1, // duplicate!
                    name: "Zone B".to_string(),
                    aoi_radius: 100.0,
                    width: 1000.0,
                    height: 1000.0,
                },
            ],
        };
        assert_eq!(
            validate_world_config(&config),
            Err(ConfigError::DuplicateZoneId(1))
        );
    }

    // ── 5. InvalidAoiRadius when aoi_radius <= 0 ─────────────────────────────

    #[test]
    fn validate_detects_non_positive_aoi_radius() {
        let zero_radius = WorldConfig {
            zones: vec![ZoneConfig {
                id: 1,
                name: "Bad Zone".to_string(),
                aoi_radius: 0.0,
                width: 1000.0,
                height: 1000.0,
            }],
        };
        assert_eq!(
            validate_world_config(&zero_radius),
            Err(ConfigError::InvalidAoiRadius(0.0, "Bad Zone".to_string()))
        );

        let negative_radius = WorldConfig {
            zones: vec![ZoneConfig {
                id: 2,
                name: "Negative Zone".to_string(),
                aoi_radius: -50.0,
                width: 1000.0,
                height: 1000.0,
            }],
        };
        assert_eq!(
            validate_world_config(&negative_radius),
            Err(ConfigError::InvalidAoiRadius(
                -50.0,
                "Negative Zone".to_string()
            ))
        );
    }

    // ── 6. EmptyZoneName when name is "" ─────────────────────────────────────

    #[test]
    fn validate_detects_empty_zone_name() {
        let config = WorldConfig {
            zones: vec![ZoneConfig {
                id: 1,
                name: "".to_string(),
                aoi_radius: 100.0,
                width: 1000.0,
                height: 1000.0,
            }],
        };
        assert_eq!(validate_world_config(&config), Err(ConfigError::EmptyZoneName));
    }

    // ── 7. load_world_config_file writes a temp file and loads it correctly ───

    #[test]
    fn load_world_config_file_round_trip() {
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().expect("temp file");
        tmp.write_all(VALID_TOML_2_ZONES.as_bytes())
            .expect("write");

        let config =
            load_world_config_file(tmp.path()).expect("load from file should succeed");
        assert_eq!(config.zones.len(), 2);
        assert_eq!(config.zones[0].name, "Elwynn Forest");
        assert_eq!(config.zones[1].name, "Stormwind City");
    }

    // ── 8. default_world_config: ≥4 zones, unique ids, all aoi_radius > 0 ───

    #[test]
    fn default_world_config_invariants() {
        let config = default_world_config();

        assert!(
            config.zones.len() >= 4,
            "expected at least 4 zones, got {}",
            config.zones.len()
        );

        // All aoi_radius values must be positive.
        for zone in &config.zones {
            assert!(
                zone.aoi_radius > 0.0,
                "zone '{}' has non-positive aoi_radius {}",
                zone.name,
                zone.aoi_radius
            );
        }

        // All ids must be unique.
        let mut ids = std::collections::HashSet::new();
        for zone in &config.zones {
            assert!(
                ids.insert(zone.id),
                "duplicate zone id {} found in default config",
                zone.id
            );
        }
    }
}
