{
  description = "Texel, a PlayStation emulator for the browser, with a self-hosted game library";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    rust-overlay = {
      url = "github:oxalica/rust-overlay";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      rust-overlay,
    }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems =
        f:
        nixpkgs.lib.genAttrs systems (
          system:
          f (
            import nixpkgs {
              inherit system;
              overlays = [ rust-overlay.overlays.default ];
            }
          )
        );
    in
    {
      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = with pkgs; [
            # Toolchain and targets come from rust-toolchain.toml.
            (rust-bin.fromRustupToolchainFile ./rust-toolchain.toml)
            rust-analyzer
            # Must match the exact `wasm-bindgen` version in Cargo.toml.
            wasm-bindgen-cli_0_2_130
            binaryen
            cargo-deny
            # chdman: converting discs to CHD, and verifying CHDs (.assets/verify.py).
            mame-tools
            nodejs_26
            pnpm
            yamllint
            nixfmt
            # Inspecting the GPU wgpu runs on (crates/ps2-gpu).
            vulkan-tools
          ];
          # wgpu loads the Vulkan loader at run time (crates/ps2-gpu): the
          # drivers themselves come from the system (hardware.graphics).
          LD_LIBRARY_PATH = pkgs.lib.makeLibraryPath [ pkgs.vulkan-loader ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt-tree);
    };
}
