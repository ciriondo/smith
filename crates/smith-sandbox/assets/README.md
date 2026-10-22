# Sandbox base image

`debian-slim.tar.gz` is the gzip-compressed docker archive of the sandbox base image. It is
embedded in `smith` with `include_bytes!` (`crates/smith-sandbox/src/image.rs`) and loaded
into the private podman store with `podman load` when the store does not contain the tag.

The archive is `debian:stable-slim@sha256:d405c1e1d9eecbf30022d23766a0eea235e7f499aa33e357dfd29271a839f82a`,
tagged `localhost/smith-base:debian-13.7`. The digest is Debian 13.7 with glibc 2.41, captured
on 2026-10-19. The archive is amd64; another architecture needs its own archive, tag and digest.

Regenerate it on an amd64 host:

```sh
store="${XDG_DATA_HOME:-$HOME/.local/share}/smith/containers/storage"
runroot="${XDG_RUNTIME_DIR:-/tmp/smith-runroot/$(id -u)}/smith/containers"
base=debian:stable-slim@sha256:d405c1e1d9eecbf30022d23766a0eea235e7f499aa33e357dfd29271a839f82a
podman --root "$store" --runroot "$runroot" pull "$base"
podman --root "$store" --runroot "$runroot" tag "$base" localhost/smith-base:debian-13.7
podman --root "$store" --runroot "$runroot" save localhost/smith-base:debian-13.7 \
  | gzip -9 > crates/smith-sandbox/assets/debian-slim.tar.gz
```

The archive contains a Debian image and its packages. The repository's MIT license covers the
smith code, not the Debian packages. The image keeps the packages' copyright files under
`/usr/share/doc`; a release that ships the archive includes the Debian license texts.
