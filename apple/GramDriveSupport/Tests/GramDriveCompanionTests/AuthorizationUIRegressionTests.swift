import AppKit
import SwiftUI
import Testing
import Vision

@testable import GramDriveCompanion

@Suite(.serialized)
struct AuthorizationUIRegressionTests {
    @Test func generatedLoginQRCodeIsScannableAndRetainsItsPayload() throws {
        let payload = "tg://login?token=synthetic-non-secret-fixture"
        let image = try #require(TelegramLoginQRCode.image(for: payload))
        #expect(image.width == image.height)

        let request = VNDetectBarcodesRequest()
        try VNImageRequestHandler(cgImage: image).perform([request])
        let qrCode = try #require(request.results?.first { $0.symbology == .qr })
        #expect(qrCode.payloadStringValue == payload)
    }

    /// Hosts the real SwiftUI secure field in an AppKit window, then types and
    /// presses Return through the window's first responder. The probe records
    /// only input lengths, never the password text.
    @MainActor
    @Test func passwordFieldTakesFocusTypingReturnAndRetry() async throws {
        let probe = PasswordSubmissionProbe()
        let session = ScriptedAuthorizationSession { input in
            guard case .submitPassword(let password) = input else { return .accepted }
            let attempt = probe.record(length: password.count)
            return attempt == 1 ? .rejected(.invalidPassword) : .accepted
        }
        let model = AuthorizationViewModel(
            backend: InMemoryCompanionBackend(session: { session }))
        await model.begin()
        session.emit(
            .waitPassword(CompanionPasswordInfo(hint: "", hasRecoveryEmail: false)))
        session.finish()
        await model.waitForCompletion()

        let window = NSWindow(
            contentRect: NSRect(x: 0, y: 0, width: 560, height: 560),
            styleMask: [.titled],
            backing: .buffered,
            defer: false)
        window.isReleasedWhenClosed = false
        window.contentView = NSHostingView(rootView: AuthorizationView(model: model))
        NSApplication.shared.activate()
        window.makeKeyAndOrderFront(nil)
        defer { window.close() }
        await drainMainRunLoop()

        try type("fixturex", in: window)
        try pressDelete(in: window)
        try pressReturn(in: window)
        await waitForSubmissionCount(1, probe: probe)
        #expect(model.lastRejection == .invalidPassword)

        // The field stays editable and focused after a validation error, so a
        // fresh retry requires no extra click.
        try type("retry", in: window)
        try pressReturn(in: window)
        await waitForSubmissionCount(2, probe: probe)
        #expect(probe.lengths == [7, 5])
        #expect(model.lastRejection == nil)
    }

    @MainActor
    private func type(_ text: String, in window: NSWindow) throws {
        let editor = try #require(window.firstResponder as? NSTextView)
        editor.insertText(text, replacementRange: editor.selectedRange())
        RunLoop.main.run(until: Date().addingTimeInterval(0.01))
    }

    @MainActor
    private func pressReturn(in window: NSWindow) throws {
        let editor = try #require(window.firstResponder as? NSTextView)
        #expect(editor.tryToPerform(#selector(NSResponder.insertNewline(_:)), with: nil))
        RunLoop.main.run(until: Date().addingTimeInterval(0.01))
    }

    @MainActor
    private func pressDelete(in window: NSWindow) throws {
        let editor = try #require(window.firstResponder as? NSTextView)
        #expect(editor.tryToPerform(#selector(NSResponder.deleteBackward(_:)), with: nil))
        RunLoop.main.run(until: Date().addingTimeInterval(0.01))
    }

    @MainActor
    private func drainMainRunLoop() async {
        for _ in 0..<10 {
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(10))
        }
    }

    @MainActor
    private func waitForSubmissionCount(_ count: Int, probe: PasswordSubmissionProbe) async {
        for _ in 0..<50 where probe.lengths.count < count {
            await Task.yield()
            try? await Task.sleep(for: .milliseconds(10))
        }
    }
}

private final class PasswordSubmissionProbe: @unchecked Sendable {
    private let lock = NSLock()
    private var recordedLengths: [Int] = []

    var lengths: [Int] {
        lock.lock()
        defer { lock.unlock() }
        return recordedLengths
    }

    @discardableResult
    func record(length: Int) -> Int {
        lock.lock()
        defer { lock.unlock() }
        recordedLengths.append(length)
        return recordedLengths.count
    }
}
