//! A zero-I/O `GhostBinding` and its validator (spec 0004).
//!
//! The types mirror the cortex GHOST contract (`cortex-domain::ghost`,
//! REQ-0125-01 to -04). littleguy does not depend on cortex. A test pins the
//! serialized shape so drift shows up. The module does no I/O.

use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The cortex seat URI prefix for a persistent Ghost.
pub const GHOST_SPIFFE_AGENT_PREFIX: &str = "spiffe://unsigned.gg/agent/";
/// The longest `ghost_id` in bytes.
pub const MAX_GHOST_ID_BYTES: usize = 64;
/// The longest `mitc_principal` in bytes.
pub const MAX_MITC_PRINCIPAL_BYTES: usize = 128;
/// The longest `spiffe_seat` in bytes.
pub const MAX_SPIFFE_SEAT_BYTES: usize = 256;
/// The longest seat team or slug in bytes.
pub const MAX_SEAT_SEGMENT_BYTES: usize = 63;

/// The Ghost manifestation taxonomy (cortex `GhostManifestation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhostManifestation {
    /// A repository or domain callsign.
    Persona,
    /// A partner paired with a human principal.
    Muse,
    /// A named persistent mesh seat.
    Agent,
    /// An ephemeral subordinate slot. It holds no persistent seat.
    Crew,
    /// An autonomous cluster or daemon guardian.
    Revenant,
}

impl GhostManifestation {
    /// The `snake_case` wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Persona => "persona",
            Self::Muse => "muse",
            Self::Agent => "agent",
            Self::Crew => "crew",
            Self::Revenant => "revenant",
        }
    }

    /// True when the manifestation can hold a persistent seat.
    #[must_use]
    pub const fn supports_persistent_seat(self) -> bool {
        !matches!(self, Self::Crew)
    }
}

impl fmt::Display for GhostManifestation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The nine Ghost lifecycle states (cortex `GhostLifecycleState`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GhostLifecycleState {
    /// Identity minted, not yet paired.
    Minted,
    /// Paired to a principal.
    Paired,
    /// Attached to a shell.
    Embodied,
    /// Running in a shell.
    Awake,
    /// Draining before a molt.
    Quiescing,
    /// Shell released, seat reserved.
    Dormant,
    /// Offline consolidation.
    Dreaming,
    /// Isolated after a violation.
    Quarantined,
    /// Terminal.
    Retired,
}

impl GhostLifecycleState {
    /// The `snake_case` wire form.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Minted => "minted",
            Self::Paired => "paired",
            Self::Embodied => "embodied",
            Self::Awake => "awake",
            Self::Quiescing => "quiescing",
            Self::Dormant => "dormant",
            Self::Dreaming => "dreaming",
            Self::Quarantined => "quarantined",
            Self::Retired => "retired",
        }
    }
}

impl fmt::Display for GhostLifecycleState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Why a Ghost binding is refused.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GhostBindingError {
    /// A required field is empty.
    #[error("ghost binding field `{field}` is empty")]
    EmptyField {
        /// The field name.
        field: &'static str,
    },
    /// A field is longer than its cap.
    #[error("ghost binding field `{field}` is longer than {max} bytes")]
    FieldTooLong {
        /// The field name.
        field: &'static str,
        /// The cap in bytes.
        max: usize,
    },
    /// A field holds a character its rule does not allow.
    #[error("ghost binding field `{field}` holds a character that is not allowed")]
    InvalidField {
        /// The field name.
        field: &'static str,
    },
    /// A `Crew` binding holds a persistent seat (REQ-0125-04).
    #[error("a crew ghost cannot hold a persistent spiffe seat")]
    CrewHoldsSeat,
    /// A `Crew` binding names a state that cortex forbids for it (REQ-0125-04).
    #[error("a crew ghost cannot be in the `{state}` state")]
    CrewForbiddenState {
        /// The refused state.
        state: GhostLifecycleState,
    },
    /// A persistent manifestation has no seat (REQ-0125-03).
    #[error("a `{manifestation}` ghost needs a persistent spiffe seat")]
    MissingSeat {
        /// The manifestation.
        manifestation: GhostManifestation,
    },
    /// The seat is not `spiffe://unsigned.gg/agent/TEAM/SLUG` (REQ-0125-03).
    #[error("invalid ghost spiffe seat: {reason}")]
    InvalidSpiffeSeat {
        /// The short reason.
        reason: &'static str,
    },
}

