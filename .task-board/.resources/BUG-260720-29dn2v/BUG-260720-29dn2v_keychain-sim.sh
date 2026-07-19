#!/bin/bash
# Simulate release.yml's throwaway-keychain lifecycle on the persistent runner
# with a DUMMY self-signed identity (no real secret touches the box), then
# prove no residue: keychain gone, default keychain and search list restored.
# Mirrors the workflow exactly: capture-verbatim at import, restore at cleanup.
set -euo pipefail
SIM="$HOME/gramdrive-ci/keychain-sim"
rm -rf "$SIM" && mkdir -p "$SIM"
KEYCHAIN_PATH="$SIM/gramdrive-signing.keychain-db"

echo "== BEFORE =="
security default-keychain -d user | tee "$SIM/default-before.txt"
security list-keychains -d user | tee "$SIM/search-before.txt"

openssl req -x509 -newkey rsa:2048 -keyout "$SIM/key.pem" -out "$SIM/cert.pem" \
  -days 1 -nodes -subj "/CN=GramDrive Keychain Sim/O=sim" >/dev/null 2>&1
openssl pkcs12 -export -out "$SIM/cert.p12" -inkey "$SIM/key.pem" -in "$SIM/cert.pem" \
  -passout pass:simpass >/dev/null 2>&1

# — release.yml "Name the throwaway" capture —
ORIGINAL_DEFAULT_KEYCHAIN="$(security default-keychain -d user | sed 's/^[[:space:]]*//;s/"//g')"
ORIGINAL_KEYCHAIN_SEARCH_LIST="$(security list-keychains -d user | sed 's/^[[:space:]]*//;s/"//g')"
keychain_pw="$(openssl rand -base64 24)"

# — release.yml import-step keychain sequence —
security create-keychain -p "$keychain_pw" "$KEYCHAIN_PATH"
security set-keychain-settings -lut 21600 "$KEYCHAIN_PATH"
security unlock-keychain -p "$keychain_pw" "$KEYCHAIN_PATH"
security default-keychain -s "$KEYCHAIN_PATH"
# shellcheck disable=SC2046
security list-keychains -d user -s "$KEYCHAIN_PATH" \
  $(security list-keychains -d user | sed 's/"//g')
security import "$SIM/cert.p12" -k "$KEYCHAIN_PATH" -P simpass \
  -T /usr/bin/codesign -T /usr/bin/security
security set-key-partition-list -S apple-tool:,apple: -k "$keychain_pw" "$KEYCHAIN_PATH" >/dev/null
rm -f "$SIM/cert.p12"

# — release.yml Developer ID CA import (BUG-260720-29dn2v) —
# Mirror the workflow's CA download+pin+import and prove the mechanic that the
# bug fix rests on: the "Developer ID Certification Authority" (G2) intermediate
# lands in the throwaway keychain, and a swapped cert (wrong pin) fails CLOSED.
# (The dummy self-signed identity above is NOT issued by G2, so this sim cannot
# assert `find-identity -v` validity — only the live run with the real p12 can.
# This section guards the exact lines the fix added to release.yml.)
devid_ca="$SIM/DeveloperIDG2CA.cer"
devid_ca_sha256="f16cd3c54c7f83cea4bf1a3e6a0819c8aaa8e4a1528fd144715f350643d2df3a"
ca_fail=0
curl --fail --location --silent --show-error --output "$devid_ca" \
  https://www.apple.com/certificateauthority/DeveloperIDG2CA.cer
echo "$devid_ca_sha256  $devid_ca" | shasum -a 256 --check --status
security import "$devid_ca" -k "$KEYCHAIN_PATH"
if security find-certificate -c "Developer ID Certification Authority" "$KEYCHAIN_PATH" >/dev/null 2>&1; then
  echo "CA import: G2 intermediate present"
else
  echo "CA import: INTERMEDIATE MISSING"; ca_fail=1
fi
if echo "0000000000000000000000000000000000000000000000000000000000000000  $devid_ca" \
  | shasum -a 256 --check --status; then
  echo "CA pin: WRONG HASH ACCEPTED"; ca_fail=1
else
  echo "CA pin: fails closed on mismatch"
fi
rm -f "$devid_ca"

echo "== DURING =="
security default-keychain -d user
security list-keychains -d user
security find-identity -p codesigning "$KEYCHAIN_PATH" | head -3 || true

# — release.yml always()-cleanup sequence —
security delete-keychain "$KEYCHAIN_PATH" 2>/dev/null || true
rm -f "$SIM/cert.p12" "$SIM/asc_key.p8" "$SIM/DeveloperIDG2CA.cer" 2>/dev/null || true
if [ -n "${ORIGINAL_KEYCHAIN_SEARCH_LIST:-}" ]; then
  old_ifs="$IFS"; IFS=$'\n'
  # shellcheck disable=SC2086
  security list-keychains -d user -s $ORIGINAL_KEYCHAIN_SEARCH_LIST || true
  IFS="$old_ifs"
else
  security list-keychains -d user -s "$HOME/Library/Keychains/login.keychain-db" || true
fi
if [ -n "${ORIGINAL_DEFAULT_KEYCHAIN:-}" ]; then
  security default-keychain -d user -s "$ORIGINAL_DEFAULT_KEYCHAIN" 2>/dev/null || true
fi

echo "== AFTER =="
security default-keychain -d user | tee "$SIM/default-after.txt"
security list-keychains -d user | tee "$SIM/search-after.txt"

echo "== VERDICT =="
fail=0
if [ -f "$KEYCHAIN_PATH" ]; then echo "KEYCHAIN FILE STILL PRESENT"; fail=1; else echo "keychain file: GONE"; fi
if diff -q "$SIM/default-before.txt" "$SIM/default-after.txt" >/dev/null; then echo "default keychain: RESTORED"; else echo "DEFAULT KEYCHAIN NOT RESTORED"; fail=1; fi
if diff -q "$SIM/search-before.txt" "$SIM/search-after.txt" >/dev/null; then echo "search list: RESTORED"; else echo "SEARCH LIST NOT RESTORED"; diff "$SIM/search-before.txt" "$SIM/search-after.txt" || true; fail=1; fi
if security list-keychains -d user | grep -q gramdrive-signing; then echo "RESIDUE IN SEARCH LIST"; fail=1; else echo "search list: no gramdrive-signing residue"; fi
if [ "${ca_fail:-0}" -ne 0 ]; then echo "CA mechanic: FAILED"; fail=1; else echo "CA mechanic: OK (G2 present + pin fails closed)"; fi
rm -f "$SIM/key.pem" "$SIM/cert.pem"
[ "$fail" -eq 0 ] && echo "SIM OK" || { echo "SIM FAILED"; exit 1; }
