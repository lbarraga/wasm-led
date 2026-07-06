{
  description = "RP2350 Bare Metal WS2812 Rust Environment";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay.url = "github:oxalica/rust-overlay";
  };

  outputs = {
    self,
    nixpkgs,
    rust-overlay,
  }: let
    system = "x86_64-linux";
    pkgs = import nixpkgs {
      inherit system;
      overlays = [(import rust-overlay)];
    };

    # Cortex-M33 target for the RP2350 chip
    rustToolchain = pkgs.rust-bin.stable.latest.default.override {
      targets = ["wasm32-unknown-unknown" "thumbv8m.main-none-eabihf"];
    };
  in {
    devShells.${system}.default = pkgs.mkShell {
      packages = [
        rustToolchain
        pkgs.elf2uf2-rs # Tool to automatically flash UF2 files over USB
        pkgs.picotool # Official utility for Pico management
        pkgs.mpremote
        pkgs.wasm-tools
        pkgs.probe-rs-tools
      ];
    };
  };
}
