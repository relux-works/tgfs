import Foundation
import GramDriveAgentCore
@testable import GramDriveCompanion
import Testing

private func health(
    observedAuthorization: ObservedAuthorizationState
) -> HealthReadout {
    .running(
        previewSnapshot(
            accounts: [
                AccountHealthSummary(
                    accountId: 42,
                    displayName: "Private",
                    authState: "authorized",
                    observedAuthorization: observedAuthorization
                ),
            ]
        )
    )
}

private final class SessionCreationProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var countStorage = 0

    func makeUnavailableSession() -> any AuthorizationSession {
        lock.withLock { countStorage += 1 }
        return UnavailableAuthorizationSession(reason: .notWired)
    }

    var count: Int {
        lock.withLock { countStorage }
    }
}

private actor DelayedHealthProbe {
    nonisolated let requests: AsyncStream<Int>
    private let requestContinuation: AsyncStream<Int>.Continuation
    private let readings: [HealthReadout]
    private var nextIndex = 0
    private var firstWaiter: CheckedContinuation<Void, Never>?

    init(_ readings: [HealthReadout]) {
        self.readings = readings
        (requests, requestContinuation) = AsyncStream.makeStream(of: Int.self)
    }

    func fetch() async -> HealthReadout {
        let index = nextIndex
        nextIndex += 1
        requestContinuation.yield(index)
        if index == 0 {
            await withCheckedContinuation { firstWaiter = $0 }
        }
        return readings[index]
    }

    func releaseFirst() {
        let waiter = firstWaiter
        firstWaiter = nil
        waiter?.resume()
    }
}

@MainActor
private func makeCompanion(backend: InMemoryCompanionBackend) -> CompanionViewModel {
    CompanionViewModel(
        backend: backend,
        diskProbe: FixedDiskSpaceProbe(available: 500_000_000_000),
        accountLabel: "Private",
        domainSetup: FixedFileProviderDomainSetup(
            rootURL: URL(fileURLWithPath: "/tmp/GramDrive")
        ),
        onboardingStore: InMemoryOnboardingCompletionStore(completed: true)
    )
}

@MainActor
struct AuthorizationHealthReconciliationTests {
    @Test func observedAuthorizedSeedsInitialOpenAndRelaunchWithoutStartingAuth() async {
        let sessions = SessionCreationProbe()
        let backend = InMemoryCompanionBackend(
            health: health(observedAuthorization: .authorized),
            session: { sessions.makeUnavailableSession() }
        )

        let initialOpen = makeCompanion(backend: backend)
        await initialOpen.refresh()
        #expect(initialOpen.authorization.state == .ready)
        #expect(initialOpen.authorization.healthState == .authorized)
        #expect(sessions.count == 0)

        let relaunched = makeCompanion(backend: backend)
        await relaunched.refresh()
        #expect(relaunched.authorization.state == .ready)
        #expect(relaunched.authorization.healthState == .authorized)
        #expect(sessions.count == 0)
    }

    @Test func requiredAndUnavailableObservationsRemainDistinctAndActionable() async {
        let backend = InMemoryCompanionBackend(
            health: health(observedAuthorization: .authorizationRequired)
        )
        let model = makeCompanion(backend: backend)

        await model.refresh()
        #expect(model.authorization.state == .idle)
        #expect(model.authorization.healthState == .authorizationRequired)

        backend.setHealth(health(observedAuthorization: .unavailable))
        await model.refresh()
        #expect(model.authorization.state == .idle)
        #expect(model.authorization.healthState == .unavailable)

        backend.setHealth(health(observedAuthorization: .authorized))
        await model.refresh()
        #expect(model.authorization.state == .ready)

        backend.setHealth(.timedOut)
        await model.refresh()
        #expect(model.authorization.state == .ready)
        #expect(model.authorization.healthState == .unavailable)
    }

    @Test func activeSessionRemainsAuthoritativeUntilItsStreamEnds() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)

        await model.begin()
        session.emit(.waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100")))
        for _ in 0 ..< 100 where model.state.kind != "wait-code" {
            await Task.yield()
        }

        model.reconcile(with: health(observedAuthorization: .authorized))
        #expect(model.state.kind == "wait-code")
        #expect(model.healthState == .unknown)

        session.finish()
        await model.waitForCompletion()
        model.reconcile(with: health(observedAuthorization: .authorized))
        #expect(model.state == .ready)
        #expect(model.healthState == .authorized)
    }

    @Test func delayedOlderRefreshCannotRegressNewerAuthorizedResult() async {
        let probe = DelayedHealthProbe([
            .notRunning,
            health(observedAuthorization: .authorized),
        ])
        let backend = InMemoryCompanionBackend(
            healthProvider: { await probe.fetch() }
        )
        let model = makeCompanion(backend: backend)
        var requests = probe.requests.makeAsyncIterator()

        let older = Task { @MainActor in await model.refresh() }
        #expect(await requests.next() == 0)
        let newer = Task { @MainActor in await model.refresh() }
        #expect(await requests.next() == 1)
        await newer.value
        #expect(model.authorization.state == .ready)

        await probe.releaseFirst()
        await older.value
        #expect(model.authorization.state == .ready)
        #expect(model.authorization.healthState == .authorized)
    }

    @Test func delayedAuthorizedHealthCannotOverrideOrDuplicateLiveStart() async {
        let probe = DelayedHealthProbe([health(observedAuthorization: .authorized)])
        let session = ScriptedAuthorizationSession()
        let sessions = SessionCreationProbe()
        let backend = InMemoryCompanionBackend(
            healthProvider: { await probe.fetch() },
            session: {
                _ = sessions.makeUnavailableSession()
                return session
            }
        )
        let model = makeCompanion(backend: backend)
        var requests = probe.requests.makeAsyncIterator()

        let refresh = Task { @MainActor in await model.refresh() }
        #expect(await requests.next() == 0)
        await model.authorization.begin()
        session.emit(.waitPhoneNumber)
        for _ in 0 ..< 100 where model.authorization.state != .waitPhoneNumber {
            await Task.yield()
        }
        await probe.releaseFirst()
        await refresh.value

        #expect(model.authorization.state == .waitPhoneNumber)
        #expect(model.authorization.healthState == .unknown)
        #expect(sessions.count == 1)
        session.finish()
        await model.authorization.waitForCompletion()
    }
}
