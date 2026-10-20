project('desklink', 'rust',
          version: '1.0.0',
    meson_version: '>= 1.0.0',
  default_options: [ 'buildtype=release', 'warning_level=2', 'werror=false', ],
)

i18n = import('i18n')
gnome = import('gnome')

subdir('data')
subdir('src')
subdir('po')

gnome.post_install(
     glib_compile_schemas: true,
    gtk_update_icon_cache: true,
  update_desktop_database: true,
)

# The GNOME Shell extension (gnome-shell-extension/README.md), for GNOME
# users of distribution packages; the app offers to enable it.
install_subdir('gnome-shell-extension/desklink@derx06.github.io',
  install_dir: get_option('datadir') / 'gnome-shell' / 'extensions',
)
