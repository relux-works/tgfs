# Telegram API Compliance Checklist

Status: baseline (research handoff)
Last updated: 2026-07-17
Owner task: TASK-260715-pyqm1k (telegram-api-compliance, SEC-030)
Scope: local-first clients (v1 macOS per POL-5) and the optional remote tier (DEC-005)
Primary sources: all citations verified against live pages on 2026-07-17 (see § Source register)

This checklist translates the Telegram API Terms of Service and related developer
documentation into verifiable controls and maps each applicable rule to the board
element that implements or validates it. It complements `docs/TRACEABILITY.md`
(requirement → board mapping); this file is the rule-level authority for
Telegram-specific obligations behind SEC-030..032, SEC-051, and POL-4/POL-7.

Rule IDs (`TGC-nn`) are stable and may be cited from board task ACs and reviews.

## How to read

- **Rule** — the obligation, with a verbatim citation from the primary source.
- **Applies to** — `v1` (local-first, macOS-first), `remote` (optional remote tier),
  `release` (release-time/operational control), `n/a` (analyzed, not applicable — with rationale).
- **Control** — the concrete, verifiable behavior GramDrive must exhibit.
- **Owner** — board element(s) whose acceptance criteria implement or validate the control.
  Rules without an owner are collected in § Gaps — there are no silent gaps.

## 1. API Terms of Service — core.telegram.org/api/terms

### Privacy and non-interference

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-01 | ToS 1.1: "All client apps must, therefore, guard their users' privacy with utmost care and comply with our Security Guidelines." | v1, remote | Never hand-roll MTProto: local tier uses official TDLib; remote tier uses gotd/td behind its own threat model. Privacy program per SEC-010..023 (redaction, no content in analytics). | TASK-260715-2ulon7, TASK-260715-rxjkpi, TASK-260715-1nohav, TASK-260715-4im48n; remote: TASK-260715-3re6t2 (SEC-040) |
| TGC-02 | ToS 1.3: basic features of main Telegram apps must "function correctly and in an expected way"; "It is forbidden to force users of other Telegram clients to download your app in order to view certain messages and content." | v1 (limited) | GramDrive is a read-only archive/drive companion: it sends no messages, alters no content, and produces nothing other users must install anything to view. Read-only capability enforcement guarantees no messaging surface exists to degrade. | TASK-260715-i3mp9x, TASK-260715-11qg88 (DEC-007, NFR-014) |
| TGC-03a | ToS 1.4: forbidden to interfere with basic functionality, incl. "preventing self-destructing content from disappearing". | v1, remote | View-once / self-destructing media is never persisted to the archive, cache, thumbnails, or rendered exports; shown as an unavailable placeholder (POL-4). | TASK-260715-23arcu (SEC-032), TASK-260715-3prhsi (DEC-016, done), renderer states via STORY-260715-1oq9jg (PRD-024) |
| TGC-03b | ToS 1.4: forbidden: "making actions on behalf of the user without the user's knowledge and consent, … tampering with the 'read' statuses of messages (e.g. implementing a 'ghost mode'), preventing typing statuses from being sent/displayed". | v1, remote | The sync engine is read-state-neutral: background discovery/crawl issues only read-only fetches and never emits view/open/read-acknowledgement calls (TDLib `viewMessages`/`openChat` or MTProto equivalents), so it neither marks content read nor fakes views. No action of any kind is taken on behalf of the user. | **Gap G-2** — proposed AC addition to TASK-260715-26dnp6, TASK-260715-10p5zp; validated via TASK-260715-3e8q4m |
| TGC-04 | ToS 1.5: "you are prohibited from using, accessing or aggregating data obtained from the Telegram platform to train, fine-tune or otherwise engage in the development, enhancement or deployment of artificial intelligence, machine learning models and similar technologies." | v1, remote, release | Standing negative constraint (SEC-051): no Telegram-derived content is ever used for AI/ML training or product development datasets. Re-checked whenever analytics/telemetry scope changes; enforced as a release gate. | TASK-260715-1nxcst (POL-8 gate); constraint noted for TASK-260715-4im48n |

