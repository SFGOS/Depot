project(
  'depot',
  version: '0.23.1',
  meson_version: '>=0.60.0',
  default_options: ['buildtype=release'],
)

fs = import('fs')

cargo = find_program('cargo', required: true)
find_prog = find_program('find', required: true)
sh = find_program('sh', required: true)
build_html_doc = get_option('build-html-doc')
cli_doc = find_program('cli_doc', required: build_html_doc)

src_root = meson.project_source_root()
build_root = meson.project_build_root()
src_root_prefix = src_root + '/'

cargo_profile = 'debug'
cargo_release = false
if get_option('buildtype') != 'debug'
  cargo_profile = 'release'
  cargo_release = true
endif

cargo_inputs = files('Cargo.toml')
if fs.is_file('Cargo.lock')
  cargo_inputs += files('Cargo.lock')
endif

cargo_src_list = run_command(
  find_prog,
  join_paths(src_root, 'src'),
  '-type',
  'f',
  '(',
  '-name',
  '*.rs',
  '-o',
  '-name',
  '*.toml',
  ')',
  check: true,
  capture: true,
).stdout().strip()
foreach rel : cargo_src_list.split('\n')
  if rel == ''
    continue
  endif
  if rel.startswith(src_root_prefix)
    rel = rel.replace(src_root_prefix, '')
  endif
  cargo_inputs += files(rel)
endforeach

depot_bin = custom_target(
  'depot-bin',
  input: cargo_inputs,
  output: 'depot',
  command: [
    sh,
    join_paths(src_root, 'tools', 'meson', 'cargo-build.sh'),
    cargo,
    src_root,
    build_root,
    cargo_profile,
    cargo_release ? '1' : '0',
    '@OUTPUT@',
  ],
  console: true,
  build_by_default: true,
  install: true,
  install_dir: get_option('bindir'),
)

if build_html_doc
  custom_target(
    'depot-html-doc',
    input: depot_bin,
    output: 'depot.html',
    command: [
      cli_doc,
      '--output-filename',
      '@OUTPUT@',
      join_paths(build_root, 'depot'),
    ],
    build_by_default: true,
    install: true,
    install_dir: join_paths(get_option('datadir'), 'doc', meson.project_name()),
  )
endif

test(
  'cargo-test',
  sh,
  args: [
    join_paths(src_root, 'tools', 'meson', 'cargo-test.sh'),
    cargo.full_path(),
    src_root,
    build_root,
    cargo_profile,
    cargo_release ? '1' : '0',
  ],
  suite: ['cargo'],
  timeout: 0,
)

# System configuration examples installed under the actual runtime lookup paths.
install_data(
  'contrib/depot.toml',
  install_dir: get_option('sysconfdir'),
)

example_sysconf_dir = join_paths(get_option('sysconfdir'), 'depot.d')
example_depotd_list = run_command(
  find_prog,
  join_paths(src_root, 'contrib', 'depot.d'),
  '-maxdepth',
  '1',
  '-type',
  'f',
  '-name',
  '*.toml',
  check: true,
  capture: true,
).stdout().strip()
example_depotd_files = []
foreach rel : example_depotd_list.split('\n')
  if rel == ''
    continue
  endif
  if rel.startswith(src_root_prefix)
    rel = rel.replace(src_root_prefix, '')
  endif
  example_depotd_files += files(rel)
endforeach
if example_depotd_files.length() > 0
  install_data(example_depotd_files, install_dir: example_sysconf_dir)
endif

# Per-user example is documentation, not a system config file.
install_data(
  'contrib/user.depot.toml.example',
  install_dir: join_paths(get_option('datadir'), 'doc', meson.project_name(), 'examples'),
)
install_data(
  'contrib/README.md',
  install_dir: join_paths(get_option('datadir'), 'doc', meson.project_name()),
)

meson.add_install_script(
  sh,
  join_paths(src_root, 'tools', 'meson', 'install-cli-assets.sh'),
  join_paths(build_root, 'depot'),
  join_paths(get_option('datadir'), 'bash-completion', 'completions'),
  join_paths(get_option('datadir'), 'zsh', 'site-functions'),
  join_paths(get_option('datadir'), 'fish', 'vendor_completions.d'),
  join_paths(get_option('mandir'), 'man1'),
)
