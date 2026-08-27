import FileProvider
import Foundation
import GramDriveCore

public struct InstalledPlaceholderResolution: Sendable {
  public let userVisibleURL: URL
  public let roundTripIdentifier: String

  public init(userVisibleURL: URL, roundTripIdentifier: String) {
    self.userVisibleURL = userVisibleURL
    self.roundTripIdentifier = roundTripIdentifier
  }
}

/// A headless installed-acceptance seam that resolves one exact provider item.
///
/// The containing companion app owns the embedded File Provider extension, so
/// it is the process in which `NSFileProviderManager(for:)` can validly resolve
/// a registered GramDrive domain. The opaque identifier arrives on stdin and
/// never appears in arguments or output. Resolving the URL requests no content;
/// the acceptance harness independently requires the resulting placeholder to
/// exist, remain dataless, and match the same stable item identity.
public enum InstalledPlaceholderResolutionCommand {
  public static let flag = "--acceptance-resolve-placeholder"

  public static func isRequested(arguments: [String]) -> Bool {
    arguments.dropFirst().first == flag
  }

  public static func run(
    arguments: [String],
    readIdentifier: () -> String?,
    resolve: (String) async throws -> InstalledPlaceholderResolution,
    emit: (String) -> Void
  ) async -> Int32 {
    guard arguments.count == 2, arguments[1] == flag,
      let identifier = readIdentifier(), isCanonicalItemIdentifier(text: identifier)
    else {
      return 2
    }
    do {
      let resolution = try await resolve(identifier)
      guard resolution.roundTripIdentifier == identifier else {
        emit("identity-mismatch")
        return 5
      }
      _ = resolution.userVisibleURL
      emit("resolved")
      return 0
    } catch {
      return 4
    }
  }

  public static func runSystem(arguments: [String] = CommandLine.arguments) async -> Int32 {
    await runSystem(
      arguments: arguments,
      readIdentifier: { readLine() },
      domains: { try await NSFileProviderManager.domains() },
      resolveURL: { domain, identifier in
        guard let manager = NSFileProviderManager(for: domain) else {
          throw InstalledPlaceholderResolutionError.domainUnavailable
        }
        return try await manager.getUserVisibleURL(
          for: NSFileProviderItemIdentifier(rawValue: identifier))
      },
      identifyURL: { try await identifierForUserVisibleFile(at: $0) },
      emit: { print($0) })
  }

  static func runSystem(
    arguments: [String],
    readIdentifier: () -> String?,
    domains: () async throws -> [NSFileProviderDomain],
    resolveURL: (NSFileProviderDomain, String) async throws -> URL,
    identifyURL: (URL) async throws -> (
      NSFileProviderItemIdentifier, NSFileProviderDomainIdentifier
    ),
    emit: (String) -> Void
  ) async -> Int32 {
    await run(
      arguments: arguments,
      readIdentifier: readIdentifier,
      resolve: { identifier in
        let availableDomains = try await domains()
        guard availableDomains.count == 1 else {
          throw InstalledPlaceholderResolutionError.domainUnavailable
        }
        let domain = availableDomains[0]
        let url = try await resolveURL(domain, identifier)
        let (roundTripIdentifier, roundTripDomain) = try await identifyURL(url)
        return InstalledPlaceholderResolution(
          userVisibleURL: url,
          roundTripIdentifier: roundTripDomain == domain.identifier
            ? roundTripIdentifier.rawValue : "")
      },
      emit: emit)
  }

  private static func identifierForUserVisibleFile(
    at url: URL
  ) async throws -> (NSFileProviderItemIdentifier, NSFileProviderDomainIdentifier) {
    try await withCheckedThrowingContinuation { continuation in
      NSFileProviderManager.getIdentifierForUserVisibleFile(at: url) {
        identifier, domain, error in
        if let error {
          continuation.resume(throwing: error)
        } else if let identifier, let domain {
          continuation.resume(returning: (identifier, domain))
        } else {
          continuation.resume(
            throwing: InstalledPlaceholderResolutionError.identityUnavailable)
        }
      }
    }
  }
}

private enum InstalledPlaceholderResolutionError: Error {
  case domainUnavailable
  case identityUnavailable
}
