import Foundation
import GramDriveCore
import Security

/// The engine-backed implementations of the control channel's command
/// seams (BUG-260720-3i74u1): sign-in sessions, account removal, and the
/// repair pass, all over the FFI's authorization surface — plus the
/// Security.framework `SecretVault` those operations read their secrets
/// through (SEC-003: credentials at runtime from the OS keychain, never
/// from configuration; DEC-002: keychain code stays native).

/// How the engine host builds the FFI's `AuthSessionConfig`: one shared
/// rule so sessions, probes, and removals agree on storage and disclosure
/// metadata (SEC-030: the device fields must be truthful).
public struct CoreAuthConfiguration: Sendable {
    /// The shared data root (the same value the lifecycle coordinates).
    public var dataRoot: URL
    /// Whether sign-in targets Telegram's test data centers — never set in
    /// a user-facing build; the acceptance smoke drives it explicitly.
    public var useTestDc: Bool

    public init(dataRoot: URL, useTestDc: Bool = false) {
        self.dataRoot = dataRoot
        self.useTestDc = useTestDc
    }

    /// The FFI config: storage under the data root, truthful device
    /// disclosure (hardware model, OS version, agent version, locale).
    public func sessionConfig() -> AuthSessionConfig {
        AuthSessionConfig(
            dataDir: dataRoot.path,
            useTestDc: useTestDc,
            deviceModel: Self.hardwareModel(),
            systemVersion: ProcessInfo.processInfo.operatingSystemVersionString,
            applicationVersion: AgentVersion.current,
            systemLanguageCode: Locale.preferredLanguages.first ?? "en")
    }

    private static func hardwareModel() -> String {
        var size = 0
        guard sysctlbyname("hw.model", nil, &size, nil, 0) == 0, size > 0 else {
            return "Mac"
        }
        var buffer = [UInt8](repeating: 0, count: size)
        guard sysctlbyname("hw.model", &buffer, &size, nil, 0) == 0 else {
            return "Mac"
        }
        return String(decoding: buffer.prefix(while: { $0 != 0 }), as: UTF8.self)
    }
}

// MARK: - The keychain vault

/// The Security.framework secret vault: product api credentials from the
/// `gramdrive-telegram` generic-password service (the repo's provisioning
/// convention — TASK-260716-1iypv4), per-account database keys under the
/// product's own service. Key creation draws from `SecRandomCopyBytes`
/// (SEC-002); nothing here logs or returns secret material beyond the FFI
/// call that needs it.
public final class KeychainSecretVault: SecretVault {
    /// The service holding `api_id`/`api_hash` (accounts of that name).
    public static let credentialsService = "gramdrive-telegram"
    /// The service holding per-account database keys.
    public static let databaseKeyService = "com.reluxworks.gramdrive.database-key"

    public init() {}

    public func apiCredentials() throws -> VaultApiCredentials {
        guard
            let idData = try copyItem(service: Self.credentialsService, account: "api_id"),
            let hashData = try copyItem(service: Self.credentialsService, account: "api_hash"),
            let idText = String(data: idData, encoding: .utf8),
            let hash = String(data: hashData, encoding: .utf8),
            let apiId = Int32(idText.trimmingCharacters(in: .whitespacesAndNewlines))
        else {
            throw DriveError.AuthRequired(
                detail: "Telegram api credentials are not provisioned in the keychain")
        }
        return VaultApiCredentials(
            apiId: apiId, apiHash: hash.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    public func databaseKey(accountId: Int64) throws -> Data? {
        try copyItem(service: Self.databaseKeyService, account: keyAccount(accountId))
    }

    public func ensureDatabaseKey(accountId: Int64) throws -> Data {
        if let existing = try databaseKey(accountId: accountId) {
            return existing
        }
        var bytes = Data(count: 32)
        let status = bytes.withUnsafeMutableBytes { buffer in
            SecRandomCopyBytes(kSecRandomDefault, 32, buffer.baseAddress!)
        }
        guard status == errSecSuccess else {
            throw DriveError.Storage(detail: "entropy unavailable: \(status)")
        }
        try storeDatabaseKey(accountId: accountId, key: bytes)
        return bytes
    }

    public func storeDatabaseKey(accountId: Int64, key: Data) throws {
        let account = keyAccount(accountId)
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.databaseKeyService,
            kSecAttrAccount as String: account,
        ]
        let update: [String: Any] = [kSecValueData as String: key]
        let status = SecItemUpdate(query as CFDictionary, update as CFDictionary)
        if status == errSecItemNotFound {
            var add = query
            add[kSecValueData as String] = key
            add[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
            let added = SecItemAdd(add as CFDictionary, nil)
            guard added == errSecSuccess else {
                throw DriveError.Storage(detail: "keychain add failed: \(added)")
            }
            return
        }
        guard status == errSecSuccess else {
            throw DriveError.Storage(detail: "keychain update failed: \(status)")
        }
    }

    public func deleteDatabaseKey(accountId: Int64) throws {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: Self.databaseKeyService,
            kSecAttrAccount as String: keyAccount(accountId),
        ]
        let status = SecItemDelete(query as CFDictionary)
        guard status == errSecSuccess || status == errSecItemNotFound else {
            throw DriveError.Storage(detail: "keychain delete failed: \(status)")
        }
    }

    private func keyAccount(_ accountId: Int64) -> String {
        "account-\(accountId)"
    }

    private func copyItem(service: String, account: String) throws -> Data? {
        let query: [String: Any] = [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: account,
            kSecReturnData as String: true,
            kSecMatchLimit as String: kSecMatchLimitOne,
        ]
        var result: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &result)
        switch status {
        case errSecSuccess:
            return result as? Data
        case errSecItemNotFound:
            return nil
        default:
            throw DriveError.Storage(detail: "keychain read failed: \(status)")
        }
    }
}

