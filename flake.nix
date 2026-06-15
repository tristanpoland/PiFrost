{
  description = "pifrost — NixOS raw disk image for immutable K3s nodes";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-24.11";
    impermanence.url = "github:nix-community/impermanence";
    nixos-generators = {
      url = "github:nix-community/nixos-generators";
      inputs.nixpkgs.follows = "nixpkgs";
    };
  };

  outputs =
    {
      self,
      nixpkgs,
      impermanence,
      nixos-generators,
    }:
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

      rawImage = nixos-generators.nixosGenerate {
        inherit system;
        format = "raw";
        modules = [
          impermanence.nixosModules.impermanence
          ./nix/configuration.nix
        ];
      };
    };
}
