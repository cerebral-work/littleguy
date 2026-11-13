---
status: in-progress
created: 2026-11-13
tags: [littleguy, ghost, cortex, spiffe, mandate, littleman, escapement]
priority: high
depends_on:
  - 0001-substrate-seat-and-biscuit-mandate-integration
---

# 0004 - GhostBinding: a zero-I/O validator that mirrors the cortex GHOST contract

## 1. Overview

A littleman pool delivery carries a `ghost_binding`. littleguy must check it
before it embodies the Ghost. The contract lives in cortex
(`crates/cortex-domain/src/ghost.rs`, `specs/0125`, REQ-0125-01 to -04).
littleguy does not depend on cortex. It mirrors the field set and the rules,
and a test pins the serialized shape so drift shows up.

Nothing in this spec does network, file or database I/O.

## 2. Mirrored contract

| littleguy item | cortex source |
|---|---|
| `GhostManifestation` (`persona`, `muse`, `agent`, `crew`, `revenant`) | `ghost.rs` `GhostManifestation` |
| `GhostLifecycleState` (nine states) | `ghost.rs` `GhostLifecycleState` |
| `GhostBinding` fields `ghost_id`, `manifestation`, `mitc_principal`, `spiffe_seat`, `molt_epoch` | `ghost.rs` `GhostLifecycleRecord` |
| Seat URI `spiffe://unsigned.gg/agent/TEAM/SLUG` | `ghost.rs` `GHOST_SPIFFE_AGENT_PREFIX`, `validate_ghost_spiffe_seat` |
| Crew refuses a seat, `dormant` and `dreaming` | `ghost.rs` `supports_persistent_seat`, `validate_transition_for` |

`persona_digest` and `handoff_ref` have no cortex counterpart. They are
littleguy-side optional fields, bounded below. `lifecycle_state` is optional
here: a delivery may or may not state it.

Deliberate differences from cortex, both stricter:
- A seat segment (team or slug) is limited to `[a-z0-9-]`. Cortex also allows `_`.
- Unknown JSON fields are refused.

Cortex does not require the seat slug to equal `ghost_id`, so this spec does not.

## 3. Bounds

| Field | Rule |
|---|---|
| `ghost_id` | 1 to 64 bytes, `[a-z0-9-]` |
| `mitc_principal` | 1 to 128 bytes, no control characters, no leading or trailing space |
| `spiffe_seat` | at most 256 bytes |
| seat team, seat slug | 1 to 63 bytes each, `[a-z0-9-]` |
| `persona_digest` | exactly 64 lowercase hex characters (SHA-256) |
| `handoff_ref` | 1 to 256 bytes, printable ASCII, no spaces |

## 4. Requirements (EARS)

- **REQ-LG-GHOST-01:** The `ghost_binding` module shall define `GhostBinding` and `GhostManifestation` with the cortex field names and the cortex `snake_case` manifestation strings.
  Proof: test `serialized_shape_matches_cortex_contract`.
- **REQ-LG-GHOST-02:** If a `Crew` binding carries a `spiffe_seat`, then the validator shall refuse it with `CrewHoldsSeat`.
  Proof: test `crew_with_seat_is_refused`.
- **REQ-LG-GHOST-03:** If a `Crew` binding names the `dormant` or `dreaming` lifecycle state, then the validator shall refuse it with `CrewForbiddenState`.
  Proof: tests `crew_in_dormant_is_refused`, `crew_in_dreaming_is_refused`.
- **REQ-LG-GHOST-04:** Where a manifestation is persistent (`persona`, `muse`, `agent`, `revenant`), the validator shall require a `spiffe_seat` of the form `spiffe://unsigned.gg/agent/TEAM/SLUG`.
  Proof: tests `persistent_without_seat_is_refused`, `persistent_with_valid_seat_is_accepted`.
- **REQ-LG-GHOST-05:** If a seat URI is malformed, uses another trust domain or path family, or has a segment outside `[a-z0-9-]`, then the validator shall refuse it with `InvalidSpiffeSeat`.
  Proof: test `malformed_seats_are_refused`.
- **REQ-LG-GHOST-06:** If a string field is empty or longer than its cap in section 3, then the validator shall refuse it with a typed error and shall not panic.
  Proof: tests `empty_fields_are_refused`, `oversized_fields_are_refused`.
- **REQ-LG-GHOST-07:** When `GhostBinding` is deserialized, the deserializer shall run the validator and shall refuse unknown fields.
  Proof: tests `deserialize_runs_the_validator`, `deserialize_refuses_unknown_fields`.
- **REQ-LG-GHOST-08:** `BiscuitMandateClaims` shall accept `spiffe://unsigned.gg/agent/TEAM/SLUG` and `spiffe://unsigned.gg/conclave/CELL/elf/NAME`, and shall refuse other trust domains, `/human/` paths and unknown path shapes.
  Proof: tests `claims_accept_agent_and_elf_spiffe_forms`, `claims_refuse_foreign_human_and_unknown_spiffe`.
- **REQ-LG-GHOST-09:** Where a `SeatReceipt` carries a `GhostBinding`, the receipt shall expose `ghost_id` and `molt_epoch` through it.
  Proof: test `seat_receipt_carries_ghost_binding`.
