{
  pkgs,
  lib,
  config,
  ...
}:
{
  # https://devenv.sh/languages/
  languages.rust = {
    enable = true;
    channel = "stable";
    components = [ "rustc" "cargo" "clippy" "rustfmt" "rust-analyzer" ];
    # Cross-compile the bridge to the theatre MacBook (Apple Silicon) from Linux via `zig` as the
    # linker (cargo-zigbuild); this adds the macOS std for that target to the toolchain.
    targets = [ "aarch64-apple-darwin" ];
  };

  # See full reference at https://devenv.sh/reference/options/
}
