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
# entry may name a crate that has not landed yet (mw-pi-node): it builds,
# gets its image and joins the Pi image as soon as the directory (a
# workspace member) appears, and until then nothing refers to it.
#
# To add a binary, add one line here; nothing else in the flake changes.
{
  hello = { package = "mw-service"; dir = "service"; example = "hello"; };
  mw-robot = { package = "mw-robot"; dir = "robot"; bin = "mw-robot"; };
  mw-gateway = { package = "mw-gateway"; dir = "gateway"; bin = "mw-gateway"; };
  mw-pi-node = { package = "mw-pi-node"; dir = "pi-node"; bin = "mw-pi-node"; };
}
