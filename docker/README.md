# Containerised builds

These Dockerfiles produce builder images that match Beam's deploy targets
exactly. Use them when you don't have Ubuntu locally (Synology, TrueNAS, macOS,
NixOS) or when you want to verify both `24.04` and `26.04` paths in CI.

## Quick reference

```bash
# 24.04 (existing target, regression)
docker build -t beam-build-2404 -f docker/Dockerfile.dev-24.04 .
docker run --rm -v "$PWD":/work -w /work beam-build-2404 make check

# 26.04 (new target, Wayland backend)
docker build -t beam-build-2604 -f docker/Dockerfile.dev-26.04 .
docker run --rm -v "$PWD":/work -w /work beam-build-2604 make check

# Interactive shell for iterative work
docker run --rm -it -v "$PWD":/work -w /work beam-build-2604 bash
```

## Caching the cargo registry

Add `-v beam-cargo:/cargo/registry -v beam-target:$PWD/target` to avoid
re-downloading crates and recompiling from scratch on every run.

## What's installed

Both images carry the build-time dependencies pulled from `scripts/install.sh`,
plus `nfpm` for producing `.deb` artefacts. **No runtime services** (Xorg,
PulseAudio, GStreamer plugins beyond build-time `.pc` files) — these images
build the project, they don't run it.

## NAS notes

On Synology DSM with Container Manager:

```bash
# Repo lives on a shared volume; mount it directly.
/usr/local/bin/docker run --rm \
    -v "/volume1/Anthropic - Claude AI/rdp-beam":/work -w /work \
    beam-build-2604 make check
```

Container Manager runs as root inside the container by default, so file
ownership in the working directory may flip to root afterwards — pass
`--user "$(id -u):$(id -g)"` if that matters to your workflow.