### Transparency and branding

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-05 | ToS 2.1: "You must obtain your own api_id for your application." obtaining_api_id: sample/published API IDs cause "API_ID_PUBLISHED_FLOOD error for your users". | v1, remote, release | Product-specific `api_id`/`api_hash` provisioned outside the repo; builds never ship sample credentials; CI secret scanning. | TASK-260716-1iypv4 (done), TASK-260715-1hdnuy (SEC-003), TASK-260715-3bhbkv (NFR-053), TASK-260715-3faqmr (SEC-001) |
| TGC-06 | ToS 2.2: "your users must be aware of the fact that your app uses the Telegram API and is part of the Telegram ecosystem. This fact must be featured prominently in the app's description in the app stores and in the in-app intro if your app has it." | v1, release | Store/website listing and companion-app onboarding both carry a prominent "uses the Telegram API / part of the Telegram ecosystem" disclosure. | **Gap G-1** — proposed AC additions to TASK-260715-13pxnu (in-app intro), TASK-260715-32gjo8 (docs/listing text), TASK-260715-1dk9ik (distribution surface); gated by TASK-260715-1nxcst |
| TGC-07 | ToS 2.3: "the title of your app must not include the word 'Telegram'. An exception can be made if the word 'Telegram' is preceded with the word 'Unofficial'." | v1, release | Public name **GramDrive** contains no "Telegram" (POL-7/DEC-019); all user-visible surfaces use GramDrive. | TASK-260715-7pdgft (done), TASK-260717-3dvved (done) |
| TGC-08 | ToS 2.4: "You must not use the official Telegram logo for your app. Both the Telegram brand and its logo are registered trademarks." | v1, release | App icon and all brand assets are original — no official Telegram logo, no confusingly similar paper-plane mark. Verified at asset creation and at the release gate. | POL-7 records the rule; TASK-260715-13pxnu (app assets), verified by TASK-260715-1nxcst |
| TGC-09 | ToS 3.1–3.2: monetization is allowed, but "you must clearly mention all the methods of monetization that are used in your app in all its app store descriptions." | release (conditional) | If/when GramDrive is sold or monetized (proprietary product per POL-6), every store/website description lists the monetization method(s). No-op until a monetization model exists. | TASK-260715-32gjo8, verified by TASK-260715-1nxcst |

### Advertising — sponsored messages

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-10 | ToS 3.3: "If your app allows accessing content from Telegram channels, you must include support for official sponsored messages in Telegram channels and may not interfere with this functionality." Mechanics: api/sponsored-messages — "Each time the user opens a channel …, messages.getSponsoredMessages must be called … The result must be cached for 5 minutes"; views/clicks reported via `messages.viewSponsoredMessage` / `messages.clickSponsoredMessage`. | **undecided** — owner decision required | GramDrive exposes channel content (media files + rendered history), so clause 3.3 is at least arguably applicable, yet the documented mechanics presume a chat-feed UI that a filesystem projection does not have. See § Gaps G-3 for the analysis and proposed decision task. GramDrive never blocks or strips sponsored messages in other clients (non-interference is trivially satisfied — we do not touch them). | **Gap G-3** — no owning board task; proposed decision task under STORY-260715-1rmrtu, escalation per POL-8 |

### Enforcement process

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-11 | ToS 4.1–4.2: on violation Telegram notifies "the Telegram account responsible for the app"; without a fix "within 10 days" API access is discontinued and app stores are contacted. | release, ops | The Telegram account that owns the production `api_id` is a monitored operational mailbox/account; an ops runbook defines who reacts to a breach notice and the 10-day cure timeline. | **Gap G-4** — proposed AC addition to TASK-260715-32gjo8 (admin/ops docs); reviewed at TASK-260715-1nxcst |

