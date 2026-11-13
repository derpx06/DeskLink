pkgdatadir = get_option('prefix') / get_option('datadir') / meson.project_name()
gnome = import('gnome')

gnome.compile_resources('desklink',
  'desklink.gresource.xml',
  gresource_bundle: true,
  install: true,
  install_dir: pkgdatadir,
)

# The build floor (packaging/README.md), checked here so an old distribution
# fails with one clear line instead of deep inside a -sys crate's build script.
dependency('gtk4', version: '>= 4.12')
dependency('libadwaita-1', version: '>= 1.5')
dependency('gstreamer-1.0', version: '>= 1.22')
dependency('gstreamer-webrtc-1.0', version: '>= 1.22')
dependency('gstreamer-sdp-1.0', version: '>= 1.22')
dependency('gstreamer-video-1.0', version: '>= 1.22')
dependency('openssl', version: '>= 3.0')
# The Terminals page's interactive terminal. Without VTE 0.76 (GTK 4 build)
# DeskLink still builds: a session's detail is a read-only preview with
# "Open in terminal window", which opens the system terminal.
vte_dep = dependency('vte-2.91-gtk4', version: '>= 0.76', required: get_option('vte'))

cargo_bin  = find_program('cargo')
cargo_opt  = [ '--manifest-path', meson.project_source_root() / 'Cargo.toml' ]
cargo_opt += [ '--target-dir', meson.project_build_root()  / 'src', '--locked' ]
# src/config.rs reads this at compile time. CARGO_HOME is left alone so Flatpak's
# vendored offline sources (and the user's registry cache) are used.
cargo_env  = [ 'DESKLINK_LOCALEDIR=' + get_option('prefix') / get_option('localedir') ]

if not vte_dep.found()
  cargo_opt += [ '--no-default-features' ]
endif

# Optimised unless debugging: distro wrappers use buildtype=plain (arch-meson).
if not get_option('debug')
  cargo_opt += [ '--release' ]
  rust_target = 'release'
else
  rust_target = 'debug'
endif

cargo_build = custom_target(
  'cargo-build',
  build_by_default: true,
  build_always_stale: true,
  output: meson.project_name(),
  console: true,
  install: true,
  install_dir: get_option('bindir'),
  command: [
    'env', cargo_env,
    cargo_bin, 'build',
    cargo_opt, '&&', 'cp', 'src' / rust_target / meson.project_name(), '@OUTPUT@',
  ]
)
