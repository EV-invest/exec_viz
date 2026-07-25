{
  nixConfig = {
    extra-substituters = [ "https://valeratrades.cachix.org" ];
    extra-trusted-public-keys = [ "valeratrades.cachix.org-1:gXVwhzO5YB+BaiEJYT48qZgzdaErGQew6xtZcz4Fo1Q=" ];
  };

  inputs = {
    v_flakes.url = "github:valeratrades/v_flakes?ref=v1.6";
  };

  outputs = { self, v_flakes }:
    let
      inherit (v_flakes) flake-utils pre-commit-hooks;
      manifest = (builtins.fromTOML (builtins.readFile ./exec_viz/Cargo.toml)).package;
      pname = manifest.name;
    in
    flake-utils.lib.eachDefaultSystem (
      system:
      let
        pkgs = import v_flakes.default_nixpkgs { inherit system; };
        rust = v_flakes.rs.default_nightly system;
        pre-commit-check = pre-commit-hooks.lib.${system}.run (v_flakes.files.preCommit { inherit pkgs; });
        stdenv = pkgs.stdenvAdapters.useMoldLinker pkgs.stdenv;

        rs = v_flakes.rs {
          inherit pkgs rust;
          build = {
            deny = false;
            workspace = let deprecate_by = "v1.0.0"; in {
              "./exec_viz/" = [ "git_version" "log_directives" { deprecate = { by_version = deprecate_by; force = true; }; } ];
            };
          };
        };
        github = v_flakes.github {
          inherit pkgs pname rs;
          enable = true;
          lastSupportedVersion = "nightly-2026-07-16";
          jobs.default = true;
        };
        readme = v_flakes.readme-fw {
          inherit pkgs pname;
          defaults = true;
          lastSupportedVersion = "nightly-1.92";
          rootDir = ./.;
          badges = [ "msrv" "crates_io" "docs_rs" "loc" "ci" ];
        };
        combined = v_flakes.utils.combine { inherit rust; modules = [ rs github readme ]; };

        # Pinned to match the workspace's `wasm-bindgen` (`=0.2.125`, as in scam_pump_liqs);
        # nixpkgs ships a different minor and a CLI/crate schema skew is a hard error. Placed
        # first on the wrapper PATH so `dx` uses it, not a stale ~/.cargo/bin/wasm-bindgen.
        wasm-bindgen-cli =
          let
            src = pkgs.fetchCrate {
              pname = "wasm-bindgen-cli";
              version = "0.2.125";
              hash = "sha256-zRawtjxMOdTMX+mZaiNR3YYfTiZJhf9qj7kXSSeMxrc=";
            };
          in
          pkgs.buildWasmBindgenCli {
            inherit src;
            cargoDeps = pkgs.rustPlatform.fetchCargoVendor {
              inherit src;
              inherit (src) pname version;
              hash = "sha256-aZCfgR23Qb0Pn4Mm4ToMtuuRQqSJjXCR9li/VvP5CTM=";
            };
          };
        # `nix run . -- <demo|live>` → build the wasm bundle here, then run the sibling
        # trading_data example that attaches the viz; it serves the bundle + /api + /lwc_draw.js
        # on one port. The toolchain is pinned here, so the runner lives here even though the apps
        # don't. `dx build` runs from this repo root: dx 0.7 canonicalizes the workspace
        # `default-members` relative to CWD.
        runViz = pkgs.writeShellApplication {
          name = "run-exec-viz";
          runtimeInputs = (with pkgs; [ rust dioxus-cli git pkg-config openssl cmake clang mold gcc ]) ++ [ wasm-bindgen-cli ];
          text = ''
            case "''${1:-demo}" in
              demo) pkg=trading_data_demo ;;
              live) pkg=trading_data_live_example ;;
              *) echo "usage: nix run . -- <demo|live>" >&2; exit 1 ;;
            esac
            repo="$(git rev-parse --show-toplevel)"
            cd "$repo"
            dx build --package ${pname} --no-default-features --features web
            export EXEC_VIZ_WEB_DIR="$repo/target/dx/${pname}/debug/web/public"
            exec cargo run --manifest-path "$repo/../trading_data/Cargo.toml" -p "$pkg"
          '';
        };
      in
      {
        apps.default = {
          type = "app";
          program = "${runViz}/bin/run-exec-viz";
        };

        packages =
          let
            rustc = rust;
            cargo = rust;
            rustPlatform = pkgs.makeRustPlatform {
              inherit rustc cargo stdenv;
            };
          in
          {
            default = rustPlatform.buildRustPackage {
              inherit pname;
              version = manifest.version;

              buildInputs = with pkgs; [
                openssl.dev
              ];
              nativeBuildInputs = with pkgs; [ pkg-config ];

              cargoLock.lockFile = ./Cargo.lock;
              src = pkgs.lib.cleanSource ./.;
            };
          };

        devShells.default =
          with pkgs;
          mkShell {
            inherit stdenv;
            shellHook =
              pre-commit-check.shellHook
              + combined.shellHook
              + ''
                cp -f ${(v_flakes.files.treefmt) { inherit pkgs; }} ./.treefmt.toml
              '';

            packages = [
              mold
              openssl
              pkg-config
              rust
              dioxus-cli
              wasm-bindgen-cli
              # `viz demo` / `viz live` — same as `nix run . -- <example>`.
              (writeShellScriptBin "viz" ''exec ${runViz}/bin/run-exec-viz "$@"'')
            ] ++ pre-commit-check.enabledPackages ++ combined.enabledPackages;

            env.RUST_BACKTRACE = 1;
            env.RUST_LIB_BACKTRACE = 0;
          };
      }
    );
}
