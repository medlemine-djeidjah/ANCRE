# AI Act requirement → evidence schema

**Status: stub.** Fill during M1, publish in August, in French. This is a
byproduct of freezing `audit_events` (mvp-plan §4) — nobody else has published
one, and it is the best lead magnet available (mvp-plan §6).

The claim this table makes, and the only one it may make: *here is the column
that carries the evidence for this obligation.* It does not claim conformity.
No harmonised standards exist yet, so no product can deliver conformity — we
sell evidence, traceability and readiness. Overclaiming here loses deals at
legal review (PRD §5).

| Article | Obligation | Carried by | Notes |
|---|---|---|---|
| 12(1) | Automatic recording of events over the lifetime | `audit_events`, whole row | Append-only enforced by hash chain, not by the engine |
| 12(2)(a) | Identify situations that may constitute a substantial modification | `config.generation.applied` + `ChangeClass` | Surfaces candidates. Never declares. Legal determination |
| 12(2)(b) | … | | |
| 12(2)(c) | … | | |
| 14 | Demonstrable human oversight | *V1* — oversight API | Not in MVP |
| 15 | Accuracy, robustness, cybersecurity | *V1* — eval runs | Not in MVP |
| 19 | Provider log retention ≥6 months | `RETENTION_FLOOR_DAYS` | Un-lowerable by configuration |
| 26(6) | Deployer log retention ≥6 months | same | |
| 50 | Transparency / disclosure | *V1-8* | Live and enforceable today — the wedge |
| GDPR 17 | Right to erasure | `subject_key_id` | Crypto-shredding, V1-6. Column exists now so V1 does not bump `canon_version` |

**To do:** every remaining 12(2) subparagraph; Annex IV cross-reference; the
French translation, which is the version that actually gets read by the buyer.
