{
  description = "Sommelier-RS packages for ChromeOS VirtWL";

  inputs.nixpkgs.url = "https://flakehub.com/f/DeterminateSystems/nixpkgs-weekly/0.1";

  outputs = { self, nixpkgs }:
    let
      supportedSystems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs supportedSystems;
    in
    {
      overlays.default = final: _prev: {
        sommelier-rs = final.callPackage ./nix/package-source.nix {
          src = self;
        };
        sommelier-rs-bin = final.callPackage ./nix/package-bin.nix { };
      };

      packages = forAllSystems (
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ self.overlays.default ];
          };
        in
        {
          default = pkgs.sommelier-rs-bin;
          inherit (pkgs) sommelier-rs sommelier-rs-bin;
        }
      );
    };
}
