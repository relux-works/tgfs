#!/bin/sh
# Make the pinned Rust toolchain available on a clean self-hosted macOS runner.
#
# This deliberately does not source a shell profile. GitHub Actions invokes
# every `run` block in a fresh non-interactive shell, so the bootstrap calls the
# rustup binary by its absolute CARGO_HOME path and publishes that directory via
# GITHUB_PATH for later steps. rust-toolchain.toml remains the single source of
# truth for the channel, profile, and required components.
set -eu

config_file="${1:-rust-toolchain.toml}"
rustup_version="1.29.0"

if [ ! -f "$config_file" ]; then
  echo "::error::pinned Rust configuration not found: $config_file" >&2
  exit 1
fi

read_toml_string() {
  key="$1"
  sed -n "s/^${key}[[:space:]]*=[[:space:]]*\"\([^\"]*\)\"[[:space:]]*$/\1/p" "$config_file"
}

toolchain="$(read_toml_string channel)"
profile="$(read_toml_string profile)"
components_line="$(sed -n 's/^components[[:space:]]*=[[:space:]]*\[\(.*\)\][[:space:]]*$/\1/p' "$config_file")"

if [ -z "$toolchain" ] || [ -z "$profile" ]; then
  echo "::error::rust-toolchain.toml must define a pinned channel and profile" >&2
  exit 1
fi

cargo_home="${CARGO_HOME:-$HOME/.cargo}"
rustup_home="${RUSTUP_HOME:-$HOME/.rustup}"
rustup_bin="$cargo_home/bin/rustup"

if [ ! -x "$rustup_bin" ]; then
  install_dir="$(mktemp -d)"
  trap 'rm -rf "$install_dir"' EXIT HUP INT TERM

  case "$(uname -m)" in
    x86_64)
      rustup_target="x86_64-apple-darwin"
      rustup_sha256="33cf85df9142bc6d29cbc62fa5ca1d4c29622cddb55213a4c1a43c457fb9b2d7"
      ;;
    arm64)
      rustup_target="aarch64-apple-darwin"
      rustup_sha256="aeb4105778ca1bd3c6b0e75768f581c656633cd51368fa61289b6a71696ac7e1"
      ;;
    *)
      echo "::error::unsupported macOS runner architecture: $(uname -m)" >&2
      exit 1
      ;;
  esac

  # The archive URL fixes rustup itself at 1.29.0. The committed digest is
  # verified before the download becomes executable, so TLS alone is never the
  # trust boundary for a clean runner bootstrap.
  rustup_init="$install_dir/rustup-init"
  curl --fail --location --silent --show-error --proto '=https' --tlsv1.2 \
    "https://static.rust-lang.org/rustup/archive/${rustup_version}/${rustup_target}/rustup-init" \
    --output "$rustup_init"
  if ! printf '%s  %s\n' "$rustup_sha256" "$rustup_init" | shasum -a 256 -c -; then
    echo "::error::rustup-init checksum verification failed" >&2
    exit 1
  fi
  chmod 0755 "$rustup_init"
  CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
    "$rustup_init" -y --no-modify-path --profile minimal --default-toolchain none
fi

if [ ! -x "$rustup_bin" ]; then
  echo "::error::rustup bootstrap did not create $rustup_bin" >&2
  exit 1
fi

CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
  "$rustup_bin" toolchain install "$toolchain" --profile "$profile"

# Shell word splitting is intentional after deleting TOML punctuation: component
# names are rustup identifiers and therefore cannot contain spaces or globs.
set -f
for component in $(printf '%s' "$components_line" | tr -d '"' | tr ',' ' '); do
  CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
    "$rustup_bin" component add --toolchain "$toolchain" "$component"
done
set +f

CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" \
  "$rustup_bin" default "$toolchain"

if [ -n "${GITHUB_PATH:-}" ] && ! grep -Fqx "$cargo_home/bin" "$GITHUB_PATH" 2>/dev/null; then
  printf '%s\n' "$cargo_home/bin" >> "$GITHUB_PATH"
fi

CARGO_HOME="$cargo_home" RUSTUP_HOME="$rustup_home" "$rustup_bin" show active-toolchain
"$cargo_home/bin/rustc" --version
"$cargo_home/bin/cargo" --version
