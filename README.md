# pixi-oci

Build OCI container images from [pixi](https://pixi.sh) environments, without Docker, BuildKit or any other daemon.

`pixi-oci` reads `pixi.toml` and `pixi.lock`, installs the locked conda packages of one environment into a prefix, slices that prefix into cacheable layers, stacks them on a base image pulled straight from a registry and writes an OCI image tarball. It runs anywhere `pixi` runs and builds `linux-64` or `linux-aarch64` images from macOS or Linux hosts alike.

It replaces the usual multi-stage Dockerfile dance (`pixi install` → `pixi shell-hook` → `COPY --from=build`) with a single command and a few lines of configuration.

## Usage

```shell
pixi-oci build                      # uses ./pixi.toml, writes <name>.tar
pixi-oci build --manifest-path app  # manifest in another directory
pixi-oci build -e prod -p linux-aarch64 -t myapp:1.2.3 -o myapp.tar
pixi-oci build --format oci-dir -o ./myapp   # OCI layout directory instead of a tarball
```

Because the binary is called `pixi-oci`, pixi picks it up as an extension: `pixi oci build` works too.

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
environment = "default"          # pixi environment to install
install-prefix = "/opt/env"      # where the environment lives inside the image
name = "example-project"         # image name (required unless given with --tag)
tag = "latest"
max-layers = 20                  # how many layers the conda packages may use
copy-to-image = [
  "my-file",                                                  # -> <working-dir>/my-file
  { source = "my-file2", destination = "/my-file-renamed" },  # explicit destination
]

[tool.pixi-oci.oci-config]
# https://github.com/moby/docker-image-spec/blob/v1.3.1/spec.md#image-json-field-descriptions
entrypoint = "caddy file-server --browse --root ."   # string (shell-split) or list
cmd = ["--listen", ":8080"]
env = { FOO = "bar" }                                # table or ["FOO=bar"] list
working-dir = "/app"
user = "1000:1000"
labels = { "org.opencontainers.image.source" = "https://github.com/you/example" }
exposed-ports = [8080, "9090/udp"]
volumes = ["/data"]
stop-signal = "SIGTERM"
```

| key | default | description |
| --- | --- | --- |
| `base` | `{}` | A registry image to build on, or a minimal root filesystem built from scratch. See [Base](#base). |
| `environment` | `default` | The pixi environment to put into the image. |
| `platform` | see below | Conda platform to build for. Only linux platforms are supported. |
| `install-prefix` | `/opt/env` | Absolute path of the environment inside the image. |
| `name` | | Image repository name. Required unless `--tag name:tag` is given. |
| `tag` | `latest` | Image tag. |
| `max-layers` | `20` | Upper bound for the number of layers used by the conda packages. |
| `copy-to-image` | `[]` | Files or directories to copy into the image, relative to the manifest. Plain strings land in the working directory under their own name, like `COPY` in a Dockerfile. |
| `activation-shell` | `/bin/bash` | Interpreter for the activation wrapper (only used when the environment has `activate.d` scripts). |
| `oci-config.*` | | Image config fields, named after the docker image spec in kebab-case (`entrypoint`, `cmd`, `env`, `working-dir`, `user`, `labels`, `exposed-ports`, `volumes`, `stop-signal`). The PascalCase spec names are accepted too. |

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

- `glibc` adds Debian's `libc6` package, downloaded from [snapshot.debian.org](https://snapshot.debian.org/binary/libc6/) and verified against the archive's checksum. `true` means `2.41-12+deb13u4` (Debian 13); a string is any Debian `libc6` version. The layer has the libraries, the dynamic loader reachable as `/lib64/ld-linux-x86-64.so.2` (or the platform's equivalent), `/etc/nsswitch.conf` and the copyright file. The iconv `gconv` modules are left out. The build fails if a locked package needs a newer glibc (`__glibc >=...`) than the chosen version.
- `shadow-setup` adds `/etc/passwd`, `/etc/group`, `/etc/shadow` and `/etc/gshadow` with `root` and `nobody`, plus `/root` and a world-writable `/tmp`, like [`shadowSetup`](https://nixos.org/manual/nixpkgs/stable/#ssec-pkgs-dockerTools-shadowSetup) in nixpkgs.

`--base-image` on the command line overrides `base` with a registry reference (or `scratch`).

Builds of `pixi-oci` with the `bundled-glibc` cargo feature embed the default `libc6` packages for `amd64` and `arm64`, so they are not downloaded at runtime. Other versions and architectures are still downloaded. The build script fetches the packages from snapshot.debian.org; set `GLIBC_DEBIAN_FILE` to one or more local `.deb` files (separated like `PATH`) to bundle those instead and build without network access:

```shell
GLIBC_DEBIAN_FILE=libc6_2.41-12+deb13u4_amd64.deb:libc6_2.41-12+deb13u4_arm64.deb \
  cargo build --release --features bundled-glibc
```

### Platform selection

The platform comes from `--platform`, then `platform` in the config, then the current machine if it runs linux, then the single linux platform of the environment. With several linux platforms in the lockfile you have to pick one explicitly. Each invocation builds one platform; `linux-64` becomes `linux/amd64`, `linux-aarch64` becomes `linux/arm64/v8`.

### Environment activation

The image config carries everything `pixi shell-hook` would export: `PATH` with `<install-prefix>/bin` in front, `CONDA_PREFIX`, variables from the packages' `etc/conda/env_vars.d` and `oci-config.env`. Containers therefore start with an activated environment, even for `scratch` images without a shell.

When packages ship `etc/conda/activate.d/*.sh` scripts, `pixi-oci` also writes a tiny wrapper to `<install-prefix>/etc/pixi-oci/entrypoint.sh` that sources them and then `exec`s your entrypoint. The wrapper is prepended to the entrypoint automatically and needs `activation-shell` to exist in the image: a scratch base has no shell, so add `bash` to the environment and point `activation-shell` at `<install-prefix>/bin/bash`.

`[activation]` tables from the manifest (`env` and `scripts`) are not applied.

### Layers

Layers are cheap to cache but every layer has a cost, so `max-layers` bounds how many `pixi-oci` creates for the packages. Packages are sorted by how many other packages depend on them; the most depended-on packages get their own layers at the bottom (they change least often), and whatever does not fit shares the last package layer. Files copied with `copy-to-image` form one more layer on top. Set `max-layers = 1` for a single environment layer.

All layers are reproducible: entries are sorted, owned by root, and carry a fixed modification time (`SOURCE_DATE_EPOCH` if set, else 0). Set `SOURCE_DATE_EPOCH` to also pin the image creation timestamp; two builds of the same lockfile then produce byte-identical tarballs.

### Caching

Conda packages go into the regular rattler package cache, base image layers into `<cache>/pixi-oci/base-images` and Debian packages into `<cache>/pixi-oci/debian`. Override the root with `--cache-dir` or `PIXI_OCI_CACHE_DIR`. Mirrors and channel authentication are read from the usual rattler/pixi sources; pass `--config` to use a pixi config file with `mirrors` settings.

## Limitations

- PyPI dependencies in the lockfile are rejected. Use conda packages for now.
- Only `[tool.pixi-oci]` is read from the manifest. `[activation]` tables are ignored, and `pixi.lock` is not checked against the manifest, so run `pixi lock` after changing dependencies.
- Source packages (pixi-build) are not supported.
- One platform per build; there is no multi-arch index yet.
- `pixi-oci` does not push to registries itself. Use `skopeo copy oci-archive:...` or `podman push`.

## Development

```shell
pixi run build
pixi run test            # needs network: downloads the example packages, ubuntu and libc6
pixi run -e lint pre-commit-run
pixi run example         # builds example/ into example-project.tar
```