// MARK: - Sign-in sessions

/// The FFI-backed authorizer: each control-channel sign-in is one
/// `AuthSession` over the process's Telegram runtime.
public struct CoreAuthorizer: AgentAuthorizing {
    private let configuration: CoreAuthConfiguration
    private let vault: any SecretVault

    public init(configuration: CoreAuthConfiguration, vault: any SecretVault) {
        self.configuration = configuration
        self.vault = vault
    }

    public func makeSession() throws -> any AgentAuthSessionHosting {
        try CoreAuthSession(configuration: configuration, vault: vault)
    }
}

/// One live sign-in: the FFI session's phases become wire states, wire
/// inputs become FFI commands.
final class CoreAuthSession: AgentAuthSessionHosting, @unchecked Sendable {
    private let session: AuthSession
    private let relay: PhaseRelay

    init(configuration: CoreAuthConfiguration, vault: any SecretVault) throws {
        let relay = PhaseRelay()
        self.relay = relay
        self.session = try AuthSession.start(
            config: configuration.sessionConfig(), vault: vault, listener: relay)
    }

    var states: AsyncStream<ControlAuthState> {
        relay.stream
    }

    func submit(_ input: ControlAuthInput) async -> AgentAuthSubmitAnswer {
        let command: AuthCommand
        switch input {
        case .submitPhoneNumber(let phoneNumber):
            command = .submitPhoneNumber(phoneNumber: phoneNumber)
        case .requestQrCode:
            command = .requestQrCode
        case .submitCode(let code):
            command = .submitCode(code: code)
        case .resendCode:
            command = .resendCode
        case .submitPassword(let password):
            command = .submitPassword(password: password)
        case .cancel:
            command = .cancel
        }
        do {
            switch try await session.submit(command: command) {
            case .accepted:
                return .accepted
            case .invalidForState:
                return .invalidForState
            case .rejected(let rejection):
                return .rejected(Self.wireRejection(rejection))
            }
        } catch let error as DriveError {
            // Channel-level failures become typed rejections the flow can
            // render: a closed session ended, everything else reads as a
            // transient network condition (retry the same input).
            if case .Cancelled = error {
                return .rejected(ControlAuthRejection(kind: "session-ended"))
            }
            return .rejected(ControlAuthRejection(kind: "network"))
        } catch {
            return .rejected(ControlAuthRejection(kind: "network"))
        }
    }

    func close() {
        session.close()
    }

    static func wireRejection(_ rejection: AuthRejectionInfo) -> ControlAuthRejection {
        switch rejection {
        case .invalidPhoneNumber:
            return ControlAuthRejection(kind: "invalid-phone-number")
        case .phoneNumberBanned:
            return ControlAuthRejection(kind: "phone-number-banned")
        case .invalidCode:
            return ControlAuthRejection(kind: "invalid-code")
        case .expiredCode:
            return ControlAuthRejection(kind: "expired-code")
        case .invalidPassword:
            return ControlAuthRejection(kind: "invalid-password")
        case .rateLimited(let retryAfterSecs):
            return ControlAuthRejection(kind: "rate-limited", retryAfterSeconds: retryAfterSecs)
        case .network:
            return ControlAuthRejection(kind: "network")
        case .sessionEnded:
            return ControlAuthRejection(kind: "session-ended")
        case .other(let code, let detail):
            return ControlAuthRejection(kind: "other", code: code, detail: detail)
        }
    }