## 2. Developer rules — core.telegram.org/api/obtaining_api_id

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-12 | "Using the Telegram API for flooding, spamming, faking subscriber and view counters of channels" is forbidden; violators "will be banned forever". | v1, remote | Backfill/crawl is bounded and paced (no flooding); engine sends nothing (no spam surface); read-state neutrality (TGC-03b) also means no view-counter inflation — background crawl never registers views. Development uses Telegram **test-DC** accounts, not real accounts. | TASK-260715-mua1ng (SEC-031), TASK-260715-22fh09, TASK-260715-162fdj (NFR-033 observability); test accounts: TASK-260716-1iypv4 (done) |
| TGC-13 | "All accounts that sign up or log in using unofficial Telegram API clients are automatically put under observation to prevent violations." Recovery of banned accounts: recover@telegram.org. | v1, ops | Accept observation as a standing operating condition (risk R-004): conservative default pacing, no gray-area API use, and a documented ban-recovery path (recover@telegram.org) in admin docs. | docs/RISK_REGISTER.md R-004; runbook part of **Gap G-4** (TASK-260715-32gjo8) |
| TGC-14 | "if you use our open source code … you must comply with the terms of the GNU GPL license" (source publication obligations). | v1 | GramDrive is proprietary (POL-6/DEC-018): GPL/AGPL Telegram client code is reference-only — never linked, vendored, or translated verbatim. TDLib is BSL-1.0; gotd/td is MIT. CI license/SBOM scanning enforces the dependency policy. | TASK-260715-2weglw (done), TASK-260715-152wjq (SEC-053), TASK-260715-3faqmr |

## 3. Protected content — core.telegram.org/api/content-protection

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-15 | Chats/channels with `noforwards`: "all messages received from protected groups/chats must still be treated as if the message.noforwards flag was set (i.e. forwards, downloads, copying, screenshots must be disabled)." | v1, remote | For protected chats: chat structure stays visible; **media is never fetched into the archive** (unavailable placeholder, "restricted by Telegram" state, POL-4); **message text is likewise excluded from rendered exports** — writing it to NDJSON/Markdown files is copying, which the rule disables (see Fact-check note F-2). Thumbnails follow the same restriction. | TASK-260715-23arcu (SEC-032), TASK-260715-3nl3mu (restriction-aware thumbnails), TASK-260715-3prhsi (POL-4/DEC-016, done), renderer fixtures STORY-260715-1oq9jg (PRD-024) |
| TGC-16 | Bot-sent protected messages: per-message `noforwards` — "for these messages, forwards, downloads, copying, screenshots must be disabled." | v1, remote | Per-message capability enforcement (TDLib `can_be_saved` and equivalents), independent of chat-level protection. | TASK-260715-23arcu (SEC-032) |
| TGC-17 | Private-chat protection: `userFull.noforwards_my_enabled` / `noforwards_peer_enabled` mark protected private chats. | v1, remote | Protected private chats receive the same treatment as protected groups/channels (TGC-15). Capability mapping covers chat-level and message-level flags across all peer types. | TASK-260715-23arcu (SEC-032) |
| TGC-18 | "Attempting to forward messages from a protected chat/channel will emit a CHAT_FORWARDS_RESTRICTED RPC error." | v1, remote | Restriction errors are classified as permanent (unsupported/protected class) — never retried as transient; surfaced as the restricted placeholder state. | TASK-260715-3b9w8x (SYNC-044), TASK-260715-22fh09 |

## 4. Takeout — core.telegram.org/api/takeout, method/account.initTakeoutSession

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-19 | "The flags passed to account.initTakeoutSession must enable exactly the data the user chose to export"; "each query must be wrapped using invokeWithTakeout"; "After finishing the export, terminate the session using account.finishTakeoutSession" (success flag semantics). | remote (deferred-optional) | Takeout session flags mirror the user's explicit selection; every query is wrapped; sessions are always terminated on success, abort, and error paths. | TASK-260715-wrgb1j (AC: "session closes on terminal paths") |
| TGC-20 | Error 420 `TAKEOUT_INIT_DELAY_%d`: "for security reasons, you will be able to begin downloading your data in %d seconds. We have notified all your devices about the export request…" | remote (deferred-optional) | The security delay is surfaced to the user, the job resumes automatically after the wait, and the delay is never bypassed or hammered with retries (see Fact-check note F-1). | TASK-260715-wrgb1j (AC: "delay is surfaced"); UX question tracked in docs/OPEN_QUESTIONS.md #8 |
| TGC-21 | TDLib exposes no takeout API; v1 local-first crawls history through normal API methods (architecture decision, `.spec/architecture.md`). | v1 | Local-first backfill uses only normal TDLib methods with flood-wait pacing; takeout belongs exclusively to the optional remote/import tooling. | TASK-260715-mua1ng (scope-explicit), TASK-260715-26dnp6 |

