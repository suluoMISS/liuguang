{
  description = "流光 Liuguang — Linux 动态壁纸引擎（Wayland 壁纸层 / DRM-KMS）";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = f: nixpkgs.lib.genAttrs systems (system: f nixpkgs.legacyPackages.${system});
      version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;
    in
    {
      overlays.default = final: _prev: {
        liuguang = final.callPackage ./nix/liuguang.nix { inherit version; };
        liuguang-gui = final.callPackage ./nix/liuguang-gui.nix { inherit version; };
      };

      packages = forAllSystems (
        pkgs:
        let
          p = self.overlays.default pkgs pkgs;
        in
        {
          inherit (p) liuguang liuguang-gui;
          default = p.liuguang;
        }
      );

      nixosModules.default = import ./nix/module.nix self;

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          inputsFrom = [
            self.packages.${pkgs.stdenv.hostPlatform.system}.liuguang
            self.packages.${pkgs.stdenv.hostPlatform.system}.liuguang-gui
          ];
          packages = with pkgs; [
            rustfmt
            clippy
            rust-analyzer
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
