{
  description = "slskr - a Soulseek server daemon and web UI written in Rust";

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
              !(pkgs.lib.hasPrefix "ref" rel || pkgs.lib.hasPrefix "target" rel || rel == "result");
          };
          cargoLock.lockFile = ./Cargo.lock;
          version = "0.1.0";

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
              cd crates/slskr-web
              HOME=$TMPDIR NO_COLOR=true trunk build --release --frozen --dist "$TMPDIR/dist"
              cd ../..
              runHook postBuild
            '';
            installPhase = ''
              runHook preInstall
              mkdir -p $out/share/slskr
              cp -r $TMPDIR/dist $out/share/slskr/wwwroot
              cp LICENSE NOTICE $out/share/slskr/wwwroot/
              cp -r licenses $out/share/slskr/wwwroot/
              runHook postInstall
            '';
            doCheck = false;
          };

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
            nativeBuildInputs = [ pkgs.makeWrapper pkgs.pkg-config ];
            buildInputs = [
              pkgs.openssl
              pkgs.sqlite
            ];
            postInstall = ''
              mkdir -p $out/share/slskr
              cp -r ${slskr-web}/share/slskr/wwwroot $out/share/slskr/wwwroot
              cp LICENSE NOTICE $out/
              cp LICENSE $out/bin/
              cp -r licenses $out/
              wrapProgram $out/bin/slskr --set-default SLSKR_CONTENT_PATH $out/share/slskr/wwwroot
            '';
            meta.mainProgram = "slskr";
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
          buildInputs = [ pkgs.openssl ];
        };
      });

      formatter = forAllSystems (pkgs: pkgs.nixfmt);
    };
}