## 5. Rate limits — core.telegram.org/api/errors

| ID | Rule (citation) | Applies to | Control | Owner |
|---|---|---|---|---|
| TGC-22 | 420 errors: "FLOOD_WAIT_X: A wait of X seconds is required"; "FLOOD_PREMIUM_WAIT_X: A wait of X seconds is required … See here » for more info on how to handle this error." | v1, remote | Flood waits honor the server-specified wait exactly: bounded request concurrency, exponential backoff, no tight retry loops (NFR-033); wait state is observable (PRD-004) and pauses the scheduler rather than erroring the drive. Applies to both metadata calls and media downloads (`FLOOD_PREMIUM_WAIT` on downloads). | TASK-260715-22fh09 (SYNC-044), TASK-260715-mua1ng (SEC-031), TASK-260715-3b9w8x, TASK-260715-162fdj; remote: TASK-260715-15ibl5 |

## Gaps — rules without an owning board task

No silent gaps: every rule above either has an owning board element or appears here
with a proposed task/AC change. Proposals require orchestrator action (this research
task does not mutate board decomposition).

- **G-1 (TGC-06, disclosure).** No AC anywhere requires the "uses the Telegram API"
  disclosure in the in-app intro and store/website listing. Proposal: add explicit AC
  items to TASK-260715-13pxnu (onboarding disclosure), TASK-260715-32gjo8 (listing/docs
  wording), TASK-260715-1dk9ik (distribution artifacts); verify at TASK-260715-1nxcst.
- **G-2 (TGC-03b, read-state neutrality).** No AC forbids read-state side effects
  during background sync. Proposal: add AC to TASK-260715-26dnp6 and TASK-260715-10p5zp —
  "history crawl and update processing never emit viewMessages/openChat or any
  read-acknowledgement; verified by the conformance suite (TASK-260715-3e8q4m) against
  the fake source's action log."
