# The service binaries the flake builds, one attribute each. The attribute
# name is the flake output name (`packages.<name>`, `packages.image-<name>`),
# the installed executable (`bin/<name>`) and the image name
# (`localhost/mw/<name>`).
#
#   package  the cargo package (`cargo build -p <package>`)
#   dir      that package's directory, for its version (image label)
#   example  build `--example <example>`, or
#   bin      build `--bin <bin>` (exactly one of the two)
#   features optional extra cargo features of <package>
#
# flake.nix keeps only the entries whose `<dir>/Cargo.toml` exists, so an
# entry may name a crate that has not landed yet (mw-posix-node): it builds,
# gets its image and joins the Pi image as soon as the directory (a
# workspace member) appears, and until then nothing refers to it.
#
# To add a binary, add one line here; nothing else in the flake changes.
{
  hello = { package = "mw-service"; dir = "service"; example = "hello"; };
  mw-robot = { package = "mw-robot"; dir = "robot"; bin = "mw-robot"; };
  mw-gateway = { package = "mw-gateway"; dir = "gateway"; bin = "mw-gateway"; };
  mw-posix-node = { package = "mw-posix-node"; dir = "posix-node"; bin = "mw-posix-node"; };
  # The updates channel (design.md UP): the server (its image runs in the
  # cluster beside the updates zenohd), the host client and the publisher.
  mw-updates = { package = "mw-updates"; dir = "updates"; bin = "mw-updates"; };
  mw-updater = { package = "mw-updates"; dir = "updates"; bin = "mw-updater"; };
  mw-release = { package = "mw-updates"; dir = "updates"; bin = "mw-release"; };
}
