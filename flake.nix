{
  description = "DeskLink: use an Android phone and a Linux desktop together";

  inputs.nixpkgs.url = "github:NixOS/nixpkgs/nixos-26.05";

  outputs =
    { self, nixpkgs }:
    let
      systems = [
        "x86_64-linux"
        "aarch64-linux"
      ];
      forAll = f: nixpkgs.lib.genAttrs systems (system: f system nixpkgs.legacyPackages.${system});
    in
    {
      packages = forAll (
        system: pkgs: {
          default = pkgs.stdenv.mkDerivation {
            pname = "desklink";
            version = "1.0.0";
            src = ./.;

            # Meson wraps `cargo build --locked`; the hook points cargo at
            # these vendored crates.
            cargoDeps = pkgs.rustPlatform.importCargoLock { lockFile = ./Cargo.lock; };

            nativeBuildInputs = with pkgs; [
              meson
              ninja
              pkg-config
              cargo
              rustc
              rustPlatform.cargoSetupHook
              wrapGAppsHook4 # also carries GST_PLUGIN_SYSTEM_PATH_1_0 into the wrapper
              glib # glib-compile-resources, glib-compile-schemas
              gettext
              desktop-file-utils
            ];

            buildInputs =
              (with pkgs; [
                gtk4
                libadwaita
                openssl
                libnice # nicesrc/nicesink for webrtcbin
                pipewire # pipewiresrc for desktop capture
              ])
              ++ (with pkgs.gst_all_1; [
                gstreamer
                gst-plugins-base
                gst-plugins-good # VP8, RTP, ximagesrc
                gst-plugins-bad # webrtcbin
                gst-libav # H.264 decoding
              ]);
            # gtk4paintablesink is compiled into the binary (gst-plugin-gtk4).

            meta = {
              description = "Use an Android phone and a Linux desktop together";
              homepage = "https://github.com/derpx06/DeskLink";
              license = nixpkgs.lib.licenses.gpl3Plus;
              mainProgram = "desklink";
              platforms = systems;
            };
          };
        }
      );

      devShells = forAll (
        system: pkgs: {
          default = pkgs.mkShell {
            inputsFrom = [ self.packages.${system}.default ];
            packages = with pkgs; [
              clippy
              rustfmt
              rust-analyzer
            ];
          };
        }
      );
    };
}
