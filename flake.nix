{
  description = "A lazygit-style terminal UI for operating Grafana";

  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
    flake-utils.url = "github:numtide/flake-utils";
  };

  outputs = { self, nixpkgs, flake-utils }:
    flake-utils.lib.eachDefaultSystem (system:
      let
        pkgs = nixpkgs.legacyPackages.${system};
        manifest = (pkgs.lib.importTOML ./Cargo.toml).package;
      in
      {
        packages.default = pkgs.rustPlatform.buildRustPackage {
          pname = manifest.name;
          version = manifest.version;
          src = pkgs.lib.cleanSource ./.;
          cargoLock.lockFile = ./Cargo.lock;
          # Integration tests need the docker dev stack; unit tests run in the sandbox.
          cargoTestFlags = [ "--lib" ];
          meta = with pkgs.lib; {
            description = manifest.description;
            license = with licenses; [ mit asl20 ];
            mainProgram = "lazygrafana";
          };
        };

        devShells.default = pkgs.mkShell {
          inputsFrom = [ self.packages.${system}.default ];
          packages = with pkgs; [ cargo clippy rustfmt rust-analyzer jq curl ];
        };
      });
}
