import Foundation

/// Persists the one bit onboarding owns: whether the first-launch guided flow
/// has been dismissed by the user, so it is *shown once* and afterward only
/// re-runs on demand (Help ▸ Setup Guide).
///
/// A seam, not a bare `UserDefaults` read, for the same reason every other
/// host surface is one: the flow's presentation logic is a pure function of
/// this flag, so it can be driven to either branch deterministically in a
/// test without touching the real defaults domain.
public protocol OnboardingCompletionStore: Sendable {
    /// Whether the user has finished (or explicitly skipped) onboarding.
    func hasCompletedOnboarding() -> Bool
    /// Records the completion state.
    func setCompletedOnboarding(_ completed: Bool)
}

/// The product store: a single boolean in `UserDefaults`. App-local UI state,
/// not agent settings — the agent never reads it, so it lives here rather than
/// in the shared settings document.
public struct UserDefaultsOnboardingCompletionStore: OnboardingCompletionStore {
    /// The defaults key, in the product namespace (POL-7 / DEC-019).
    public static let defaultKey = "com.reluxworks.gramdrive.onboarding.completed"

    // `UserDefaults` is thread-safe but not `Sendable`; the store carries it
    // across isolation domains only to call its thread-safe accessors.
    private nonisolated(unsafe) let defaults: UserDefaults
    private let key: String

    public init(
        defaults: UserDefaults = .standard,
        key: String = UserDefaultsOnboardingCompletionStore.defaultKey
    ) {
        self.defaults = defaults
        self.key = key
    }

    public func hasCompletedOnboarding() -> Bool {
        defaults.bool(forKey: key)
    }

    public func setCompletedOnboarding(_ completed: Bool) {
        defaults.set(completed, forKey: key)
    }
}

/// An in-memory store for previews and tests — no defaults domain touched.
public final class InMemoryOnboardingCompletionStore: OnboardingCompletionStore, @unchecked Sendable {
    private let lock = NSLock()
    private var completed: Bool

    public init(completed: Bool = false) {
        self.completed = completed
    }

    public func hasCompletedOnboarding() -> Bool {
        lock.lock()
        defer { lock.unlock() }
        return completed
    }

    public func setCompletedOnboarding(_ completed: Bool) {
        lock.lock()
        defer { lock.unlock() }
        self.completed = completed
    }
}
