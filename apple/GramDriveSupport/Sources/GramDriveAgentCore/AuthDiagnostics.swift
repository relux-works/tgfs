import Foundation
import OSLog

/// The complete vocabulary permitted in installed-build auth diagnostics.
///
/// These values deliberately describe only control-flow outcomes. In
/// particular, neither an auth input nor an auth-state payload can become a
/// diagnostic value: those payloads can contain phone numbers, one-time
/// codes, passwords, QR links, account identifiers, and display names.
public enum AuthDiagnosticCode: String, CaseIterable, Codable, Equatable, Sendable {
    case sessionStarted = "auth-session-started"
    case refusedInvalidPhoneNumber = "auth-refused-invalid-phone-number"
    case refusedPhoneNumberBanned = "auth-refused-phone-number-banned"
    case refusedInvalidCode = "auth-refused-invalid-code"
    case refusedExpiredCode = "auth-refused-expired-code"
    case refusedInvalidPassword = "auth-refused-invalid-password"
    case refusedRateLimited = "auth-refused-rate-limited"
    case refusedNetwork = "auth-refused-network"
    case refusedSessionEnded = "auth-refused-session-ended"
    case refusedOther = "auth-refused-other"
    case refusedUnknown = "auth-refused-unknown"
    case finalizeSucceeded = "auth-finalize-succeeded"
    case finalizeFailed = "auth-finalize-failed"
    case probeSignedOut = "auth-probe-signed-out"

    static func refusal(for rejection: ControlAuthRejection) -> AuthDiagnosticCode {
        switch rejection.kind {
        case "invalid-phone-number": .refusedInvalidPhoneNumber
        case "phone-number-banned": .refusedPhoneNumberBanned
        case "invalid-code": .refusedInvalidCode
        case "expired-code": .refusedExpiredCode
        case "invalid-password": .refusedInvalidPassword
        case "rate-limited": .refusedRateLimited
        case "network": .refusedNetwork
        case "session-ended": .refusedSessionEnded
        case "other": .refusedOther
        default: .refusedUnknown
        }
    }

    static func finalization(for state: ControlAuthState) -> AuthDiagnosticCode? {
        switch state.kind {
        case "ready": .finalizeSucceeded
        case "failed": .finalizeFailed
        default: nil
        }
    }
}

/// A bounded, durable append-only view of fixed auth diagnostic codes.
///
/// Persistence is intentionally separate from the core-owned state database:
/// it remains available while authorization itself is repairing or refusing,
/// and its JSON shape cannot acquire account or auth payload fields by
/// accident. Failure to write diagnostics never changes an auth outcome.
final class AuthDiagnosticTrail: @unchecked Sendable {
    private static let retainedEventLimit = 64

    private let fileURL: URL
    private let lock = NSLock()
    private let logger = Logger(
        subsystem: "com.reluxworks.gramdrive", category: "agent.auth"
    )

    init(fileURL: URL) {
        self.fileURL = fileURL
    }

    func restore() -> [String] {
        lock.lock()
        defer { lock.unlock() }
        guard
            let data = try? Data(contentsOf: fileURL),
            let rawCodes = try? JSONDecoder().decode([String].self, from: data)
        else {
            return []
        }
        // Reject any future/foreign string rather than surfacing persisted
        // material whose privacy invariant this process cannot establish.
        return rawCodes.compactMap(AuthDiagnosticCode.init(rawValue:)).map(\.rawValue)
    }

    func record(_ code: AuthDiagnosticCode) {
        logger.notice("\(Self.logMessage(for: code), privacy: .public)")
        lock.lock()
        defer { lock.unlock() }

        let existing = restoreLocked()
        let retained = Array((existing + [code.rawValue]).suffix(Self.retainedEventLimit))
        guard let data = try? JSONEncoder().encode(retained) else { return }
        try? data.write(to: fileURL, options: .atomic)
    }

    /// Kept pure so privacy tests exercise the exact unified-log rendering.
    static func logMessage(for code: AuthDiagnosticCode) -> String {
        "event=\(code.rawValue)"
    }

    private func restoreLocked() -> [String] {
        guard
            let data = try? Data(contentsOf: fileURL),
            let rawCodes = try? JSONDecoder().decode([String].self, from: data)
        else {
            return []
        }
        return rawCodes.compactMap(AuthDiagnosticCode.init(rawValue:)).map(\.rawValue)
    }
}
