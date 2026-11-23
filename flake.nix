{
  description = "slskr: a Soulseek client daemon with a web UI";

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
      toolchainFor =
        pkgs:
        pkgs.rust-bin.stable.latest.default.override {
          targets = [ "wasm32-unknown-unknown" ];
          extensions = [
            "rust-src"
            "rust-analyzer"
          ];
        };
    in
    {
      packages = forAllSystems (
        pkgs:
        let
          toolchain = toolchainFor pkgs;
          rustPlatform = pkgs.makeRustPlatform {
            cargo = toolchain;
            rustc = toolchain;
          };
          src = pkgs.lib.cleanSourceWith {
            src = ./.;
            filter =
              path: type:
              let
                rel = pkgs.lib.removePrefix (toString ./. + "/") (toString path);
              in
              !(builtins.any (prefix: pkgs.lib.hasPrefix prefix rel) [
                "ref"
                "target"
                "dist"
                "result"
                "data"
                ".direnv"
              ]);
          };
          cargoLock.lockFile = ./Cargo.lock;
          version = (builtins.fromTOML (builtins.readFile ./Cargo.toml)).workspace.package.version;

          # The web UI (trunk output). The daemon embeds it at build time.
          slskr-web = rustPlatform.buildRustPackage {
            pname = "slskr-web";
            inherit version src cargoLock;
            nativeBuildInputs = [
              pkgs.trunk
              pkgs.wasm-bindgen-cli
              pkgs.binaryen
            ];
            buildPhase = ''
              runHook preBuild
              (cd crates/slskr-web && HOME=$TMPDIR NO_COLOR=true trunk build --release --frozen --dist "$TMPDIR/dist")
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out
              cp -r $TMPDIR/dist/. $out/
              runHook postInstall
            '';
            doCheck = false;
          };

          # A single binary with the web UI built in.
          slskr = rustPlatform.buildRustPackage {
            pname = "slskr";
            inherit version src cargoLock;
            cargoBuildFlags = [
              "-p"
              "slskr"
            ];
            cargoTestFlags = [
              "-p"
              "slskr"
              "-p"
              "soulseek"
            ];
            env.SLSKR_EMBED_UI = "${slskr-web}";
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [
              pkgs.sqlite
            ];
            postInstall = ''
              install -Dm644 -t $out/share/doc/slskr LICENSE NOTICE README.md
              cp -r licenses $out/share/doc/slskr/
            '';
            meta = {
              description = "Soulseek client daemon with a web UI";
              license = pkgs.lib.licenses.agpl3Plus;
              mainProgram = "slskr";
            };
          };
        in
        {
          inherit slskr slskr-web;
          default = slskr;
        }
      );

      devShells = forAllSystems (pkgs: {
        default = pkgs.mkShell {
          packages = [
            (toolchainFor pkgs)
            pkgs.trunk
            pkgs.wasm-bindgen-cli
            pkgs.binaryen
            pkgs.cargo-watch
            pkgs.sqlite
            pkgs.pkg-config
          ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
