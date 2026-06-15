{
  description = "pifrost — NixOS raw disk image for immutable K3s nodes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    impermanence.url = "github:nix-community/impermanence";
  };

  outputs =
    { self, nixpkgs, impermanence }:
    let
      system = "x86_64-linux";
      pkgs = nixpkgs.legacyPackages.${system};
      lib = nixpkgs.lib;
    in
    {
      nixosConfigurations.pifrost-node = lib.nixosSystem {
        inherit system;
        modules = [
          impermanence.nixosModules.impermanence
          ./nix/configuration.nix
        ];
      };
    };
}
