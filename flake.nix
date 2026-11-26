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
              "slskr-types"
              "-p"
              "soulseek"
            ];
            doCheck = false;
            buildFeatures = [ "embed-ui" ];
            env.SLSKR_EMBED_UI = "${slskr-web}";
            # nixpkgs' cargo setup hook sets LIBSQLITE3_SYS_USE_PKG_CONFIG=1, so the package
            # links this SQLite rather than rusqlite's bundled copy (Docker still bundles it).
            nativeBuildInputs = [ pkgs.pkg-config ];
            buildInputs = [
              pkgs.sqlite
            ];
            postInstall = ''
              install -Dm644 -t $out/share/doc/slskr LICENSE README.md
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

      checks = forAllSystems (pkgs: {
        slskr = self.packages.${pkgs.stdenv.hostPlatform.system}.slskr.overrideAttrs (_: {
          doCheck = true;
        });
      });

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
          # Link the shell's SQLite instead of compiling the bundled amalgamation on every
          # cold build, as the nix package does; this overrides rusqlite's `bundled` feature.
          # Plain `cargo` outside the shell, and the Docker image, still bundle it.
          LIBSQLITE3_SYS_USE_PKG_CONFIG = "1";
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
