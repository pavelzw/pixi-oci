# pixi-oci

[![CI](https://img.shields.io/github/actions/workflow/status/pavelzw/pixi-oci/ci.yml?style=flat-square&branch=main)](https://github.com/pavelzw/pixi-oci/actions/workflows/ci.yml)
[![Conda Platform](https://img.shields.io/conda/vn/conda-forge/pixi-oci?style=flat-square)](https://prefix.dev/channels/conda-forge/packages/pixi-oci)

Build OCI container images from [pixi](https://pixi.sh) environments, without Docker, BuildKit or any other daemon.

`pixi-oci` reads `pixi.toml` and `pixi.lock`, installs the locked conda packages of one environment into a prefix, slices that prefix into cacheable layers, stacks them on a base image pulled straight from a registry and writes an OCI image tarball.

It replaces the usual multi-stage Dockerfile dance (`pixi install` → `pixi shell-hook` → `COPY --from=build`) with a single command and a few lines of configuration.

## Usage

```shell
pixi-oci build
pixi-oci build --manifest-path app
pixi-oci build -e prod -p linux-aarch64 -t myapp:1.2.3 -o myapp.tar
pixi-oci build --format oci-dir -o ./myapp
```

The tarball is an OCI image layout that also carries a docker-archive `manifest.json`, so every common tool accepts it:

```shell
docker load -i myapp.tar
podman load -i myapp.tar
skopeo copy oci-archive:myapp.tar docker://ghcr.io/you/myapp:1.2.3
skopeo inspect oci-archive:myapp.tar
```

## Configuration

Everything lives in the `[tool.pixi-oci]` table of your `pixi.toml` (or `pyproject.toml`). Command line flags override the corresponding keys. The rest of the manifest is not read: the packages come from `pixi.lock`.

```toml
[workspace]
name = "example-project"
channels = ["conda-forge"]
platforms = ["osx-arm64", "linux-64", "linux-aarch64"]

[dependencies]
caddy = "*"

[tool.pixi-oci]
base = { shadow-setup = true, glibc = "2.41-12+deb13u4" }   # or a registry image, see below
environment = "default"
install-prefix = "/opt/env"
tag = "example-project:latest"
max-layers = 20
copy-to-image = [
  # -> <working-dir>/my-file
  "my-file",
  { source = "my-file2", destination = "/my-file-renamed" },
]

[tool.pixi-oci.oci-config]
# https://github.com/moby/docker-image-spec/blob/v1.3.1/spec.md#image-json-field-descriptions
entrypoint = "caddy file-server --browse --root ."
cmd = ["--listen", ":8080"]
env = { FOO = "bar" }
working-dir = "/app"
user = "1000:1000"
labels = { "org.opencontainers.image.source" = "https://github.com/you/example" }
exposed-ports = [8080, "9090/udp"]
volumes = ["/data"]
stop-signal = "SIGTERM"
shell = ["/opt/env/bin/bash", "-c"]

[tool.pixi-oci.oci-config.healthcheck]
test = ["curl", "-f", "http://localhost:8080"]
interval = "30s"
timeout = "5s"
start-period = "10s"
start-interval = "1s"
retries = 3
```

| key                | description                                                                                                                                                                                                                             |
| ------------------ | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `base`             | A registry image to build on, or a minimal root filesystem built from scratch. See [Base](#base).                                                                                                                                       |
| `environment`      | The pixi environment to put into the image.                                                                                                                                                                                             |
| `platform`         | Conda platform to build for. Only linux platforms are supported.                                                                                                                                                                        |
| `install-prefix`   | Absolute path of the environment inside the image.                                                                                                                                                                                      |
| `tag`              | Image reference as `name:tag`, or `name` for `name:latest`. Required unless `--tag` is given, which replaces it.                                                                                                                        |
| `max-layers`       | Upper bound for the number of layers used by the conda packages.                                                                                                                                                                        |
| `copy-to-image`    | Files or directories to copy into the image, relative to the manifest. Plain strings land in the working directory under their own name, like `COPY` in a Dockerfile.                                                                   |
| `activation-shell` | Interpreter for the activation wrapper (only used when the environment has `activate.d` scripts).                                                                                                                                       |
| `oci-config.*`     | Image config fields, named after the docker image spec in kebab-case (`entrypoint`, `cmd`, `env`, `working-dir`, `user`, `labels`, `exposed-ports`, `volumes`, `stop-signal`, `healthcheck`, `shell`). See [Healthcheck](#healthcheck). |

### Healthcheck

`oci-config.healthcheck` works like `HEALTHCHECK` in a Dockerfile. A string `test` runs through the image's shell (`CMD-SHELL`), a list is executed directly (`CMD`). Lists that already start with `CMD`, `CMD-SHELL` or `NONE` are kept as they are, so `test = ["NONE"]` disables a healthcheck inherited from the base image. Durations are Go-style strings (`"30s"`, `"1m30s"`, `"500ms"`) or integers in seconds.

`oci-config.shell` sets the shell for `CMD-SHELL` healthchecks, like `SHELL` in a Dockerfile. A scratch base has no `/bin/sh`, so either use the list form of `test` or add `bash` to the environment and set `shell = ["<install-prefix>/bin/bash", "-c"]`.

Both fields are docker extensions to the OCI image config: docker honours them, runtimes that only follow the OCI image spec ignore them. Unset fields are inherited from the base image.

### Base

`base` is either a registry reference or a table describing a root filesystem built from scratch:

```toml
# a public registry image, pulled anonymously
base = "ubuntu:latest"

# from scratch, with glibc and a shadow setup (the default, same as `base = {}`)
base = { shadow-setup = true, glibc = true }

# from scratch, pinning the glibc version
base = { glibc = "2.41-12+deb13u4" }

# a completely empty root filesystem (same as `base = "scratch"`)
base = { shadow-setup = false, glibc = false }
```

- `glibc` adds Debian's `libc6` package, downloaded from [snapshot.debian.org](https://snapshot.debian.org/binary/libc6/) and verified against the archive's checksum. `true` means `2.41-12+deb13u4` (Debian 13); a string is any Debian `libc6` version. The layer has the libraries, the dynamic loader reachable as `/lib64/ld-linux-x86-64.so.2` (or the platform's equivalent) and `/etc/nsswitch.conf`.
- `shadow-setup` adds `/etc/passwd`, `/etc/group`, `/etc/shadow` and `/etc/gshadow` with `root` and `nobody`, plus `/root` and a world-writable `/tmp`, like [`shadowSetup`](https://nixos.org/manual/nixpkgs/stable/#ssec-pkgs-dockerTools-shadowSetup) in nixpkgs.

Builds of `pixi-oci` with the `bundled-glibc` cargo feature embed the default `libc6` packages for `amd64` and `arm64`, so they are not downloaded at runtime. Other versions and architectures are still downloaded.

### Environment activation

The image config carries everything `pixi shell-hook` would export: `PATH` with `<install-prefix>/bin` in front, `CONDA_PREFIX`, variables from the packages' `etc/conda/env_vars.d` and `oci-config.env`. Containers therefore start with an activated environment, even for `scratch` images without a shell.

When packages ship `etc/conda/activate.d/*.sh` scripts, `pixi-oci` also writes a tiny wrapper to `<install-prefix>/etc/pixi-oci/entrypoint.sh` that sources them and then `exec`s your entrypoint. The wrapper is prepended to the entrypoint automatically and needs `activation-shell` to exist in the image: a scratch base has no shell, so add `bash` to the environment and point `activation-shell` at `<install-prefix>/bin/bash`.

`[activation]` tables from the manifest (`env` and `scripts`) are not applied.

## Limitations

- PyPI dependencies in the lockfile are rejected. Use conda packages for now.
- Only `[tool.pixi-oci]` is read from the manifest. `[activation]` tables are ignored, and `pixi.lock` is not checked against the manifest, so run `pixi lock` after changing dependencies.
- Source packages (pixi-build) are not yet supported.
- One platform per build; there is no multi-arch index yet.
- `pixi-oci` does not push to registries itself. Use `skopeo copy oci-archive:...` or `podman push`.

## Development

```shell
pixi run build
pixi run test
pixi run pre-commit-run
pixi run example
```
