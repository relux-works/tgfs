import Foundation

/// The usable result of reconciling the app's File Provider domains.
///
/// Registration is not considered successful until the system can resolve a
/// user-visible root URL. That keeps onboarding from declaring success while
/// "Open in Finder" can only fall back to the CloudStorage container.
public struct FileProviderDomainSetupResult: Equatable, Sendable {
    public let rootURL: URL

    public init(rootURL: URL) {
        self.rootURL = rootURL
    }
}

/// The companion-side seam for File Provider domain reconciliation.
///
/// The live implementation is supplied by the containing app because domain
/// management must run from the bundle that embeds the File Provider
/// extension. Tests and previews use deterministic substitutes.
public protocol FileProviderDomainSettingUp: Sendable {
    func reconcile() async throws -> FileProviderDomainSetupResult
}

/// Coalesces overlapping launch and onboarding reconciliation attempts while
/// still allowing a later retry or relaunch to perform a fresh idempotent pass.
public actor CoalescingFileProviderDomainSetup: FileProviderDomainSettingUp {
    public typealias Operation =
        @Sendable () async throws -> FileProviderDomainSetupResult

    private let operation: Operation
    private var inFlight: Task<FileProviderDomainSetupResult, any Error>?

    public init(operation: @escaping Operation) {
        self.operation = operation
    }

    public func reconcile() async throws -> FileProviderDomainSetupResult {
        if let inFlight {
            return try await inFlight.value
        }

        let task = Task { try await operation() }
        inFlight = task
        defer { inFlight = nil }
        return try await task.value
    }
}

/// A fixed setup result for previews and focused view-model tests.
public struct FixedFileProviderDomainSetup: FileProviderDomainSettingUp {
    private let result: FileProviderDomainSetupResult

    public init(rootURL: URL) {
        self.result = FileProviderDomainSetupResult(rootURL: rootURL)
    }

    public func reconcile() async throws -> FileProviderDomainSetupResult {
        result
    }
}
