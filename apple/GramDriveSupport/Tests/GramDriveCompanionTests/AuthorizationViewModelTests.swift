import Foundation
import Testing

@testable import GramDriveCompanion

/// Awaits the view model's flow to quiescence — the scripted session finishes
/// its stream, so this returns deterministically once every emitted state has
/// been applied.
@MainActor
private func settle(_ model: AuthorizationViewModel) async {
    await model.waitForCompletion()
}

@MainActor
@Suite struct AuthorizationScreenStateTests {
    // Every screen state renders from exactly one reported state — the "unit
    // test for each screen state" requirement, driven through the real
    // consume path rather than a private setter.
    @Test func eachReportedStateBecomesTheRenderedState() async {
        let cases: [CompanionAuthState] = [
            .starting,
            .configuring,
            .waitPhoneNumber,
            .waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100", codeLength: 5)),
            .waitQrConfirmation(link: "tg://login?token=abc"),
            .waitPassword(CompanionPasswordInfo(hint: "birthday", hasRecoveryEmail: true)),
            .ready,
            .loggingOut,
            .closing,
            .closed,
            .unsupported(kind: "authorizationStateWaitRegistration"),
        ]
        for expected in cases {
            let session = ScriptedAuthorizationSession()
            let backend = InMemoryCompanionBackend(session: { session })
            let model = AuthorizationViewModel(backend: backend)
            await model.begin()
            session.emit(expected)
            session.finish()
            await settle(model)
            #expect(model.state == expected)
        }
    }

    @Test func fullPhoneCodePasswordFlowProgresses() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.configuring)
        session.emit(.waitPhoneNumber)
        session.emit(.waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100")))
        session.emit(.waitPassword(CompanionPasswordInfo(hint: "", hasRecoveryEmail: false)))
        session.emit(.ready)
        session.finish()
        await settle(model)
        #expect(model.isAuthorized)
        #expect(
            model.stateHistory.map(\.kind) == [
                "configuring", "wait-phone-number", "wait-code", "wait-password", "ready",
            ])
    }

    @Test func qrPathProgressesToReady() async {
        let session = ScriptedAuthorizationSession()
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.waitPhoneNumber)
        session.emit(.waitQrConfirmation(link: "tg://login?token=a"))
        session.emit(.waitQrConfirmation(link: "tg://login?token=b"))  // refreshed link
        session.emit(.ready)
        session.finish()
        await settle(model)
        #expect(model.isAuthorized)
    }
}

@MainActor
@Suite struct AuthorizationInputTests {
    @Test func unavailableChannelIsSurfacedNotFailed() async {
        let backend = InMemoryCompanionBackend(
            session: { UnavailableAuthorizationSession(reason: .notWired) })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        #expect(model.unavailable == .notWired)
        #expect(model.state == .idle)
    }

    @Test func aRejectionIsClassifiedWithAdvice() async {
        let session = ScriptedAuthorizationSession(onSubmit: { input in
            if case .submitCode = input { return .rejected(.expiredCode) }
            return .accepted
        })
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.waitCode(CompanionCodeInfo(phoneNumber: "+1 555 0100")))
        session.finish()
        await model.waitForCompletion()  // waitCode is applied before we submit
        await model.submit(.submitCode("00000"))
        #expect(model.lastRejection == .expiredCode)
        #expect(model.advice == .requestNewCode)
    }

    @Test func aStructurallyInvalidInputIsRefusedLocally() async {
        let session = ScriptedAuthorizationSession(onSubmit: { _ in
            Issue.record("submit must not reach the session for an invalid input")
            return .accepted
        })
        let backend = InMemoryCompanionBackend(session: { session })
        let model = AuthorizationViewModel(backend: backend)
        await model.begin()
        session.emit(.waitPhoneNumber)
        session.finish()
        await model.waitForCompletion()
        // A code is not valid while waiting for a phone number.
        await model.submit(.submitCode("123"))
        #expect(model.lastInvalidInput == .submitCode("123"))
        #expect(model.state == .waitPhoneNumber)
    }

    @Test func cancelIsValidEverywhereButClosed() {
        #expect(CompanionAuthInput.cancel.isValid(in: .waitPassword(
            CompanionPasswordInfo(hint: "", hasRecoveryEmail: false))))
        #expect(CompanionAuthInput.cancel.isValid(in: .unsupported(kind: "x")))
        #expect(!CompanionAuthInput.cancel.isValid(in: .closed))
    }

    @Test func adviceMappingMatchesTheCoreVocabulary() {
        #expect(CompanionAuthRejection.network.advice == .retrySameInput)
        #expect(CompanionAuthRejection.invalidPassword.advice == .reviseInput)
        #expect(CompanionAuthRejection.expiredCode.advice == .requestNewCode)
        #expect(
            CompanionAuthRejection.rateLimited(retryAfterSeconds: 30).advice
                == .waitThenRetry(afterSeconds: 30))
        #expect(CompanionAuthRejection.phoneNumberBanned.advice == .abort)
        #expect(CompanionAuthRejection.sessionEnded.advice == .abort)
    }
}
