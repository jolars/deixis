{
  description = "A portable MCP server for language-server operations";

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
        "aarch64-darwin"
        "aarch64-linux"
        "x86_64-darwin"
        "x86_64-linux"
      ];
      forAllSystems = nixpkgs.lib.genAttrs systems;
      packageFor =
        system:
        let
          pkgs = import nixpkgs {
            inherit system;
            overlays = [ rust-overlay.overlays.default ];
          };
          toolchain = pkgs.rust-bin.fromRustupToolchainFile ./rust-toolchain.toml;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
        in
        rustPlatform.buildRustPackage {
          pname = "deixis";
          version = "0.1.0";

          src = pkgs.lib.fileset.toSource {
            root = ./.;
            fileset = pkgs.lib.fileset.unions [
              ./Cargo.lock
              ./Cargo.toml
              ./LICENSE-APACHE
              ./LICENSE-MIT
              ./README.md
              ./src
              ./tests
            ];
          };
          cargoLock.lockFile = ./Cargo.lock;

          meta = {
            description = "An MCP server for interacting with language servers";
            homepage = "https://github.com/jolars/deixis";
            license = with pkgs.lib.licenses; [
              asl20
              mit
            ];
            mainProgram = "deixis";
            platforms = pkgs.lib.platforms.unix;
          };
        };
    in
    {
      packages = forAllSystems (
        system:
        let
          deixis = packageFor system;
        in
        {
          inherit deixis;
          default = deixis;
        }
      );

      apps = forAllSystems (
        system:
        let
          app = {
            type = "app";
            program = "${self.packages.${system}.deixis}/bin/deixis";
            meta.description = "Run the Deixis MCP server";
          };
        in
        {
          deixis = app;
          default = app;
        }
      );

      checks = forAllSystems (system: {
        default = self.packages.${system}.deixis;
      });

      formatter = forAllSystems (system: (import nixpkgs { inherit system; }).nixfmt);

      homeManagerModules = {
        default = self.homeManagerModules.deixis;

        deixis =
          {
            config,
            lib,
            pkgs,
            ...
          }:
          let
            cfg = config.programs.deixis;
          in
          {
            options.programs.deixis = {
              enable = lib.mkEnableOption "Deixis MCP server";

              package = lib.mkOption {
                type = lib.types.package;
                default = self.packages.${pkgs.stdenv.hostPlatform.system}.deixis;
                defaultText = lib.literalExpression "inputs.deixis.packages.\${pkgs.system}.deixis";
                description = "The Deixis package to install and expose as an MCP server.";
              };
            };

            config = lib.mkIf cfg.enable {
              home.packages = [ cfg.package ];
              programs.mcp = {
                enable = true;
                servers.deixis.command = lib.getExe cfg.package;
              };
            };
          };
      };
    };
}
