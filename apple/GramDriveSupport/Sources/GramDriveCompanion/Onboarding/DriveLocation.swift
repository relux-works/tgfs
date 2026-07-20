import Foundation

#if canImport(AppKit)
import AppKit
#endif

/// Where the GramDrive drive is visible in Finder, and the action to reveal
/// it — the onboarding success step's "Open in Finder" affordance.
///
/// A seam so the reveal is testable without a real File Provider domain or a
/// live Finder: the resolution rule is a pure function of a directory listing,
/// and the reveal itself is an injectable side effect.
public protocol DriveLocationProviding: Sendable {
    /// The user-visible Finder URL of the drive, when it can be resolved yet.
    /// `nil` before any provider folder exists (e.g. sign-in not finished).
    func resolveDriveURL() -> URL?
    /// Reveals the drive in Finder. Returns whether a location could be shown.
    @discardableResult
    func reveal() -> Bool
}

/// The product drive-location provider.
///
/// macOS surfaces a File Provider replicated domain under
/// `~/Library/CloudStorage`; GramDrive's domain presents as `GramDrive`
/// (POL-7 / ``DomainIdentity``). This resolves the provider folder by that
/// name prefix and, until it exists, falls back to the CloudStorage container
/// itself so "Open in Finder" always lands somewhere sensible.
public struct CloudStorageDriveLocation: DriveLocationProviding {
    private let baseDirectory: URL
    private let displayNamePrefix: String
    // `FileManager` is not `Sendable`; the provider only ever calls its
    // thread-safe read-only directory APIs.
    private nonisolated(unsafe) let fileManager: FileManager
    private let revealer: @Sendable (URL) -> Bool

    public init(
        baseDirectory: URL? = nil,
        displayNamePrefix: String = "GramDrive",
        fileManager: FileManager = .default,
        revealer: @escaping @Sendable (URL) -> Bool = CloudStorageDriveLocation.defaultRevealer
    ) {
        self.baseDirectory = baseDirectory ?? CloudStorageDriveLocation.defaultBaseDirectory
        self.displayNamePrefix = displayNamePrefix
        self.fileManager = fileManager
        self.revealer = revealer
    }

    /// `~/Library/CloudStorage`.
    public static var defaultBaseDirectory: URL {
        FileManager.default.homeDirectoryForCurrentUser
            .appendingPathComponent("Library/CloudStorage", isDirectory: true)
    }

    public func resolveDriveURL() -> URL? {
        // Prefer a GramDrive-prefixed provider folder if one exists…
        if let match = driveChild() {
            return match
        }
        // …otherwise the CloudStorage container itself, when it is present.
        var isDirectory: ObjCBool = false
        if fileManager.fileExists(atPath: baseDirectory.path, isDirectory: &isDirectory),
            isDirectory.boolValue
        {
            return baseDirectory
        }
        return nil
    }

    /// The first GramDrive-prefixed child of the CloudStorage directory, by
    /// stable name order (deterministic when several accounts each register a
    /// domain).
    private func driveChild() -> URL? {
        guard
            let entries = try? fileManager.contentsOfDirectory(
                at: baseDirectory,
                includingPropertiesForKeys: [.isDirectoryKey],
                options: [.skipsHiddenFiles])
        else {
            return nil
        }
        return
            entries
            .filter { $0.lastPathComponent.hasPrefix(displayNamePrefix) }
            .sorted { $0.lastPathComponent < $1.lastPathComponent }
            .first
    }

    @discardableResult
    public func reveal() -> Bool {
        guard let url = resolveDriveURL() else { return false }
        return revealer(url)
    }

    /// Opens the resolved folder in Finder.
    public static let defaultRevealer: @Sendable (URL) -> Bool = { url in
        #if canImport(AppKit)
        return NSWorkspace.shared.open(url)
        #else
        return false
        #endif
    }
}

/// A fixed drive location for previews and tests: it resolves to a supplied
/// URL (or none) and records reveal calls instead of touching Finder.
public final class FixedDriveLocation: DriveLocationProviding, @unchecked Sendable {
    private let url: URL?
    private let lock = NSLock()
    private var _revealCount = 0

    public init(url: URL?) {
        self.url = url
    }

    /// How many times ``reveal()`` was invoked — for assertions.
    public var revealCount: Int {
        lock.lock()
        defer { lock.unlock() }
        return _revealCount
    }

    public func resolveDriveURL() -> URL? { url }

    @discardableResult
    public func reveal() -> Bool {
        lock.lock()
        _revealCount += 1
        lock.unlock()
        return url != nil
    }
}
