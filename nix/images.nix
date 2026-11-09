# OCI images as docker-archive tarballs (`docker load`, `podman load`,
# `k3s ctr images import`), one per static binary.
#
# Each image holds the binary's closure (the binary alone: it is static),
# CA certificates and the zoneinfo database, and nothing else: no shell,
# no package manager, no /bin. It runs as uid/gid 65532 ("nonroot"), so it
# satisfies `runAsNonRoot` and a read-only root filesystem. Timestamps are
# the epoch and the tarball is uncompressed, so the same inputs give the
# same bytes.
{ pkgs, lib, source, revision, tag }:
let
  nonroot = "65532";
in
{
  # `exe`: the derivation whose bin/<exe> is the entrypoint.
  mkImage = name: exe: pkgs.dockerTools.buildLayeredImage {
    name = "localhost/mw/${name}";
    inherit tag;
    contents = [ pkgs.cacert pkgs.tzdata ];
    # buildLayeredImage keeps every file in a layer at uid 0; nothing in
    # the image is written at run time, so no chown is needed.
    compressor = "none";
    config = {
      Entrypoint = [ "${exe}/bin/${exe.exe}" ];
      User = "${nonroot}:${nonroot}";
      Env = [
        "SSL_CERT_FILE=${pkgs.cacert}/etc/ssl/certs/ca-bundle.crt"
        "TZDIR=${pkgs.tzdata}/share/zoneinfo"
      ];
      Labels = {
        "org.opencontainers.image.source" = source;
        "org.opencontainers.image.revision" = revision;
        "org.opencontainers.image.version" = exe.version;
        "org.opencontainers.image.title" = name;
      };
    };
  };
}
