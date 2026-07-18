# Review verdict: ACCEPTED (-> done)

Reviewer independently verified, not just re-read the implementer notes:

## Evidence

1. **Tests green, gates green (re-run, not trusted from notes):**
   - cargo test -p gramdrive-source-tdjson: all pass, incl. 17 fixture-corpus
     integration tests (tests/message_normalization.rs) + 11 message unit tests.
   - make check: 8/8 (toolchain, format, clippy -D warnings, workspace tests,
     architecture, supply-chain, traceability, scripts). Provenance
     .temp/acceptance/local-all.

2. **Wire shapes independently spot-checked against pinned td_api.tl
   (022d60202e446ad1287b9fb68e687c8a0760788b, .temp/tdlib/src):** message
   envelope (topic_id:MessageTopic, media_album_id:int64, self_destruct_type,
   can_be_saved, reply_to, interaction_info), messageReplyToMessage/Story field
   names (story_poster_chat_id confirmed), messageTopicForum/DirectMessages/
   SavedMessages member names, reactionTypeEmoji/CustomEmoji/Paid,
   messageReaction/messageReactions/messageInteractionInfo nesting, textEntity,
   file/remoteFile, every media content member path (photo.sizes[].photo,
   video.video, voice_note.voice, video_note.video incl. length-as-diameter,
   sticker.sticker), is_secret placement, all 4 messageExpired*, and all 19
   modeled service-action shapes. All match the code.

3. **AC coverage:** PRD-022 fact list fully mapped (identity, time-ms, sender,
   text/caption entities, reply, topic, album key, reactions, edits, service
   actions, POL-4 protection, PRD-030 v1 attachment descriptors with PRD-032
   name/MIME/size and PRD-033 remote_unique_id). Unknown content degrades
   explicitly: typed Unsupported with versioned raw JSON (round-trip asserted),
   per-vocabulary Unknown variants for periphery, typed Malformed only for
   broken identity. Fail-closed POL-4 (absent can_be_saved = protected; unknown
   self-destruct flavor = ViewOnce) verified in code and tests.

4. **Architecture fit:** pure sans-IO functions over serde_json::Value, no new
   deps, no state-crate linkage; parse_order -> parse_int64 rename is a clean
   generalization with call sites updated; architecture gate green.

## Non-blocking observations (no rework requested)

- messageText with an absent text member degrades to an empty Text record
  rather than Unsupported — module docs say broken required members go to
  Unsupported. No observable data loss (absent field carries no data; link
  previews are out of v1 scope), but the doc sentence slightly overstates.
  Fine to fold into a later docs touch-up.
- Pinned schema messagePhoto also carries video:video (photo+video shape);
  ignored like other out-of-scope periphery — consistent with the documented
  v1 scope exclusion.
