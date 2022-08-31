{
  inputs = {
    nixpkgs.url = "github:NixOS/nixpkgs/nixos-unstable";
  };

  outputs = { self, nixpkgs, ... }: {
    devShells."x86_64-linux".default = with nixpkgs.legacyPackages."x86_64-linux"; mkShell {
      name = "dev-env";
      buildInputs = [
        rustup # need nightly for now
        # rustc
        # cargo
        # clippy
        # rustfmt
        rust-analyzer
        lldb
      ];
    };
  };
}
