import FileProvider
import Foundation
import Testing

@testable import GramDriveFileProvider

@Suite("Installed placeholder resolution command")
struct InstalledPlaceholderResolutionCommandTests {
  // ItemKey::Canonical(Account(42)).id().text(), pinned by identity_golden.rs.
  private let validIdentifier = "gdaeaqaaaaaaaaaabk"

  @Test("Only the exact first command flag selects the headless entry point")
  func exactFlagSelectsCommand() {
    #expect(
      InstalledPlaceholderResolutionCommand.isRequested(
        arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag]))
    #expect(
      !InstalledPlaceholderResolutionCommand.isRequested(
        arguments: ["GramDrive", "other", InstalledPlaceholderResolutionCommand.flag]))
    #expect(!InstalledPlaceholderResolutionCommand.isRequested(arguments: ["GramDrive"]))
  }

  @Test("The exact opaque identifier resolves without appearing in output")
  func exactIdentifierResolvesPrivately() async {
    var resolved: String?
    var output: [String] = []

    let exitCode = await InstalledPlaceholderResolutionCommand.run(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      resolve: {
        resolved = $0
        return InstalledPlaceholderResolution(
          userVisibleURL: URL(fileURLWithPath: "/private/opaque-placeholder"),
          roundTripIdentifier: $0)
      },
      emit: { output.append($0) })

    #expect(exitCode == 0)
    #expect(resolved == validIdentifier)
    #expect(output == ["resolved"])
    #expect(!output.joined().contains(validIdentifier))
  }

  @Test("Missing, malformed, noncanonical, non-ItemId, and extra input fail before File Provider")
  func invalidInputFailsClosed() async {
    var resolveCount = 0
    let resolver: (String) async throws -> InstalledPlaceholderResolution = { identifier in
      resolveCount += 1
      return InstalledPlaceholderResolution(
        userVisibleURL: URL(fileURLWithPath: "/private/should-not-resolve"),
        roundTripIdentifier: identifier)
    }

    let missing = await InstalledPlaceholderResolutionCommand.run(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { nil }, resolve: resolver, emit: { _ in })
    let invalidIdentifiers = [
      "gdNOT-BASE32",
      "gdm",  // impossible unpadded Base32 length residue
      "gdaebah",  // nonzero trailing padding bits
      "gdaebag",  // canonical Base32 bytes, not a structured ItemId
    ]
    var malformedExitCodes: [Int32] = []
    for identifier in invalidIdentifiers {
      malformedExitCodes.append(
        await InstalledPlaceholderResolutionCommand.run(
          arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
          readIdentifier: { identifier }, resolve: resolver, emit: { _ in }))
    }
    let extra = await InstalledPlaceholderResolutionCommand.run(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag, "extra"],
      readIdentifier: { validIdentifier }, resolve: resolver, emit: { _ in })

    #expect(missing == 2)
    #expect(malformedExitCodes.allSatisfy { $0 == 2 })
    #expect(extra == 2)
    #expect(resolveCount == 0)
  }

  @Test("A remapped URL whose round-trip identity differs fails with a fixed category")
  func remappedURLIdentityMismatchFailsClosed() async {
    var output: [String] = []
    let exitCode = await InstalledPlaceholderResolutionCommand.run(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      resolve: { _ in
        InstalledPlaceholderResolution(
          userVisibleURL: URL(fileURLWithPath: "/private/remapped-placeholder"),
          roundTripIdentifier: "gdaeaqaaaaaaaaaabi")
      },
      emit: { output.append($0) })

    #expect(exitCode == 5)
    #expect(output == ["identity-mismatch"])
    #expect(!output.joined().contains(validIdentifier))
  }

  @Test("The production adapter round-trips the resolved URL and domain")
  func systemAdapterRoundTripsURLAndDomain() async {
    let domainIdentifier = NSFileProviderDomainIdentifier(rawValue: "test-domain")
    let domain = NSFileProviderDomain(identifier: domainIdentifier, displayName: "Test")
    let resolvedURL = URL(fileURLWithPath: "/private/returned-by-provider")
    var resolvedInput: String?
    var identifiedURL: URL?
    var output: [String] = []

    let exitCode = await InstalledPlaceholderResolutionCommand.runSystem(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      domains: { [domain] },
      resolveURL: { receivedDomain, identifier in
        #expect(receivedDomain.identifier == domainIdentifier)
        resolvedInput = identifier
        return resolvedURL
      },
      identifyURL: { url in
        identifiedURL = url
        return (NSFileProviderItemIdentifier(validIdentifier), domainIdentifier)
      },
      emit: { output.append($0) })

    #expect(exitCode == 0)
    #expect(resolvedInput == validIdentifier)
    #expect(identifiedURL == resolvedURL)
    #expect(output == ["resolved"])
  }

  @Test("A remapped production URL cannot cross an identity or domain boundary")
  func systemAdapterRejectsRoundTripMismatch() async {
    let domainIdentifier = NSFileProviderDomainIdentifier(rawValue: "test-domain")
    let otherDomain = NSFileProviderDomainIdentifier(rawValue: "other-domain")
    let domain = NSFileProviderDomain(identifier: domainIdentifier, displayName: "Test")
    var output: [String] = []

    let exitCode = await InstalledPlaceholderResolutionCommand.runSystem(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      domains: { [domain] },
      resolveURL: { _, _ in URL(fileURLWithPath: "/private/remapped") },
      identifyURL: { _ in
        (NSFileProviderItemIdentifier(validIdentifier), otherDomain)
      },
      emit: { output.append($0) })

    #expect(exitCode == 5)
    #expect(output == ["identity-mismatch"])
  }

  @Test("A production URL that round-trips to another item fails in the same domain")
  func systemAdapterRejectsRoundTripItemMismatch() async {
    let domainIdentifier = NSFileProviderDomainIdentifier(rawValue: "test-domain")
    let domain = NSFileProviderDomain(identifier: domainIdentifier, displayName: "Test")
    var output: [String] = []

    let exitCode = await InstalledPlaceholderResolutionCommand.runSystem(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      domains: { [domain] },
      resolveURL: { _, _ in URL(fileURLWithPath: "/private/remapped") },
      identifyURL: { _ in
        (NSFileProviderItemIdentifier("gdaeaqaaaaaaaaaabi"), domainIdentifier)
      },
      emit: { output.append($0) })

    #expect(exitCode == 5)
    #expect(output == ["identity-mismatch"])
    #expect(!output.joined().contains(validIdentifier))
  }

  @Test("An absent or ambiguous domain fails before URL resolution")
  func systemAdapterRequiresExactlyOneDomain() async {
    var resolveCount = 0
    let exitCode = await InstalledPlaceholderResolutionCommand.runSystem(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      domains: { [] },
      resolveURL: { _, _ in
        resolveCount += 1
        return URL(fileURLWithPath: "/private/should-not-resolve")
      },
      identifyURL: { _ in
        (NSFileProviderItemIdentifier(validIdentifier), .init(rawValue: "unused"))
      },
      emit: { _ in })

    #expect(exitCode == 4)
    #expect(resolveCount == 0)
  }

  @Test("Multiple production domains fail before URL resolution or identification")
  func systemAdapterRejectsAmbiguousDomains() async {
    let first = NSFileProviderDomain(
      identifier: .init(rawValue: "first-domain"), displayName: "First")
    let second = NSFileProviderDomain(
      identifier: .init(rawValue: "second-domain"), displayName: "Second")
    var resolveCount = 0
    var identifyCount = 0
    var output: [String] = []

    let exitCode = await InstalledPlaceholderResolutionCommand.runSystem(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      domains: { [first, second] },
      resolveURL: { _, _ in
        resolveCount += 1
        return URL(fileURLWithPath: "/private/should-not-resolve")
      },
      identifyURL: { _ in
        identifyCount += 1
        return (NSFileProviderItemIdentifier(validIdentifier), first.identifier)
      },
      emit: { output.append($0) })

    #expect(exitCode == 4)
    #expect(resolveCount == 0)
    #expect(identifyCount == 0)
    #expect(output.isEmpty)
  }

  @Test("A File Provider read failure is never reported as resolution")
  func providerFailureStaysFailure() async {
    var output: [String] = []
    let exitCode = await InstalledPlaceholderResolutionCommand.run(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      resolve: { _ in throw CocoaError(.fileReadNoSuchFile) },
      emit: { output.append($0) })

    #expect(exitCode == 4)
    #expect(output.isEmpty)
  }

  @Test("Cancellation returns the fixed provider failure without resolving")
  func cancellationFailsClosed() async {
    let identifier = validIdentifier
    let command = Task {
      await InstalledPlaceholderResolutionCommand.run(
        arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
        readIdentifier: { identifier },
        resolve: { _ in
          try await Task.sleep(for: .seconds(60))
          Issue.record("cancelled resolver continued to a resolution")
          return InstalledPlaceholderResolution(
            userVisibleURL: URL(fileURLWithPath: "/private/should-not-resolve"),
            roundTripIdentifier: identifier)
        },
        emit: { _ in })
    }

    command.cancel()
    let exitCode = await command.value

    #expect(exitCode == 4)
  }

  @Test("A production provider-stage throw stays failure")
  func systemAdapterProviderFailureStaysFailure() async {
    let domain = NSFileProviderDomain(
      identifier: .init(rawValue: "test-domain"), displayName: "Test")
    var identifyCount = 0
    var output: [String] = []

    let exitCode = await InstalledPlaceholderResolutionCommand.runSystem(
      arguments: ["GramDrive", InstalledPlaceholderResolutionCommand.flag],
      readIdentifier: { validIdentifier },
      domains: { [domain] },
      resolveURL: { _, _ in throw CocoaError(.fileReadNoSuchFile) },
      identifyURL: { _ in
        identifyCount += 1
        return (NSFileProviderItemIdentifier(validIdentifier), domain.identifier)
      },
      emit: { output.append($0) })

    #expect(exitCode == 4)
    #expect(identifyCount == 0)
    #expect(output.isEmpty)
  }
}
