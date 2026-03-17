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
// CreatureId / AiBehavior / CreatureDef / SpawnPoint
// ─────────────────────────────────────────────────────────────────────────────

/// Numeric identifier for a creature template (wolf, bear, NPC, etc.).
pub type CreatureId = u32;

/// Determines how a creature's AI behaves in the world.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AiBehavior {
    /// Stands still (vendors, quest givers).
    Idle,
    /// Random walk within radius of spawn point.
    Wander,
}

/// Static definition of a creature type. Loaded from config, never mutated at runtime.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreatureDef {
    pub id: CreatureId,
    pub name: String,
    pub level: u32,
    pub max_health: u32,
    pub move_speed: f32,
    pub behavior: AiBehavior,
    /// `true` = NPC (EntityKind::Npc), `false` = Mob (EntityKind::Mob).
    pub is_npc: bool,
}

/// A point where a creature spawns (and respawns after death).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SpawnPoint {
    pub creature_id: CreatureId,
    pub x: f32,
    pub z: f32,
    pub wander_radius: f32,
    pub respawn_secs: f32,
}

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
    /// Creature spawn points within this zone.
    #[serde(default)]
    pub spawns: Vec<SpawnPoint>,
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
    /// Creature type definitions. Referenced by `SpawnPoint::creature_id`.
    #[serde(default)]
    pub creatures: Vec<CreatureDef>,
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

    /// Two or more creatures share the same numeric id.
    #[error("duplicate creature id: {0}")]
    DuplicateCreatureId(CreatureId),

    /// A spawn point references a creature id not defined in `creatures`.
    #[error("spawn references unknown creature id {creature_id} in zone '{zone_name}'")]
    UnknownCreatureId {
        creature_id: CreatureId,
        zone_name: String,
    },

    /// A creature definition has an invalid value (zero/negative health, speed, level).
    #[error("invalid creature '{name}': {reason}")]
    InvalidCreature { name: String, reason: String },
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
        creatures: vec![
            CreatureDef {
                id: 1,
                name: "Forest Wolf".to_string(),
                level: 3,
                max_health: 120,
                move_speed: 5.0,
                behavior: AiBehavior::Wander,
                is_npc: false,
            },
            CreatureDef {
                id: 2,
                name: "Young Bear".to_string(),
                level: 5,
                max_health: 200,
                move_speed: 4.0,
                behavior: AiBehavior::Wander,
                is_npc: false,
            },
            CreatureDef {
                id: 3,
                name: "Marshal Dughan".to_string(),
                level: 25,
                max_health: 2000,
                move_speed: 0.0,
                behavior: AiBehavior::Idle,
                is_npc: true,
            },
        ],
        zones: vec![
            ZoneConfig {
                id: 1,
                name: "Elwynn Forest".to_string(),
                aoi_radius: 150.0,
                width: 2000.0,
                height: 2000.0,
                spawns: vec![
                    SpawnPoint { creature_id: 1, x: 200.0, z: 300.0, wander_radius: 40.0, respawn_secs: 30.0 },
                    SpawnPoint { creature_id: 1, x: 400.0, z: 500.0, wander_radius: 35.0, respawn_secs: 30.0 },
                    SpawnPoint { creature_id: 2, x: 600.0, z: 200.0, wander_radius: 50.0, respawn_secs: 45.0 },
                    SpawnPoint { creature_id: 2, x: 300.0, z: 700.0, wander_radius: 45.0, respawn_secs: 45.0 },
                    SpawnPoint { creature_id: 3, x: 100.0, z: 100.0, wander_radius: 0.0, respawn_secs: 60.0 },
                ],
            },
            ZoneConfig {
                id: 2,
                name: "Stormwind City".to_string(),
                aoi_radius: 100.0,
                width: 500.0,
                height: 500.0,
                spawns: vec![],
            },
            ZoneConfig {
                id: 3,
                name: "Westfall".to_string(),
                aoi_radius: 175.0,
                width: 2200.0,
                height: 2200.0,
                spawns: vec![],
            },
            ZoneConfig {
                id: 4,
                name: "The Barrens".to_string(),
                aoi_radius: 200.0,
                width: 4000.0,
                height: 4000.0,
                spawns: vec![],
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
    // Validate creatures.
    let mut seen_creature_ids = std::collections::HashSet::new();
    for creature in &config.creatures {
        if !seen_creature_ids.insert(creature.id) {
            return Err(ConfigError::DuplicateCreatureId(creature.id));
        }
        if creature.max_health == 0 {
            return Err(ConfigError::InvalidCreature {
                name: creature.name.clone(),
                reason: "max_health must be > 0".to_string(),
            });
        }
        if creature.level == 0 {
            return Err(ConfigError::InvalidCreature {
                name: creature.name.clone(),
                reason: "level must be > 0".to_string(),
            });
        }
        if creature.move_speed < 0.0 {
            return Err(ConfigError::InvalidCreature {
                name: creature.name.clone(),
                reason: "move_speed must be >= 0".to_string(),
            });
        }
    }

    // Validate zones.
    let mut seen_zone_ids = std::collections::HashSet::new();

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

        if !seen_zone_ids.insert(zone.id) {
            return Err(ConfigError::DuplicateZoneId(zone.id));
        }

        // Validate spawn points reference defined creatures.
        for spawn in &zone.spawns {
            if !seen_creature_ids.contains(&spawn.creature_id) {
                return Err(ConfigError::UnknownCreatureId {
                    creature_id: spawn.creature_id,
                    zone_name: zone.name.clone(),
                });
            }
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
            creatures: vec![],
            zones: vec![
                ZoneConfig {
                    id: 1,
                    name: "Zone A".to_string(),
                    aoi_radius: 100.0,
                    width: 1000.0,
                    height: 1000.0,
                    spawns: vec![],
                },
                ZoneConfig {
                    id: 1, // duplicate!
                    name: "Zone B".to_string(),
                    aoi_radius: 100.0,
                    width: 1000.0,
                    height: 1000.0,
                    spawns: vec![],
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
            creatures: vec![],
            zones: vec![ZoneConfig {
                id: 1,
                name: "Bad Zone".to_string(),
                aoi_radius: 0.0,
                width: 1000.0,
                height: 1000.0,
                spawns: vec![],
            }],
        };
        assert_eq!(
            validate_world_config(&zero_radius),
            Err(ConfigError::InvalidAoiRadius(0.0, "Bad Zone".to_string()))
        );

        let negative_radius = WorldConfig {
            creatures: vec![],
            zones: vec![ZoneConfig {
                id: 2,
                name: "Negative Zone".to_string(),
                aoi_radius: -50.0,
                width: 1000.0,
                height: 1000.0,
                spawns: vec![],
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
            creatures: vec![],
            zones: vec![ZoneConfig {
                id: 1,
                name: "".to_string(),
                aoi_radius: 100.0,
                width: 1000.0,
                height: 1000.0,
                spawns: vec![],
            }],
        };
        assert_eq!(validate_world_config(&config), Err(ConfigError::EmptyZoneName));
    }

    // ── Creature validation ─────────────────────────────────────────────────

    #[test]
    fn validate_detects_duplicate_creature_id() {
        let config = WorldConfig {
            creatures: vec![
                CreatureDef { id: 1, name: "Wolf".into(), level: 1, max_health: 100, move_speed: 5.0, behavior: AiBehavior::Wander, is_npc: false },
                CreatureDef { id: 1, name: "Bear".into(), level: 2, max_health: 200, move_speed: 4.0, behavior: AiBehavior::Wander, is_npc: false },
            ],
            zones: vec![],
        };
        assert_eq!(validate_world_config(&config), Err(ConfigError::DuplicateCreatureId(1)));
    }

    #[test]
    fn validate_detects_unknown_creature_ref() {
        let config = WorldConfig {
            creatures: vec![],
            zones: vec![ZoneConfig {
                id: 1,
                name: "Test".into(),
                aoi_radius: 100.0,
                width: 1000.0,
                height: 1000.0,
                spawns: vec![SpawnPoint { creature_id: 99, x: 0.0, z: 0.0, wander_radius: 10.0, respawn_secs: 30.0 }],
            }],
        };
        match validate_world_config(&config) {
            Err(ConfigError::UnknownCreatureId { creature_id: 99, .. }) => {}
            other => panic!("expected UnknownCreatureId, got {:?}", other),
        }
    }

    #[test]
    fn validate_detects_invalid_creature_health() {
        let config = WorldConfig {
            creatures: vec![
                CreatureDef { id: 1, name: "Bad".into(), level: 1, max_health: 0, move_speed: 5.0, behavior: AiBehavior::Idle, is_npc: false },
            ],
            zones: vec![],
        };
        match validate_world_config(&config) {
            Err(ConfigError::InvalidCreature { .. }) => {}
            other => panic!("expected InvalidCreature, got {:?}", other),
        }
    }

    #[test]
    fn validate_detects_invalid_creature_level() {
        let config = WorldConfig {
            creatures: vec![
                CreatureDef { id: 1, name: "Bad".into(), level: 0, max_health: 100, move_speed: 5.0, behavior: AiBehavior::Idle, is_npc: false },
            ],
            zones: vec![],
        };
        match validate_world_config(&config) {
            Err(ConfigError::InvalidCreature { .. }) => {}
            other => panic!("expected InvalidCreature, got {:?}", other),
        }
    }

    #[test]
    fn toml_with_creatures_and_spawns_parses() {
        let toml = r#"
[[creatures]]
id = 1
name = "Wolf"
level = 3
max_health = 120
move_speed = 5.0
behavior = "Wander"
is_npc = false

[[zones]]
id = 1
name = "Forest"
aoi_radius = 150.0
width = 2000.0
height = 2000.0

[[zones.spawns]]
creature_id = 1
x = 200.0
z = 300.0
wander_radius = 40.0
respawn_secs = 30.0
"#;
        let config = load_world_config(toml).expect("should parse");
        assert_eq!(config.creatures.len(), 1);
        assert_eq!(config.creatures[0].name, "Wolf");
        assert_eq!(config.zones[0].spawns.len(), 1);
        assert_eq!(config.zones[0].spawns[0].creature_id, 1);
        validate_world_config(&config).expect("should be valid");
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

        assert!(
            !config.creatures.is_empty(),
            "default config should include creatures"
        );

        // At least one zone should have spawn points.
        assert!(
            config.zones.iter().any(|z| !z.spawns.is_empty()),
            "at least one zone should have spawn points"
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
