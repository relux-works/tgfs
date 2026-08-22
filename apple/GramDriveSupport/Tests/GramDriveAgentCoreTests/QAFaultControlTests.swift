#if GRAMDRIVE_QA_FAULT_CONTROL
  import CryptoKit
  import Foundation
  import GramDriveCore
  import GramDriveSupport
  import Testing

  @testable import GramDriveAgentCore

  private let qaTestSecretHex =
    "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"

  private final class QASuccessHydrator: ContentHydrating, @unchecked Sendable {
    private let lock = NSLock()
    private var callCount = 0

    var calls: Int {
      lock.lock()
      defer { lock.unlock() }
      return callCount
    }

    func hydrate(
      _ request: HydrationRequest,
      progress: @escaping @Sendable (HydrationProgress) -> Void,
      token: CancellationToken
    ) async throws -> HydratedContent {
      recordCall()
      return HydratedContent(
        stagedPath: "/qa/synthetic.bin",
        contentVersion: request.contentVersion,
        byteCount: 1)
    }

    private func recordCall() {
      lock.lock()
      callCount += 1
      lock.unlock()
    }
  }

  private func qaRecord(
    fault: QAHydrationFault,
    request: HydrationRequest,
    expiresAtMs: Int64 = 2_000
  ) throws -> QAFaultControlRecord {
    var record = QAFaultControlRecord(
      schema: QAFaultControlRecord.schema,
      nonce: String(repeating: "a", count: 32),
      expiresAtMs: expiresAtMs,
      accountId: request.accountId,
      itemId: request.itemId,
      purpose: request.purpose,
      fault: fault,
      mac: "")
    let key = SymmetricKey(data: Data(qaTestSecretHex.utf8).hexDecoded())
    let signature = HMAC<SHA256>.authenticationCode(
      for: try record.authenticatedBytes(), using: key)
    record.mac = Data(signature).map { String(format: "%02x", $0) }.joined()
    return record
  }

  private func writeQARecord(_ record: QAFaultControlRecord, dataRoot: URL) throws -> URL {
    let directory = dataRoot.appendingPathComponent("qa", isDirectory: true)
    try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
    let url = directory.appendingPathComponent(QAHydrationFaultControl.recordName)
    let encoder = JSONEncoder()
    encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
    try encoder.encode(record).write(to: url, options: .atomic)
    try FileManager.default.setAttributes(
      [.posixPermissions: 0o600], ofItemAtPath: url.path)
    return url
  }

  extension Data {
    fileprivate func hexDecoded() -> Data {
      let text = String(decoding: self, as: UTF8.self)
      var bytes: [UInt8] = []
      var index = text.startIndex
      while index < text.endIndex {
        let next = text.index(index, offsetBy: 2)
        bytes.append(UInt8(text[index..<next], radix: 16)!)
        index = next
      }
      return Data(bytes)
    }
  }

  @Suite("Compile-time QA hydration fault control", .serialized)
  struct QAFaultControlTests {
    private let request = HydrationRequest(
      accountId: 77,
      itemId: "qa-synthetic-item-001",
      contentVersion: "qa-v1")

    @Test("Python harness and Swift parser share one canonical HMAC contract")
    func crossLanguageRecordVector() throws {
      let record = try qaRecord(fault: .sourceNotFound, request: request)
      #expect(
        record.mac
          == "857572f09b8ae32984389edcf6a158cde197b1e1f91fc89364dfee5df8d9a934")
    }

    @Test(
      "Authenticated fault matrix is item and purpose scoped", arguments: QAHydrationFault.allCases)
    func authenticatedMatrix(fault: QAHydrationFault) throws {
      try withTemporaryDirectory { root in
        let control = QAHydrationFaultControl(dataRoot: root, nowMs: { 1_000 })
        _ = try writeQARecord(qaRecord(fault: fault, request: request), dataRoot: root)
        let expected: QAHydrationFaultDisposition =
          switch fault {
          case .timeout: .timeout
          case .transport: .transport
          case .rendererSourceNotFound, .unavailableContent: .failure(.sourceUnavailable)
          case .sourceNotFound: .failure(.notFound)
          }
        #expect(control.disposition(for: request) == expected)
        var otherItem = request
        otherItem.itemId = "qa-synthetic-item-002"
        #expect(control.disposition(for: otherItem) == nil)
        var otherPurpose = request
        otherPurpose.purpose = .thumbnail
        #expect(control.disposition(for: otherPurpose) == nil)
      }
    }

    @Test("Tampering expiry and broad file permissions fail closed")
    func invalidRecordsAreIgnored() throws {
      try withTemporaryDirectory { root in
        let control = QAHydrationFaultControl(dataRoot: root, nowMs: { 1_500 })
        var record = try qaRecord(fault: .sourceNotFound, request: request)
        let url = try writeQARecord(record, dataRoot: root)
        record.itemId = "tampered"
        _ = try writeQARecord(record, dataRoot: root)
        #expect(control.disposition(for: request) == nil)

        record = try qaRecord(
          fault: .sourceNotFound, request: request, expiresAtMs: 1_499)
        _ = try writeQARecord(record, dataRoot: root)
        #expect(control.disposition(for: request) == nil)

        record = try qaRecord(fault: .sourceNotFound, request: request)
        _ = try writeQARecord(record, dataRoot: root)
        try FileManager.default.setAttributes(
          [.posixPermissions: 0o644], ofItemAtPath: url.path)
        #expect(control.disposition(for: request) == nil)
      }
    }

    @Test(
      "Real hydration channel injects each fault and recovers after clear",
      arguments: QAHydrationFault.allCases)
    func realChannelFailureThenRecovery(fault: QAHydrationFault) async throws {
      try await withTemporaryDirectoryAsync { root in
        let socket = root.appendingPathComponent("hydration.sock")
        let control = QAHydrationFaultControl(dataRoot: root, nowMs: { 1_000 })
        let recordURL = try writeQARecord(
          qaRecord(fault: fault, request: request), dataRoot: root)
        let hydrator = QASuccessHydrator()
        let server = try HydrationServer.startQAFaultControlled(
          socketURL: socket,
          registry: TransferRegistry(),
          admission: { _ in .admit },
          hydrator: hydrator,
          faultControl: control)
        defer { server.stop() }
        let client = AgentHydrationClient(
          socketURL: { socket }, idleTimeout: .seconds(1))

        switch fault {
        case .timeout, .transport:
          await #expect(throws: HydrationTransportError.self) {
            _ = try await client.hydrate(request) { _ in }
          }
        case .rendererSourceNotFound, .unavailableContent:
          do {
            _ = try await client.hydrate(request) { _ in }
            Issue.record("expected source-unavailable QA failure")
          } catch let failure as HydrationFailure {
            #expect(failure.category == .sourceUnavailable)
          }
        case .sourceNotFound:
          do {
            _ = try await client.hydrate(request) { _ in }
            Issue.record("expected source-not-found QA failure")
          } catch let failure as HydrationFailure {
            #expect(failure.category == .notFound)
          }
        }
        #expect(hydrator.calls == 0)

        try FileManager.default.removeItem(at: recordURL)
        let recovered = try await client.hydrate(request) { _ in }
        #expect(recovered.contentVersion == request.contentVersion)
        #expect(hydrator.calls == 1)
      }
    }

    @Test("Durable admission refusal wins before the QA fault")
    func admissionPrecedesFault() async throws {
      try await withTemporaryDirectoryAsync { root in
        let socket = root.appendingPathComponent("hydration.sock")
        let control = QAHydrationFaultControl(dataRoot: root, nowMs: { 1_000 })
        _ = try writeQARecord(
          qaRecord(fault: .transport, request: request), dataRoot: root)
        let hydrator = QASuccessHydrator()
        let server = try HydrationServer.startQAFaultControlled(
          socketURL: socket,
          registry: TransferRegistry(),
          admission: { _ in
            .refuse(HydrationFailure(category: .notFound, detail: "durable row absent"))
          },
          hydrator: hydrator,
          faultControl: control)
        defer { server.stop() }
        let client = AgentHydrationClient(socketURL: { socket })
        do {
          _ = try await client.hydrate(request) { _ in }
          Issue.record("expected durable admission refusal")
        } catch let failure as HydrationFailure {
          #expect(failure.category == .notFound)
        }
        #expect(hydrator.calls == 0)
      }
    }
  }
#endif