/// The unvalidated wire form of a [`GhostBinding`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GhostBindingWire {
    /// The Ghost callsign.
    pub ghost_id: String,
    /// The manifestation.
    pub manifestation: GhostManifestation,
    /// The human principal that owns the Ghost.
    pub mitc_principal: String,
    /// The persistent seat, or `None` for `Crew`.
    #[serde(default)]
    pub spiffe_seat: Option<String>,
    /// The molt counter.
    pub molt_epoch: u64,
    /// The lifecycle state, when the delivery states it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<GhostLifecycleState>,
}

/// A validated Ghost callsign: 1 to 64 bytes of `[a-z0-9-]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GhostId(String);

impl GhostId {
    /// The callsign as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated persistent seat `spiffe://unsigned.gg/agent/TEAM/SLUG`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct GhostSeat(String);

impl GhostSeat {
    /// The seat URI as text.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A validated Ghost binding. Build it with [`validate`] or deserialize it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "GhostBindingWire", into = "GhostBindingWire")]
pub struct GhostBinding {
    ghost_id: GhostId,
    manifestation: GhostManifestation,
    mitc_principal: String,
    spiffe_seat: Option<GhostSeat>,
    molt_epoch: u64,
    state: Option<GhostLifecycleState>,
}

impl GhostBinding {
    /// The Ghost callsign.
    #[must_use]
    pub fn ghost_id(&self) -> &str {
        self.ghost_id.as_str()
    }

    /// The manifestation.
    #[must_use]
    pub const fn manifestation(&self) -> GhostManifestation {
        self.manifestation
    }

    /// The human principal that owns the Ghost.
    #[must_use]
    pub fn mitc_principal(&self) -> &str {
        &self.mitc_principal
    }

    /// The persistent seat, if any.
    #[must_use]
    pub fn spiffe_seat(&self) -> Option<&str> {
        self.spiffe_seat.as_ref().map(GhostSeat::as_str)
    }

    /// The molt counter.
    #[must_use]
    pub const fn molt_epoch(&self) -> u64 {
        self.molt_epoch
    }

    /// The stated lifecycle state, if any.
    #[must_use]
    pub const fn state(&self) -> Option<GhostLifecycleState> {
        self.state
    }
}

impl From<GhostBinding> for GhostBindingWire {
    fn from(binding: GhostBinding) -> Self {
        Self {
            ghost_id: binding.ghost_id.0,
            manifestation: binding.manifestation,
            mitc_principal: binding.mitc_principal,
            spiffe_seat: binding.spiffe_seat.map(|seat| seat.0),
            molt_epoch: binding.molt_epoch,
            state: binding.state,
        }
    }
}

impl TryFrom<GhostBindingWire> for GhostBinding {
    type Error = GhostBindingError;

