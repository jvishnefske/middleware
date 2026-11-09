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
# To add a binary, add one line here; nothing else in the flake changes.
{
  hello = { package = "mw-service"; dir = "service"; example = "hello"; };
}