    static func wireState(_ phase: AuthPhase) -> ControlAuthState {
        switch phase {
        case .starting:
            return ControlAuthState(kind: "starting")
        case .configuring:
            return ControlAuthState(kind: "configuring")
        case .waitPhoneNumber:
            return ControlAuthState(kind: "wait-phone-number")
        case .waitCode(let info):
            return ControlAuthState(
                kind: "wait-code",
                codeInfo: ControlAuthCodeInfo(
                    phoneNumber: info.phoneNumber,
                    codeLength: info.codeLength.map(Int.init),
                    resendTimeoutSeconds: info.resendTimeoutSecs.map(Int.init)))
        case .waitQrConfirmation(let link):
            return ControlAuthState(kind: "wait-qr-confirmation", qrLink: link)
        case .waitPassword(let info):
            return ControlAuthState(
                kind: "wait-password",
                passwordInfo: ControlAuthPasswordInfo(
                    hint: info.hint, hasRecoveryEmail: info.hasRecoveryEmail))
        case .finalizing:
            return ControlAuthState(kind: "finalizing")
        case .complete(let accountId, let displayName):
            return ControlAuthState(
                kind: "ready",
                account: ControlAccountIdentity(
                    accountId: accountId, displayName: displayName))
        case .loggingOut:
            return ControlAuthState(kind: "logging-out")
        case .closing:
            return ControlAuthState(kind: "closing")
        case .closed:
            return ControlAuthState(kind: "closed")
        case .unsupported(let kind):
            return ControlAuthState(kind: "unsupported", unsupportedKind: kind)
        case .failed(let detail):
            return ControlAuthState(kind: "failed", failureDetail: detail)
        }
    }
}

/// Bridges the FFI's listener callbacks (session background thread) into
/// an `AsyncStream` of wire states, finishing on any terminal phase.
private final class PhaseRelay: AuthStateListener, @unchecked Sendable {
    let stream: AsyncStream<ControlAuthState>
    private let continuation: AsyncStream<ControlAuthState>.Continuation

    init() {
        (stream, continuation) = AsyncStream.makeStream(of: ControlAuthState.self)
    }

    func onPhase(phase: AuthPhase) {
        continuation.yield(CoreAuthSession.wireState(phase))
        switch phase {
        case .complete, .closed, .failed:
            continuation.finish()
        default:
            break
        }
    }
}

// MARK: - Removal and repair

/// The FFI-backed removal seam: the SEC-004 engine half, end to end.
public struct CoreAccountRemover: AgentAccountRemoving {
    private let configuration: CoreAuthConfiguration
    private let vault: any SecretVault

    public init(configuration: CoreAuthConfiguration, vault: any SecretVault) {
        self.configuration = configuration
        self.vault = vault
    }

    public func remove(_ request: ControlRemovalRequest) async -> ControlCommandOutcome {
        do {
            try await removeAccount(
                config: configuration.sessionConfig(),
                accountId: request.accountId,
                revokeSession: request.revokeSession,
                vault: vault)
            return .completed
        } catch {
            return .failed(ControlServer.failure(from: error))
        }
    }
}

/// The FFI-backed repair seam: prove durable state is readable, then prove
/// each configured account's stored Telegram session still authorizes. A
/// signed-out account reports `authRequired` — the actionable answer.
public struct CoreRepairRunner: AgentRepairing {
    private let configuration: CoreAuthConfiguration
    private let vault: any SecretVault
    private let accounts: @Sendable () throws -> [AccountHealthSummary]

    public init(
        configuration: CoreAuthConfiguration,
        vault: any SecretVault,
        accounts: @escaping @Sendable () throws -> [AccountHealthSummary]
    ) {
        self.configuration = configuration
        self.vault = vault
        self.accounts = accounts
    }

    public func repair() async -> ControlCommandOutcome {
        let configured: [AccountHealthSummary]
        do {
            configured = try accounts()
        } catch {
            return .failed(
                ControlCommandFailure(
                    category: .storage, detail: "durable state is not readable"))
        }
        for account in configured {
            do {
                let outcome = try await probeAuthorization(
                    config: configuration.sessionConfig(),
                    accountId: account.accountId,
                    vault: vault)
                if case .signedOut = outcome {
                    return .failed(
                        ControlCommandFailure(
                            category: .authRequired,
                            detail: "account \(account.accountId) needs a fresh sign-in"))
                }
            } catch {
                return .failed(ControlServer.failure(from: error))
            }
        }
        return .completed
    }
}
