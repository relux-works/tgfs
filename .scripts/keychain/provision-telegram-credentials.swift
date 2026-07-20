// Provision the Telegram api credentials into the login keychain so the
// *signed product binaries* can read them without a consent prompt
// (BUG-260720-3i74u1).
//
// Why a signed Swift tool and not `security add-generic-password`: a file
// keychain item carries two independent gates — the trusted-application
// ACL and the partition list (the creator's code-signing partition,
// e.g. `apple-tool:` for the `security` CLI, `teamid:...` for a signed
// app). An item created by the `security` CLI is partition-locked to
// `apple-tool:`, so a Developer ID binary reading it always triggers the
// interactive consent prompt, ACL or not — and changing the partition
// list of an existing item requires the keychain password. Creating the
// items *from a binary signed with the product's team* sets the partition
// to that team at creation, and an explicit trusted-application ACL
// covers the product binaries; both gates then pass silently.
//
// Usage (driven by provision_telegram_credentials.py, which compiles and
// signs this source with the product's Developer ID identity):
//
//   provision-telegram-credentials --agent PATH --app PATH
//   provision-telegram-credentials --check   # prove the binary runs; no I/O
//
// The api_id/api_hash values are read from the GRAMDRIVE_API_ID /
// GRAMDRIVE_API_HASH environment variables — never from argv (visible in
// `ps`) and never echoed.
//
// The tool only ever *writes* the credentials (from its environment); it has
// no read/consume path. A signed binary that could read these items and hand
// them to an arbitrary command would be a promptless secret-exfiltration
// primitive, so no such mode exists and the tool does not name itself in the
// items' ACL — only the product binaries (and the `security` CLI for dev
// inspection) may read them.

import Foundation
import Security

let service = "gramdrive-telegram"

func fail(_ message: String) -> Never {
    FileHandle.standardError.write(Data(("provision: " + message + "\n").utf8))
    exit(1)
}

var arguments = Array(CommandLine.arguments.dropFirst())
// A side-effect-free liveness probe: the driver runs it after signing to
// prove the binary loads and runs on this machine *before* it clears the
// existing keychain items. Reads and writes nothing.
if arguments.first == "--check" {
    print("provision-telegram-credentials: ok")
    exit(0)
}
// The trusted-application ACL never includes this tool: it has no read path,
// so self-trust would only widen the items' reach for no benefit.
var trustedPaths: [String] = ["/usr/bin/security"]
while !arguments.isEmpty {
    let flag = arguments.removeFirst()
    guard flag == "--agent" || flag == "--app", !arguments.isEmpty else {
        fail("usage: provision-telegram-credentials --agent PATH --app PATH")
    }
    trustedPaths.append(arguments.removeFirst())
}

let environment = ProcessInfo.processInfo.environment
guard
    let apiId = environment["GRAMDRIVE_API_ID"], !apiId.isEmpty,
    let apiHash = environment["GRAMDRIVE_API_HASH"], !apiHash.isEmpty
else {
    fail("GRAMDRIVE_API_ID / GRAMDRIVE_API_HASH must be set in the environment")
}

// The trusted-application ACL: the product binaries and the security CLI
// (dev inspection) only — never this tool, which has no read path. The
// SecTrustedApplication/SecAccess API family is deprecated but remains
// the only way to author file-keychain ACLs, which is exactly what the
// product's vault reads (SecItemCopyMatching over the login keychain).
var trusted: [SecTrustedApplication] = []
for path in trustedPaths {
    var application: SecTrustedApplication?
    let status = SecTrustedApplicationCreateFromPath(path, &application)
    guard status == errSecSuccess, let application else {
        fail("cannot trust \(path): \(status)")
    }
    trusted.append(application)
}
var access: SecAccess?
let accessStatus = SecAccessCreate("Telegram api credentials" as CFString, trusted as CFArray, &access)
guard accessStatus == errSecSuccess, let access else {
    fail("SecAccessCreate failed: \(accessStatus)")
}

for (account, value) in [("api_id", apiId), ("api_hash", apiHash)] {
    let match: [String: Any] = [
        kSecClass as String: kSecClassGenericPassword,
        kSecAttrService as String: service,
        kSecAttrAccount as String: account,
    ]
    // Replacement of an item this tool created earlier; items created by
    // other owners (the `security` CLI) refuse the delete with
    // errSecInvalidOwnerEdit and are removed by the driver script instead —
    // if one survives anyway, the add below reports the duplicate.
    let deleteStatus = SecItemDelete(match as CFDictionary)
    guard
        deleteStatus == errSecSuccess || deleteStatus == errSecItemNotFound
            || deleteStatus == errSecInvalidOwnerEdit
    else {
        fail("cannot replace \(account): delete failed \(deleteStatus)")
    }
    var add = match
    add[kSecValueData as String] = Data(value.utf8)
    add[kSecAttrAccess as String] = access
    let addStatus = SecItemAdd(add as CFDictionary, nil)
    guard addStatus == errSecSuccess else {
        fail("cannot add \(account): \(addStatus)")
    }
    print("provisioned \(service)/\(account)")
}