- **G-3 (TGC-10, sponsored messages).** No board task owns ToS 3.3 applicability.
  This is a product/ToS-risk decision reserved to the owner (POL-8: "actions with
  Telegram ToS/account-safety risk beyond behaviors already approved" always escalate).
  Proposal: create a decision task under STORY-260715-1rmrtu ("sponsored-messages
  applicability decision") producing a DEC row. Analysis for that decision:
  - GramDrive "allows accessing content from Telegram channels" (channel media as
    files, history as NDJSON/Markdown), so clause 3.3 is arguably triggered.
  - The documented mechanics (call `messages.getSponsoredMessages` "each time the
    user opens a channel", 5-minute cache, view/click reporting) presuppose a
    channel-feed UI. GramDrive v1 has no channel reading UI — the filesystem and a
    settings-style companion app are the only surfaces; there is no defined placement
    for a sponsored message in a Finder folder listing.
  - Options: (a) treat rendered channel exports/companion browsing as "opening a
    channel" and display sponsored content there; (b) document a reasoned
    non-applicability position (no channel feed surface exists) and optionally
    confirm with Telegram (recover@/support channels) before public release;
    (c) exclude channel content from v1 scope (severe product cost).
  - Recommendation: option (b) with explicit owner sign-off recorded as a DEC row,
    and re-evaluation if any channel-feed-like UI is ever added to the companion app.
    Until decided, this item blocks the public-release gate (TASK-260715-1nxcst).
- **G-4 (TGC-11/TGC-13, enforcement & ban-recovery ops).** No task documents the
  breach-notice/cure-window process or the account-ban recovery path. Proposal: add AC
  to TASK-260715-32gjo8 (admin/ops docs) — monitored owner account for the production
  api_id, 10-day cure runbook, recover@telegram.org procedure, reference to R-004.

## Key takeaways

1. **Most Telegram rules already have owners.** Branding (POL-7), protected content
   (POL-4/SEC-032), flood-wait pacing (SEC-031/NFR-033), api_id hygiene (SEC-001/003,
   NFR-053), the AI-training ban (SEC-051), and GPL avoidance (POL-6) map cleanly onto
   existing accepted policies and board tasks — the checklist mostly confirms coverage.
2. **Sponsored messages (ToS 3.3) is the one genuinely open compliance decision** for
   a filesystem-projection product, and it is an owner-level call (POL-8). It should
   be decided before the public-release gate, not at implementation time (G-3).
3. **Protected-content handling extends to exported text**, not just media: the
   primary source disables "copying" for protected chats, so NDJSON/Markdown exports
   must exclude protected-chat message text. POL-4's "text is exported only where
   Telegram permits" resolves to "not from protected chats" (F-2).
4. **Read-state neutrality needs to be explicit.** A background archiver is safe by
   construction only if it provably never emits view/read acknowledgements; that
   guarantee should be a tested AC, not an accident of implementation (G-2).
5. **Takeout obligations apply only to the deferred remote tier** — v1 local-first
   crawls via normal TDLib methods with flood-wait pacing. The 420
   `TAKEOUT_INIT_DELAY_%d` security delay was re-verified against the method page
   and matches the existing research record (F-1).
6. **Unofficial-client accounts are under automatic observation** and bans are
   permanent-by-default with a human recovery path — conservative pacing defaults and
   test-DC accounts during development are compliance measures, not just politeness.

## Fact-check notes

- **F-1.** The takeout security delay was re-verified on the method page
  (core.telegram.org/method/account.initTakeoutSession, checked 2026-07-17): code
  **420** `TAKEOUT_INIT_DELAY_%d`, consistent with the existing record in
  `.research/260715-core-libraries.md`. Older third-party sources sometimes list it
  as a 403; the repo does not.
- **F-2.** Interpretation, flagged for reviewer attention: api/content-protection says
  protected-content "copying … must be disabled". Writing protected-chat text into
  user-readable export files is copying; the official Telegram Desktop export tool
  likewise refuses protected content. This checklist therefore reads POL-4's "text is
  exported only where Telegram permits" as **no text export from protected chats**.
  This does not contradict the accepted POL-4 wording, but implementers of
  TASK-260715-23arcu and the renderers should treat it as normative unless the owner
  decides otherwise.
- **F-3.** ToS 1.5 additionally references the "Telegram Terms of Service for Content
  Licensing and AI Scraping" as a further binding document; it was not separately
  fetched. The SEC-051 control is written to the stricter reading (no AI/ML use at
  all), so the extra document cannot relax obligations, only add detail. Flagged for
  the legal review in SEC-053.
- **F-4.** The sponsored-messages page confirms third-party display obligations and
  mechanics but does not state a channel-size threshold; no rule in this checklist
  depends on one.
- All verbatim quotes were extracted from the live pages on 2026-07-17 via direct
  HTTP fetch (not from summaries or third-party mirrors).

## Source register

| Source | URL | Checked |
|---|---|---|
| Telegram API Terms of Service | https://core.telegram.org/api/terms | 2026-07-17 |
| Creating your Telegram Application | https://core.telegram.org/api/obtaining_api_id | 2026-07-17 |
| Content protection | https://core.telegram.org/api/content-protection | 2026-07-17 |
| Takeout API | https://core.telegram.org/api/takeout | 2026-07-17 |
| account.initTakeoutSession (error table) | https://core.telegram.org/method/account.initTakeoutSession | 2026-07-17 |
| RPC errors (420 FLOOD) | https://core.telegram.org/api/errors | 2026-07-17 |
| Sponsored messages | https://core.telegram.org/api/sponsored-messages | 2026-07-17 |
| MTProto security guidelines (via ToS 1.1) | https://core.telegram.org/mtproto/security_guidelines | referenced |