    fn try_from(wire: GhostBindingWire) -> Result<Self, Self::Error> {
        validate(wire)
    }
}

/// Validates a wire binding against the cortex GHOST contract.
///
/// # Errors
///
/// Returns [`GhostBindingError`] when a field breaks its bound or a cortex rule.
pub fn validate(wire: GhostBindingWire) -> Result<GhostBinding, GhostBindingError> {
    check_text("ghost_id", &wire.ghost_id, MAX_GHOST_ID_BYTES)?;
    if !wire.ghost_id.bytes().all(is_slug_byte) {
        return Err(GhostBindingError::InvalidField { field: "ghost_id" });
    }
    check_text(
        "mitc_principal",
        &wire.mitc_principal,
        MAX_MITC_PRINCIPAL_BYTES,
    )?;
    let principal = &wire.mitc_principal;
    if principal.chars().any(char::is_control) || principal.trim() != principal {
        return Err(GhostBindingError::InvalidField {
            field: "mitc_principal",
        });
    }
    let persistent = wire.manifestation.supports_persistent_seat();
    if !persistent {
        // Mirrors cortex `validate_transition_for` and `new_persistent` (REQ-0125-04).
        if wire.spiffe_seat.is_some() {
            return Err(GhostBindingError::CrewHoldsSeat);
        }
        if let Some(state @ (GhostLifecycleState::Dormant | GhostLifecycleState::Dreaming)) =
            wire.state
        {
            return Err(GhostBindingError::CrewForbiddenState { state });
        }
    }
    let seat = match (persistent, &wire.spiffe_seat) {
        (true, None) => {
            return Err(GhostBindingError::MissingSeat {
                manifestation: wire.manifestation,
            });
        }
        (true, Some(seat)) => Some(validate_seat(seat)?),
        (false, _) => None,
    };
    Ok(GhostBinding {
        ghost_id: GhostId(wire.ghost_id),
        manifestation: wire.manifestation,
        mitc_principal: wire.mitc_principal,
        spiffe_seat: seat,
        molt_epoch: wire.molt_epoch,
        state: wire.state,
    })
}

const fn is_slug_byte(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'
}

const fn check_text(field: &'static str, text: &str, max: usize) -> Result<(), GhostBindingError> {
    if text.is_empty() {
        return Err(GhostBindingError::EmptyField { field });
    }
    if text.len() > max {
        return Err(GhostBindingError::FieldTooLong { field, max });
    }
    Ok(())
}

/// Checks a seat like cortex `validate_ghost_spiffe_seat` (REQ-0125-03).
/// This check is stricter: segments use `[a-z0-9-]` and stop at 63 bytes.
fn validate_seat(seat: &str) -> Result<GhostSeat, GhostBindingError> {
    check_text("spiffe_seat", seat, MAX_SPIFFE_SEAT_BYTES)?;
    let invalid = |reason| GhostBindingError::InvalidSpiffeSeat { reason };
    let rest = seat
        .strip_prefix(GHOST_SPIFFE_AGENT_PREFIX)
        .ok_or_else(|| invalid("expected the spiffe://unsigned.gg/agent/ prefix"))?;
    let (team, slug) = rest
        .split_once('/')
        .ok_or_else(|| invalid("expected TEAM/SLUG after the prefix"))?;
    check_segment(team)?;
    check_segment(slug)?;
    Ok(GhostSeat(seat.to_owned()))
}

fn check_segment(segment: &str) -> Result<(), GhostBindingError> {
    let invalid = |reason| GhostBindingError::InvalidSpiffeSeat { reason };
    if segment.is_empty() {
        return Err(invalid("a segment is empty"));
    }
    if segment.len() > MAX_SEAT_SEGMENT_BYTES {
        return Err(invalid("a segment is longer than 63 bytes"));
    }
    if !segment.bytes().all(is_slug_byte) {
        return Err(invalid("a segment holds a character outside [a-z0-9-]"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wire(manifestation: GhostManifestation) -> GhostBindingWire {
        let seat = manifestation
            .supports_persistent_seat()
            .then(|| "spiffe://unsigned.gg/agent/platform/sadr".to_owned());
        GhostBindingWire {
            ghost_id: "sadr".to_owned(),
            manifestation,
            mitc_principal: "ctodie".to_owned(),
            spiffe_seat: seat,
            molt_epoch: 3,
            state: None,
        }
    }

    fn err(w: GhostBindingWire) -> GhostBindingError {
        validate(w).expect_err("binding must be refused")
    }

    // Pinned to cortex `crates/cortex-domain/src/ghost.rs` at main:
    // `GhostManifestation` (:29, `snake_case`), `GhostLifecycleState` (:101,
    // `snake_case`), `GhostLifecycleRecord` (:428: `ghost_id`, `manifestation`,
    // `mitc_principal`, `spiffe_seat`, `molt_epoch`) and
    // `GHOST_SPIFFE_AGENT_PREFIX` (:21). The `state` field is optional here and
    // omitted when `None` (spec 0004). This test pins littleguy's side only. It
    // cannot detect cortex drift, because there is no cortex dependency.
    #[test]
    fn serialized_shape_matches_cortex_contract() {
        let json =
            serde_json::to_value(validate(wire(GhostManifestation::Agent)).unwrap()).unwrap();
        let expected = serde_json::json!({
            "ghost_id": "sadr",
            "manifestation": "agent",
            "mitc_principal": "ctodie",
            "spiffe_seat": "spiffe://unsigned.gg/agent/platform/sadr",
            "molt_epoch": 3,
        });
        assert_eq!(json, expected);
        let mut with_state = wire(GhostManifestation::Agent);
        with_state.state = Some(GhostLifecycleState::Awake);
        let json = serde_json::to_value(validate(with_state).unwrap()).unwrap();
        assert_eq!(json["state"], serde_json::json!("awake"));
        assert_eq!(GHOST_SPIFFE_AGENT_PREFIX, "spiffe://unsigned.gg/agent/");
        for (m, s) in [
            (GhostManifestation::Persona, "persona"),
            (GhostManifestation::Muse, "muse"),
            (GhostManifestation::Agent, "agent"),
            (GhostManifestation::Crew, "crew"),
            (GhostManifestation::Revenant, "revenant"),
        ] {
            assert_eq!(serde_json::to_value(m).unwrap(), serde_json::json!(s));
            assert_eq!(m.as_str(), s);
        }
        let states = [
            (GhostLifecycleState::Minted, "minted"),
            (GhostLifecycleState::Paired, "paired"),
            (GhostLifecycleState::Embodied, "embodied"),
            (GhostLifecycleState::Awake, "awake"),
            (GhostLifecycleState::Quiescing, "quiescing"),
            (GhostLifecycleState::Dormant, "dormant"),
            (GhostLifecycleState::Dreaming, "dreaming"),
            (GhostLifecycleState::Quarantined, "quarantined"),
            (GhostLifecycleState::Retired, "retired"),
        ];
        for (s, text) in states {
            assert_eq!(serde_json::to_value(s).unwrap(), serde_json::json!(text));
            assert_eq!(s.to_string(), text);
        }
    }

    #[test]
    fn crew_with_seat_is_refused() {
        let mut w = wire(GhostManifestation::Crew);
        w.spiffe_seat = Some("spiffe://unsigned.gg/agent/platform/crew-1".to_owned());
        assert_eq!(err(w), GhostBindingError::CrewHoldsSeat);
    }

    #[test]
    fn crew_in_dormant_is_refused() {
        let mut w = wire(GhostManifestation::Crew);
        w.state = Some(GhostLifecycleState::Dormant);
        assert_eq!(
            err(w),
            GhostBindingError::CrewForbiddenState {
                state: GhostLifecycleState::Dormant
            }
        );
    }

    #[test]
    fn crew_in_dreaming_is_refused() {
        let mut w = wire(GhostManifestation::Crew);
        w.state = Some(GhostLifecycleState::Dreaming);
        assert_eq!(
            err(w),
            GhostBindingError::CrewForbiddenState {
                state: GhostLifecycleState::Dreaming
            }
        );
    }

    #[test]
    fn crew_without_seat_is_accepted() {
        let mut w = wire(GhostManifestation::Crew);
        w.state = Some(GhostLifecycleState::Awake);
        let b = validate(w).unwrap();
        assert_eq!(b.spiffe_seat(), None);
        assert_eq!(b.molt_epoch(), 3);
    }

    #[test]
    fn persistent_without_seat_is_refused() {
        for m in [
            GhostManifestation::Persona,
            GhostManifestation::Muse,
            GhostManifestation::Agent,
            GhostManifestation::Revenant,
        ] {
            let mut w = wire(m);
            w.spiffe_seat = None;
            assert_eq!(err(w), GhostBindingError::MissingSeat { manifestation: m });
        }
    }

    #[test]
    fn persistent_with_valid_seat_is_accepted() {
        for m in [
            GhostManifestation::Persona,
            GhostManifestation::Muse,
            GhostManifestation::Agent,
            GhostManifestation::Revenant,
        ] {
            let b = validate(wire(m)).unwrap();
            assert_eq!(b.ghost_id(), "sadr");
            assert_eq!(b.manifestation(), m);
            assert_eq!(b.mitc_principal(), "ctodie");
            assert_eq!(
                b.spiffe_seat(),
                Some("spiffe://unsigned.gg/agent/platform/sadr")
            );
        }
    }

    #[test]
    fn malformed_seats_are_refused() {
        let prefix = "expected the spiffe://unsigned.gg/agent/ prefix";
        let shape = "expected TEAM/SLUG after the prefix";
        let empty = "a segment is empty";
        let charset = "a segment holds a character outside [a-z0-9-]";
        let bad = [
            ("agent/platform/sadr", prefix),
            ("spiffe://unsigned.gg/agent/platform", shape),
            ("spiffe://unsigned.gg/agent//sadr", empty),
            ("spiffe://unsigned.gg/agent/platform/", empty),
            ("spiffe://unsigned.gg/agent/platform/sadr/extra", charset),
            ("spiffe://unsigned.gg/agent/Platform/sadr", charset),
            ("spiffe://unsigned.gg/agent/platform/sa_dr", charset),
            ("spiffe://unsigned.gg/agent/platform/sa.dr", charset),
            ("spiffe://unsigned.gg/agent/platform/..", charset),
            ("spiffe://unsigned.gg/conclave/ceres/elf/w1", prefix),
            ("spiffe://unsigned.gg/human/ctodie", prefix),
            ("spiffe://cerebral.internal/agent/platform/sadr", prefix),
            ("spiffe://unsigned.gg.evil/agent/platform/sadr", prefix),
            ("spiffe://unsigned.gg/agent/platform/sadr ", charset),
            ("spiffe://unsigned.gg/agent/platform/sadr?x=1", charset),
        ];
        for (seat, reason) in bad {
            let mut w = wire(GhostManifestation::Agent);
            w.spiffe_seat = Some(seat.to_owned());
            assert_eq!(
                err(w),
                GhostBindingError::InvalidSpiffeSeat { reason },
                "seat `{seat}` must be refused with that reason"
            );
        }
        let mut w = wire(GhostManifestation::Agent);
        w.spiffe_seat = Some(String::new());
        assert_eq!(
            err(w),
            GhostBindingError::EmptyField {
                field: "spiffe_seat"
            }
        );
    }

    #[test]
    fn empty_fields_are_refused() {
        let mut w = wire(GhostManifestation::Agent);
        w.ghost_id = String::new();
        assert_eq!(err(w), GhostBindingError::EmptyField { field: "ghost_id" });
        let mut w = wire(GhostManifestation::Agent);
        w.mitc_principal = String::new();
        assert_eq!(
            err(w),
            GhostBindingError::EmptyField {
                field: "mitc_principal"
            }
        );
        let mut w = wire(GhostManifestation::Agent);
        w.spiffe_seat = Some(String::new());
        assert_eq!(
            err(w),
            GhostBindingError::EmptyField {
                field: "spiffe_seat"
            }
        );
        let mut w = wire(GhostManifestation::Agent);
        w.mitc_principal = "   ".to_owned();
        assert_eq!(
            err(w),
            GhostBindingError::InvalidField {
                field: "mitc_principal"
            }
        );
    }

    #[test]
    fn oversized_fields_are_refused() {
        let mut w = wire(GhostManifestation::Agent);
        w.ghost_id = "a".repeat(MAX_GHOST_ID_BYTES + 1);
        assert_eq!(
            err(w),
            GhostBindingError::FieldTooLong {
                field: "ghost_id",
                max: MAX_GHOST_ID_BYTES
            }
        );
        let mut w = wire(GhostManifestation::Agent);
        w.mitc_principal = "p".repeat(MAX_MITC_PRINCIPAL_BYTES + 1);
        assert_eq!(
            err(w),
            GhostBindingError::FieldTooLong {
                field: "mitc_principal",
                max: MAX_MITC_PRINCIPAL_BYTES
            }
        );
        let mut w = wire(GhostManifestation::Agent);
        w.spiffe_seat = Some(format!(
            "{GHOST_SPIFFE_AGENT_PREFIX}team/{}",
            "s".repeat(MAX_SPIFFE_SEAT_BYTES)
        ));
        assert_eq!(
            err(w),
            GhostBindingError::FieldTooLong {
                field: "spiffe_seat",
                max: MAX_SPIFFE_SEAT_BYTES
            }
        );
        let mut w = wire(GhostManifestation::Agent);
        w.spiffe_seat = Some(format!(
            "{GHOST_SPIFFE_AGENT_PREFIX}team/{}",
            "s".repeat(MAX_SEAT_SEGMENT_BYTES + 1)
        ));
        assert_eq!(
            err(w),
            GhostBindingError::InvalidSpiffeSeat {
                reason: "a segment is longer than 63 bytes"
            }
        );
        let mut w = wire(GhostManifestation::Agent);
        w.spiffe_seat = Some(format!(
            "{GHOST_SPIFFE_AGENT_PREFIX}team/{}",
            "s".repeat(MAX_SEAT_SEGMENT_BYTES)
        ));
        assert!(validate(w).is_ok());
    }

    #[test]
    fn bad_field_characters_are_refused() {
        for id in ["Sadr", "sa_dr", "sa dr", "sadr\n", "sadr\u{e9}"] {
            let mut w = wire(GhostManifestation::Agent);
            w.ghost_id = id.to_owned();
            assert_eq!(
                err(w),
                GhostBindingError::InvalidField { field: "ghost_id" }
            );
        }
        for p in [" ctodie", "ctodie ", "cto\u{0}die"] {
            let mut w = wire(GhostManifestation::Agent);
            w.mitc_principal = p.to_owned();
            assert_eq!(
                err(w),
                GhostBindingError::InvalidField {
                    field: "mitc_principal"
                }
            );
        }
    }

    #[test]
    fn state_is_kept_when_valid() {
        let mut w = wire(GhostManifestation::Muse);
        w.state = Some(GhostLifecycleState::Dormant);
        let b = validate(w).unwrap();
        assert_eq!(b.state(), Some(GhostLifecycleState::Dormant));
    }

    #[test]
    fn error_messages_name_the_cause() {
        assert_eq!(
            GhostBindingError::CrewHoldsSeat.to_string(),
            "a crew ghost cannot hold a persistent spiffe seat"
        );
        assert_eq!(
            GhostBindingError::MissingSeat {
                manifestation: GhostManifestation::Muse
            }
            .to_string(),
            "a `muse` ghost needs a persistent spiffe seat"
        );
    }

    #[test]
    fn deserialize_runs_the_validator() {
        let ok =
            r#"{"ghost_id":"x1","manifestation":"crew","mitc_principal":"ctodie","molt_epoch":0}"#;
        let b: GhostBinding = serde_json::from_str(ok).unwrap();
        assert_eq!(b.ghost_id(), "x1");
        let bad = r#"{"ghost_id":"x1","manifestation":"crew","mitc_principal":"ctodie","molt_epoch":0,"spiffe_seat":"spiffe://unsigned.gg/agent/t/x1"}"#;
        let refused = serde_json::from_str::<GhostBinding>(bad).unwrap_err();
        assert!(refused.to_string().contains("spiffe seat"), "{refused}");
        let unknown = r#"{"ghost_id":"x1","manifestation":"wizard","mitc_principal":"ctodie","molt_epoch":0}"#;
        let refused = serde_json::from_str::<GhostBinding>(unknown).unwrap_err();
        assert!(refused.to_string().contains("wizard"), "{refused}");
        let dormant_crew = r#"{"ghost_id":"x1","manifestation":"crew","mitc_principal":"ctodie","molt_epoch":0,"state":"dormant"}"#;
        let refused = serde_json::from_str::<GhostBinding>(dormant_crew).unwrap_err();
        assert!(refused.to_string().contains("dormant"), "{refused}");
        let round: GhostBindingWire = b.clone().into();
        assert_eq!(GhostBinding::try_from(round).unwrap(), b);
    }

    #[test]
    fn deserialize_refuses_unknown_fields() {
        let json = r#"{"ghost_id":"x1","manifestation":"crew","mitc_principal":"ctodie","molt_epoch":0,"extra":1}"#;
        let refused = serde_json::from_str::<GhostBinding>(json).unwrap_err();
        assert!(
            refused.to_string().contains("unknown field `extra`"),
            "{refused}"
        );
    }

    // A full cortex `GhostLifecycleRecord` is refused by design. The sender
    // projects it to the binding fields first.
    #[test]
    fn full_cortex_record_fields_are_refused() {
        let json = r#"{"ghost_id":"x1","manifestation":"crew","mitc_principal":"ctodie","molt_epoch":0,"shell_id":"ceres/w1:p1"}"#;
        let refused = serde_json::from_str::<GhostBinding>(json).unwrap_err();
        assert!(
            refused.to_string().contains("unknown field `shell_id`"),
            "{refused}"
        );
    }
}
