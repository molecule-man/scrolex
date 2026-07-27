# Flatpak

Local/CI Flatpak build for scrolex. Not published to Flathub.

## Prerequisites

```sh
flatpak install flathub org.gnome.Platform//49 org.gnome.Sdk//49 \
    org.freedesktop.Sdk.Extension.rust-stable//25.08 \
    org.freedesktop.Sdk.Extension.llvm20//25.08
```

`flatpak-builder` is also required (e.g. `pacman -S flatpak-builder`).

## Build and install

The app module builds from the local git repo's committed `main`, so commit
before building.

```sh
flatpak-builder --user --install --force-clean \
    build packaging/flatpak/com.andr2i.scrolex.yml
flatpak run com.andr2i.scrolex
```

## Notes

- The PDF engine (mupdf) is vendored and statically linked by the `mupdf`
  crate, so no PDF library is built as a separate module. Its build compiles C
  and runs bindgen, hence the `llvm20` SDK extension for libclang.
- The build is offline. Every crate is fetched by flatpak-builder as a pinned
  archive listed in `cargo-sources.json`, vendored into `cargo/vendor`, and
  built with `cargo build --offline`.

## Regenerating cargo-sources.json

`ci/bump-version` regenerates it on every version bump, so a release always
ships a crate list matching `Cargo.lock`. `ci/check-cargo-sources` runs in CI and
fails if the two drift apart.

To regenerate by hand, after changing dependencies without cutting a release:

```sh
uv run https://raw.githubusercontent.com/flatpak/flatpak-builder-tools/f03a673abe6ce189cea1c2857e2b44af2dd79d1f/cargo/flatpak-cargo-generator.py \
    Cargo.lock -o packaging/flatpak/cargo-sources.json
```

`uv` resolves the script's own dependencies in a throwaway environment, so
nothing needs installing. The generator is pinned to a commit so a release never
runs whatever upstream's `master` happens to say that day.

## Gaps to close before Flathub

- Screenshots in the metainfo (Flathub requires at least one).
- `--filesystem=host:ro` is permitted but gets flagged in review; a document
  viewer has a case for it, to be justified in the submission PR.
- The manifest in the Flathub repo must build from a remote git URL pinned to a
  tag and commit, not this one's local `path:` source.
