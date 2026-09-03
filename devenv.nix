{ pkgs, ... }:

{
  packages = with pkgs; [
    cargo-audit
    cargo-deny
    go-task
  ];

  languages.rust = {
    enable = true;
    toolchainFile = ./rust-toolchain.toml;
  };

  git-hooks.hooks = {
    clippy = {
      enable = true;
      settings.allFeatures = true;
    };
    rustfmt.enable = true;
  };

  enterTest = ''
    task check
  '';
}
