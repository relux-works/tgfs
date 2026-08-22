#if GRAMDRIVE_QA_FAULT_CONTROL
  import CryptoKit
  import Darwin
  import Foundation
  import GramDriveQAFaultSecret
  import GramDriveSupport

  /// Compile-time-only fault classes for the installed File Provider QA bundle.
  /// This file has no declarations in ordinary builds.
  enum QAHydrationFault: String, Codable, CaseIterable, Sendable {
    case timeout
    case transport
    case rendererSourceNotFound = "renderer_source_not_found"
    case sourceNotFound = "source_not_found"
    case unavailableContent = "unavailable_content"
  }

  enum QAHydrationFaultDisposition: Equatable, Sendable {
    case timeout
    case transport
    case failure(HydrationFailureCategory)
  }

  /// Authenticated, item-scoped control payload. The record is deliberately
  /// separate from normal agent control IPC: it is one App-Group-local file,
  /// available only in a build that linked the per-build secret target.
  struct QAFaultControlRecord: Codable, Equatable, Sendable {
    static let schema = "gramdrive.qa-fault-control.v1"

    var schema: String
    var nonce: String
    var expiresAtMs: Int64
    var accountId: Int64
    var itemId: String
    var purpose: HydrationPurpose
    var fault: QAHydrationFault
    var mac: String

    enum CodingKeys: String, CodingKey {
      case schema
      case nonce
      case expiresAtMs = "expires_at_ms"
      case accountId = "account_id"
      case itemId = "item_id"
      case purpose
      case fault
      case mac
    }

    private struct AuthenticatedFields: Codable {
      let schema: String
      let nonce: String
      let expiresAtMs: Int64
      let accountId: Int64
      let itemId: String
      let purpose: HydrationPurpose
      let fault: QAHydrationFault

      enum CodingKeys: String, CodingKey {
        case schema
        case nonce
        case expiresAtMs = "expires_at_ms"
        case accountId = "account_id"
        case itemId = "item_id"
        case purpose
        case fault
      }
    }

    func authenticatedBytes() throws -> Data {
      let encoder = JSONEncoder()
      encoder.outputFormatting = [.sortedKeys, .withoutEscapingSlashes]
      return try encoder.encode(
        AuthenticatedFields(
          schema: schema,
          nonce: nonce,
          expiresAtMs: expiresAtMs,
          accountId: accountId,
          itemId: itemId,
          purpose: purpose,
          fault: fault))
    }
  }

  /// Reads one persistent fault armed for one stable item identity. The file is
  /// intentionally not consumed: retries continue to fail until the harness
  /// explicitly clears it, making fault-clearance recovery deterministic even
  /// when macOS performs an automatic retry.
  final class QAHydrationFaultControl: @unchecked Sendable {
    static let recordName = "qa-fault-control-v1.json"
    static let maximumRecordBytes = 8 * 1024

    let recordURL: URL
    private let lock = NSLock()
    private let nowMs: @Sendable () -> Int64
    private let secret: SymmetricKey

    init(
      dataRoot: URL,
      nowMs: @escaping @Sendable () -> Int64 = {
        Int64((Date().timeIntervalSince1970 * 1_000).rounded())
      }
    ) {
      recordURL =
        dataRoot
        .appendingPathComponent("qa", isDirectory: true)
        .appendingPathComponent(Self.recordName, isDirectory: false)
      self.nowMs = nowMs
      let hex = String(cString: gramdrive_qa_fault_secret())
      precondition(hex.count == 64, "invalid QA fault-control build secret")
      self.secret = SymmetricKey(data: Data(hexadecimal: hex)!)
    }

    func disposition(for request: HydrationRequest) -> QAHydrationFaultDisposition? {
      lock.lock()
      defer { lock.unlock() }
      guard let data = readSecureRecord(),
        let record = try? JSONDecoder().decode(QAFaultControlRecord.self, from: data),
        record.schema == QAFaultControlRecord.schema,
        record.expiresAtMs >= nowMs(),
        record.accountId == request.accountId,
        record.itemId == request.itemId,
        record.purpose == request.purpose,
        record.nonce.count >= 32,
        record.nonce.allSatisfy({ $0.isHexDigit }),
        let suppliedMAC = Data(hexadecimal: record.mac),
        suppliedMAC.count == SHA256.byteCount,
        let authenticated = try? record.authenticatedBytes(),
        HMAC<SHA256>.isValidAuthenticationCode(
          suppliedMAC, authenticating: authenticated, using: secret)
      else {
        return nil
      }
      switch record.fault {
      case .timeout:
        return .timeout
      case .transport:
        return .transport
      case .rendererSourceNotFound, .unavailableContent:
        return .failure(.sourceUnavailable)
      case .sourceNotFound:
        return .failure(.notFound)
      }
    }

    private func readSecureRecord() -> Data? {
      let descriptor = open(recordURL.path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW)
      guard descriptor >= 0 else { return nil }
      defer { close(descriptor) }
      var facts = stat()
      guard fstat(descriptor, &facts) == 0,
        (facts.st_mode & S_IFMT) == S_IFREG,
        facts.st_uid == getuid(),
        (facts.st_mode & 0o777) == 0o600,
        facts.st_size > 0,
        facts.st_size <= Self.maximumRecordBytes
      else {
        return nil
      }
      var data = Data(count: Int(facts.st_size))
      let readCount = data.withUnsafeMutableBytes { bytes in
        read(descriptor, bytes.baseAddress, bytes.count)
      }
      guard readCount == data.count else { return nil }
      return data
    }
  }

  extension Data {
    fileprivate init?(hexadecimal: String) {
      guard hexadecimal.count.isMultiple(of: 2),
        hexadecimal.allSatisfy({ $0.isHexDigit })
      else { return nil }
      var bytes: [UInt8] = []
      bytes.reserveCapacity(hexadecimal.count / 2)
      var index = hexadecimal.startIndex
      while index < hexadecimal.endIndex {
        let next = hexadecimal.index(index, offsetBy: 2)
        guard let byte = UInt8(hexadecimal[index..<next], radix: 16) else { return nil }
        bytes.append(byte)
        index = next
      }
      self = Data(bytes)
    }
  }
#endif
