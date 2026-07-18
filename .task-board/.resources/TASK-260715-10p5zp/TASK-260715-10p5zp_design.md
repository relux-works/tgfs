# TASK-260715-10p5zp — Ordered update loop and gap recovery — design

## Placement

New module `crates/gramdrive-source-tdjson/src/live.rs`: `LiveMachine`, the
deterministic sans-IO live *message* update loop. Sibling of:

- `updates.rs` (`UpdateMachine`) — chat metadata/list live mapper (done, 1c8fea)
- `history.rs` (`CrawlMachine`) — resumable backfill (done, 26dnp6)
- `message.rs` (`normalize_message`) — the shared normalizer (done, 1ynmct)

Architecture direction: source-tdjson depends only on model+source; state is a
dev-dependency for the integration suite (established pattern).

## Contract with the crawl (the crawl/live boundary)

`history.rs` docs state it: the crawl's catch-up phase reconnects a chat's
committed window to the present; **the live loop only ever extends `newest`**.
Corollaries baked into the design:

1. The live machine NEVER establishes a window where none exists (anchoring is
   the crawl's job with its completeness-reset discipline — the anchor-gap fix).
   Windowless chats get boundary-free commits: records persist, cursor untouched.
2. A live commit expresses the boundary as `advance_newest: Option<i64>` —
   "raise stored newest to at least this id". The caller merges with the stored
   window *inside the same transaction* (keep stored `oldest` and
   `history_complete`). This is what makes concurrent backfill safe: the crawl
   moves `oldest` down while the live loop moves `newest` up, and neither
   clobbers the other.
3. Records above/outside the window are safe to persist any time (idempotent
   replay, SYNC-021); only the *cursor* claims contiguity. So observations are
   never gated; only `advance_newest` is.

## Gap detection and recovery (SYNC-023)

Per-chat boundary state:

- `Floating` — no committed window: never advance (see corollary 1).
- `Unverified { newest }` — committed window exists; this session has not yet
  proven the stream is contiguous with it.
- `Bridging { newest, top, floor }` — a live message above `newest` arrived on
  an Unverified chat: messages may have landed while offline. Targeted
  re-fetch: `getChatHistory(chat, from=floor|0, …)` pages descend (exact
  catch-up protocol of the crawl, same page-contract validation) until a page
  reaches `newest` (or history exhausts). Bridge pages commit records under the
  *unchanged* cursor; the connecting page's commit carries
  `advance_newest = max(top, live_top, newest)`. Gap recovered strictly before
  cursor publication.
- `Verified { newest }` — contiguity proven; each live message `id > newest`
  advances the cursor with its own commit.
- `Frozen` — the bridge failed (chat-level TDLib rejection or page-contract
  violation): reported once as `LiveStep::Degraded` (reusing
  `history::UnavailableReason`), records keep flowing boundary-free, the cursor
  never advances this session; the next crawl run owns recovery.

Crash mid-bridge: durable newest still the old value; a fresh machine replans
Unverified and re-bridges; replay is idempotent (SYNC-021) — the AC "never
advance cursor without state" holds because the advance rides the same
transaction as the connecting records, after the gap pages are already down.

## Update surface

- `updateNewMessage` — normalize; messages with a `sending_state` are skipped
  (provisional ids); boundary logic above.
- `updateMessageSendSucceeded` — the final message ingests exactly like a new
  message (its provisional twin was never ingested).
- `updateMessageContent` / `updateMessageEdited` — an *edit signal*: both are
  partial (content without edit_date / edit_date without content), so the loop
  re-fetches the full message with `getMessage` (targeted re-fetch, one
  consistent snapshot, coalesced per message id), normalizes, and commits the
  revision. The state layer classifies observed-vs-edited and guards stale
  replays (monotonic revised-at) and post-deletion resurrection.
- `updateDeleteMessages` — only `is_permanent && !from_cache` counts
  (from_cache is cache eviction, not deletion). Emits ordered
  `LiveChange::Deleted`; deletion of a never-observed message is skipped by the
  state layer (POL-3: never imply unobserved history).
- `updateMessageInteractionInfo` is deliberately NOT consumed in v1: reaction/
  view tallies change constantly; folding every tick into the append-only event log
  (POL-3) would be pathological growth. Reactions refresh whenever the message
  is re-observed (history re-fetch, edit refresh). Documented boundary.

## Untracked chats

An update naming a chat outside the plan buffers (order preserved) and reports
`LiveStep::Unresolved { chat_id }` once; the caller resolves the chat through
the chat machinery (canonical row must exist first — FK), then calls
`track_chat(chat_id, newest)`, which replays the buffer through the normal
paths. No forged rows, no dropped messages.

## Concurrency/pacing

One outstanding request machine-wide (SEC-031), flood-wait/transport →
`LiveBackoff` advice + identical re-issue (SYNC-044/TGC-22), runtime-level
failures are fatal for the machine (recovery = fresh machine from durable
cursors). Commits drain before new fetches start (checkpoint early, bounded
memory). All iteration orders deterministic (BTreeMap/BTreeSet).

## Step vocabulary

`LiveStep::{Submit(Value), Backoff(LiveBackoff), Commit(Box<LiveCommit>),
Unresolved{chat_id}, Degraded(Box<ChatDegraded>), Idle}`;
`LiveCommit { chat_id, changes: Vec<LiveChange>, advance_newest,
skipped_malformed, refreshes_rejected }`;
`LiveChange::{Observed(MessageRecord), Deleted{message_id}}`;
`LiveError::{Plan, Request, Malformed, NoRequestOutstanding}` — mirrors crawl.

Timestamps: the machine reads no clock; `observed_at_ms` is stamped by the
caller at commit time (SYNC-073), exactly like the crawl suite does.

## Test plan

Unit (module): plan validation; boundary transitions incl. bridge connect on
exact/at-below/empty pages; bridge multi-page floor descent; live records
during bridge; sending_state skip + send-succeeded remap; edit coalescing;
delete filtering (from_cache); untracked buffering + track_chat replay; frozen
after rejection/page-contract; flood backoff identical re-issue; duplicate/
out-of-order convergence; deterministic drain order; poisoned-after-misuse.

Integration (`tests/live_updates.rs`, store-backed like history_crawl.rs):
- gap recovery before publication: offline messages exist in fixture history;
  live message triggers bridge; assert stored newest unchanged until the
  connecting commit, then all gap messages present and newest advanced.
- crash/replay at every commit boundary: apply k commits, restart machine from
  durable rows, re-feed the same updates; byte-identical convergence, no
  duplicate events, cursor never ahead of state.
- duplicates/out-of-order: re-fed updates and edit-after-delete converge; no
  resurrection; event counts stable.
- crawl/live interplay: interleaved CrawlMachine backfill commits and live
  commits — final window [crawl-oldest, live-newest], no lost events.
- edits via getMessage round-trip (one fetch per edit burst, edited event
  exactly once); deletes honored; runtime round-trip suite over mock tdjson
  for payload fidelity.
