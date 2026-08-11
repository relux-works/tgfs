import Darwin
import Foundation

/// UNIX-domain descriptor passing for the one generated-file hand-off. The
/// payload byte and `SCM_RIGHTS` control record are sent together, so a crash
/// after `sendmsg` leaves the File Provider process with an independent open
/// reference to the immutable source inode.
public enum UnixFileDescriptorTransfer {
  private static let controlAlignment = MemoryLayout<cmsghdr>.alignment
  private static let controlHeaderSize = aligned(MemoryLayout<cmsghdr>.size)
  private static let descriptorSize = MemoryLayout<Int32>.size
  private static let controlSpace = controlHeaderSize + aligned(descriptorSize)

  /// Writes the first payload byte with a duplicated descriptor, then writes
  /// the remaining bytes normally. The caller retains ownership of `fd`.
  public static func send(_ data: Data, fileDescriptor fd: Int32, on socket: Int32) throws {
    guard let first = data.first else {
      throw UnixSocketError.failed(operation: "sendmsg", code: EINVAL)
    }
    var firstByte = [first]
    var control = [UInt8](repeating: 0, count: controlSpace)
    let sent = firstByte.withUnsafeMutableBytes { firstBytes in
      control.withUnsafeMutableBytes { controlBytes in
        let header = controlBytes.baseAddress!.assumingMemoryBound(to: cmsghdr.self)
        header.pointee.cmsg_len = socklen_t(controlHeaderSize + descriptorSize)
        header.pointee.cmsg_level = SOL_SOCKET
        header.pointee.cmsg_type = SCM_RIGHTS
        let descriptor = controlBytes.baseAddress!
          .advanced(by: controlHeaderSize)
          .assumingMemoryBound(to: Int32.self)
        descriptor.pointee = fd
        var iov = iovec(iov_base: firstBytes.baseAddress, iov_len: firstBytes.count)
        return withUnsafeMutablePointer(to: &iov) { iovPointer in
          var message = msghdr(
            msg_name: nil,
            msg_namelen: 0,
            msg_iov: iovPointer,
            msg_iovlen: 1,
            msg_control: controlBytes.baseAddress,
            msg_controllen: socklen_t(controlBytes.count),
            msg_flags: 0)
          return sendmsg(socket, &message, 0)
        }
      }
    }
    guard sent == 1 else {
      throw UnixSocketError.failed(operation: "sendmsg", code: sent < 0 ? errno : EIO)
    }
    if data.count > 1 {
      try writeAll(Data(data.dropFirst()), on: socket)
    }
  }

  /// Reads one byte chunk and returns any received `SCM_RIGHTS` descriptor.
  /// The receiver owns the returned descriptor and must close it exactly once.
  public static func receive(into buffer: inout [UInt8], on socket: Int32) throws -> (
    count: Int, fileDescriptor: Int32?
  ) {
    var control = [UInt8](repeating: 0, count: controlSpace)
    let result: (Int, Int32?) = try buffer.withUnsafeMutableBytes { bytes in
      try control.withUnsafeMutableBytes { controlBytes in
        var iov = iovec(iov_base: bytes.baseAddress, iov_len: bytes.count)
        return try withUnsafeMutablePointer(to: &iov) { iovPointer in
          var message = msghdr(
            msg_name: nil,
            msg_namelen: 0,
            msg_iov: iovPointer,
            msg_iovlen: 1,
            msg_control: controlBytes.baseAddress,
            msg_controllen: socklen_t(controlBytes.count),
            msg_flags: 0)
          let count = recvmsg(socket, &message, 0)
          guard count >= 0 else {
            throw UnixSocketError.failed(operation: "recvmsg", code: errno)
          }
          guard message.msg_flags & Int32(MSG_CTRUNC) == 0 else {
            throw UnixSocketError.failed(operation: "recvmsg", code: EMSGSIZE)
          }
          return (
            count,
            receivedDescriptor(from: controlBytes, length: Int(message.msg_controllen))
          )
        }
      }
    }
    return (result.0, result.1)
  }

  private static func receivedDescriptor(
    from control: UnsafeMutableRawBufferPointer,
    length: Int
  ) -> Int32? {
    guard length >= controlHeaderSize + descriptorSize else { return nil }
    let header = control.baseAddress!.assumingMemoryBound(to: cmsghdr.self).pointee
    guard header.cmsg_level == SOL_SOCKET,
      header.cmsg_type == SCM_RIGHTS,
      Int(header.cmsg_len) >= controlHeaderSize + descriptorSize
    else { return nil }
    let descriptor = control.baseAddress!
      .advanced(by: controlHeaderSize)
      .assumingMemoryBound(to: Int32.self)
      .pointee
    _ = fcntl(descriptor, F_SETFD, FD_CLOEXEC)
    return descriptor
  }

  private static func writeAll(_ data: Data, on socket: Int32) throws {
    try data.withUnsafeBytes { raw in
      var offset = 0
      while offset < raw.count {
        let written = write(socket, raw.baseAddress!.advanced(by: offset), raw.count - offset)
        guard written > 0 else {
          throw UnixSocketError.failed(operation: "write", code: errno)
        }
        offset += written
      }
    }
  }

  private static func aligned(_ value: Int) -> Int {
    (value + controlAlignment - 1) & ~(controlAlignment - 1)
  }
}
