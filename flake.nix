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
            tomlFormat = pkgs.formats.toml { };
            serverType = lib.types.submodule {
              options = {
                command = lib.mkOption {
                  type = lib.types.str;
                  description = "Language-server executable or command name.";
                };

                args = lib.mkOption {
                  type = lib.types.listOf lib.types.str;
                  default = [ ];
                  description = "Arguments passed directly to the language server.";
                };

                environment = lib.mkOption {
                  type = lib.types.attrsOf lib.types.str;
                  default = { };
                  description = "Environment variables added to the language-server process.";
                };

                fileExtensions = lib.mkOption {
                  type = lib.types.attrsOf lib.types.str;
                  default = { };
                  example = {
                    ".rs" = "rust";
                  };
                  description = "File-name suffixes mapped to LSP language identifiers.";
                };

                filePatterns = lib.mkOption {
                  type = lib.types.attrsOf lib.types.str;
                  default = { };
                  example = {
                    "**/CMakeLists.txt" = "cmake";
                  };
                  description = "Project-relative glob patterns mapped to LSP language identifiers.";
                };

                initializationOptions = lib.mkOption {
                  type = tomlFormat.type;
                  default = { };
                  description = "Language-server initialization options.";
                };

                timeouts = lib.mkOption {
                  default = { };
                  description = "Language-server request and shutdown timeouts.";
                  type = lib.types.submodule {
                    options = {
                      requestMs = lib.mkOption {
                        type = lib.types.ints.positive;
                        default = 30000;
                        description = "Request timeout in milliseconds.";
                      };

                      shutdownMs = lib.mkOption {
                        type = lib.types.ints.positive;
                        default = 5000;
                        description = "Shutdown timeout in milliseconds.";
                      };
                    };
                  };
                };
              };
            };
            generatedConfig = tomlFormat.generate "deixis-config.toml" {
              servers = lib.mapAttrs (_: server: {
                inherit (server)
                  args
                  command
                  environment
                  ;
                file_extensions = server.fileExtensions;
                file_patterns = server.filePatterns;
                initialization_options = server.initializationOptions;
                timeouts = {
                  request_ms = server.timeouts.requestMs;
                  shutdown_ms = server.timeouts.shutdownMs;
                };
              }) cfg.servers;
            };
            hasConfig = cfg.configFile != null || cfg.servers != { };
            configSource = if cfg.configFile != null then cfg.configFile else generatedConfig;
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

              configFile = lib.mkOption {
                type = lib.types.nullOr lib.types.path;
                default = null;
                description = ''
                  Existing Deixis TOML configuration to install in the user
                  configuration directory. This cannot be combined with
                  `programs.deixis.servers`.
                '';
              };

              servers = lib.mkOption {
                type = lib.types.attrsOf serverType;
                default = { };
                description = ''
                  Named language servers. When nonempty, Home Manager generates
                  `deixis/config.toml` and registers Deixis as an MCP server.
                '';
              };
            };

            config = lib.mkIf cfg.enable (
              lib.mkMerge [
                {
                  assertions = [
                    {
                      assertion = cfg.configFile == null || cfg.servers == { };
                      message = "programs.deixis.configFile and programs.deixis.servers are mutually exclusive";
                    }
                  ];
                  home.packages = [ cfg.package ];
                }

                (lib.mkIf hasConfig {
                  xdg.configFile."deixis/config.toml".source = configSource;
                  programs.mcp = {
                    enable = true;
                    servers.deixis.command = lib.getExe cfg.package;
                  };
                })
              ]
            );
          };
      };
    };
}
