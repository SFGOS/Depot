Contrib: example system and user configuration files for Depot

Place these files on your system to provide sensible defaults and examples
for `/etc/depot.toml`, `/etc/depot.d/build.toml`, or user-level configs.

Recommended installation:

  # System-wide
  sudo cp contrib/depot.toml /etc/depot.toml
  sudo mkdir -p /etc/depot.d
  sudo cp -r contrib/depot.d/* /etc/depot.d/

  # Per-user (example)
  mkdir -p ~/.config
  cp contrib/user.depot.toml.example ~/.config/depot.toml

Notes
 - These files are examples only. Review and adapt to your distribution and
   security policies before deploying.
 - The config loader supports append syntax (e.g. `cflags += ["-g"]`).
