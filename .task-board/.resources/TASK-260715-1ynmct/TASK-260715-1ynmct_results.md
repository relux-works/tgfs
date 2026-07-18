# TASK-260715-1ynmct — Message normalization: implementation notes

Status: ready for review. Full gate `make check` green (8/8, provenance
`.temp/acceptance/local-all`).

## What was built

A pure, sans-IO message normalizer in `gramdrive-source-tdjson`:

- `crates/gramdrive-source-tdjson/src/message.rs` (new) — one TDLib
  `message` JSON object becomes one typed, provider-neutral
  `MessageRecord`. Entry points:
  - `normalize_message(&Value) -> Result<MessageRecord, MessageError>` —
    the full-message path (history crawl pages, `updateNewMessage`).
  - `normalize_content(&Value, ProtectionFacts)` — the content-only path
    `updateMessageContent` needs (TASK-260715-37nhe5 / 10p5zp reuse).
  - `normalize_reactions(&Value)` — the `messageInteractionInfo` path
    `updateMessageInteractionInfo` needs.
- `crates/gramdrive-source-tdjson/tests/message_normalization.rs` (new) —
  the PRD-022 fixture corpus, realistic tdjson wire shapes (int64 fields
  as decimal strings, omitted default fields, extra ignored fields).
- `wire.rs`: `parse_order` renamed to `parse_int64` — it was always the
  general tdjson int64-as-string decoder; the album id and custom emoji
  ids need it too. Call sites in `snapshot.rs`/`updates.rs` updated.
- `lib.rs`: module registered, vocabulary re-exported, crate docs updated.

## Record coverage (PRD-022 / DoD)

Identity (chat_id, message_id), time (`sent_at_ms`/`edited_at_ms`,
milliseconds by the boundary rule), sender (`SenderRef`: user/chat/
unknown), text+entities and captions (`FormattedText`, UTF-16 offsets
verbatim, full `TextEntityKind` vocabulary), reply target
(`ReplyTarget`: message with optional cross-chat id and quote, story,
unknown), topic (`TopicRef`: forum/direct-messages/saved-messages/
unknown), album grouping (`album_id` = `media_album_id` as the grouping
key; assembling the group is the consumer's join), reactions
(`Reaction`/`ReactionKind`: emoji/custom/paid/unknown), service actions
(`ServiceAction`, 19 modeled narratable actions), edit revision
observation (`edited_at_ms`; retention policy is TASK-260715-37nhe5),
protection (POL-4: `can_be_saved` verbatim, `self_destruct`, derived
`AttachmentAvailability` on every descriptor), attachment descriptors
(`AttachmentDescriptor`: kind, local file id, remote id + unique id
(PRD-033 dedup key), original name/MIME/size (PRD-032), dimensions,
duration) for all PRD-030 v1 classes (photo largest-size selection,
video, animation, audio, document, voice note, video note, sticker).

## Degradation model (PRD-024, AC "unknown content degrades explicitly")

Strict about identity, degrading about everything else:

- No integer `id`/`chat_id`/`date`, no `content`, or typeless content →
  typed `MessageError::Malformed`. Never a guessed record.
- Unknown content `@type`, or a known type with broken required members →
  `MessageContent::Unsupported { raw_type, raw_json, raw_schema_version }`
  — the one place raw TDLib JSON is preserved (versioned raw preservation
  only where required for migration, per task scope). `raw_json` is
  compact sorted-key JSON → deterministic bytes; round-trip losslessness
  is asserted in tests.
- Unknown sender/reply/topic/reaction/self-destruct shapes → that
  vocabulary's own `Unknown` variant; the rest of the record survives.
- Unknown self-destruct flavors still count as self-destructing →
  availability fails closed to `ViewOnce` (POL-4).
- Structurally broken text entities are dropped (text never lost, only
  broken decoration); unknown entity *types* keep their span as
  `TextEntityKind::Unknown`.
- Expired self-destruct placeholders (`messageExpired*`) →
  `MessageContent::Expired` — explicit unavailability, no fabricated
  recoverability.

## Deliberate scope exclusions (documented in module docs)

Forward origins, view/forward counters, reply markup, paid-content
accounting (`is_pinned`, `via_bot_user_id`, `author_signature` likewise)
— outside the PRD-022 v1 fact list. Polls/locations/contacts/invoices
etc. degrade to `Unsupported` with raw preserved; a future task can
promote them without data loss.

## Schema verification

All wire shapes were verified against the authoritative `td_api.tl` of
the pinned TDLib commit `022d60202e446ad1287b9fb68e687c8a0760788b`
(`.temp/tdlib/src`), not against memory. Notable findings recorded in the
logbook (message carries `topic_id:MessageTopic`, no `message_thread_id`;
tdjson omits default-valued fields, so absent `can_be_saved` *is* the
protected shape — the normalizer defaults it to `false`, fail-closed).

## Verification

- `cargo test -p gramdrive-source-tdjson` — 98 lib tests (10 new unit
  tests in `message::tests`) + 17 new fixture-corpus integration tests,
  all passing.
- `make check` — all 8 gates green: toolchain, format, lint
  (clippy -D warnings), workspace tests, architecture, supply chain,
  traceability, script self-tests.

## Alignment with the state layer (STORY-260715-16ik2x)

Field vocabulary deliberately mirrors what `gramdrive-state` stores:
`sent_at_ms`/`edited_at_ms` match `messages` columns; the descriptor maps
onto `AttachmentFacts` (remote_unique_id → telegram_unique_id, remote_id
→ telegram_file_id, name/MIME/size, availability, can_be_saved);
`MessageRecord` is the intended `message_events.payload` value — its
serialization (payload bytes + `payload_schema`) is owned by the event
writer (TASK-260715-10p5zp), not this task.
