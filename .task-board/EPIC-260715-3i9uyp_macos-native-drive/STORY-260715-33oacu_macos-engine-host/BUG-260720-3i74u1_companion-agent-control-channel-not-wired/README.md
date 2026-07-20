# BUG-260720-3i74u1: companion-agent-control-channel-not-wired

## Description
Acceptance testing of the released v0.1.0 build: Sign In (and Repair/Removal/status) in the companion shows This action needs the agent control channel, which is not available in this build yet - LiveCompanionBackend returns ControlChannelUnavailable.notWired. The companion UI was built over an abstract backend, the hydration channel (extension<->agent unix socket) is live, but the companion<->agent CONTROL channel was never wired end-to-end. Fix: implement the live control channel over the existing agent IPC surface (same unix-socket family as hydration, App Group socket path): auth flow commands/events (phone/code/2FA/QR states from the authorization state machine), account/provider status, cache settings, repair and removal commands. Companion must ensure the agent is running (launch the bundled gramdrive-agent via SMAppService or direct spawn on first use, honoring the login-item preference) before opening the channel, with a clear starting agent state instead of the notWired error. Read-only v1 semantics unchanged (DEC-007); no Telegram operations from filesystem callbacks.

## Scope
(define bug scope / affected area)

## Acceptance Criteria
From the released .app bundle: Start Sign In drives the real TDLib auth flow with the agent live control channel; the agent proves every hop up to Telegram code acceptance against test infrastructure and the notWired error is unreachable in the shipped bundle; the agent auto-starts honoring the login-item preference; swift test and make check green and packaging assembles+signs. FINAL live-Telegram acceptance (real code accepted, session persists) is verified by the OWNER signing in on the released v0.1.1 build — Telegram retired shared-test-number auto-code (tdlib/td#3361), so this last hop is a human step by decision 2026-07-20, not an agent gap.
