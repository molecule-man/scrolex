# Flatpak

The manifest scrolex is published from, plus a local build of it for testing
inside the sandbox.

Scrolex ships on Flathub as
[com.andr2i.scrolex](https://flathub.org/apps/com.andr2i.scrolex), built from
the [flathub/com.andr2i.scrolex](https://github.com/flathub/com.andr2i.scrolex)
repo. The manifest here is the one to edit; the Flathub copy is derived from it
(see [Releasing to Flathub](#releasing-to-flathub)).

## Prerequisites

```sh
flatpak install flathub org.gnome.Platform//50 org.gnome.Sdk//50 \
    org.freedesktop.Sdk.Extension.rust-stable//25.08 \
    org.freedesktop.Sdk.Extension.llvm20//25.08
```

`flatpak-builder` is also required (e.g. `pacman -S flatpak-builder`).

## Build and install

Useful for reproducing behaviour that only shows up under the sandbox — portal
file dialogs, `--device=dri` rendering, missing filesystem access.

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

## Releasing to Flathub

The Flathub repo holds its own copy of `com.andr2i.scrolex.yml` and
`cargo-sources.json`, identical to these except for the app module's `sources`
entry: Flathub builds from a remote git URL pinned to a tag and commit, not this
one's local `path:`. Publishing a release means copying both files over and
repinning:

```sh
cd ../flathub-com.andr2i.scrolex        # a clone of flathub/com.andr2i.scrolex
cp ../scrolex/packaging/flatpak/{com.andr2i.scrolex.yml,cargo-sources.json} .
```

Then in the copied manifest replace

```yaml
      - type: git
        path: ../..
        branch: main
```

with the tag and its commit:

```yaml
      - type: git
        url: https://github.com/molecule-man/scrolex.git
        tag: X.Y.Z
        commit: <git rev-parse X.Y.Z>
```

Open that as a PR against `master`; Flathub's buildbot builds it and publishes
once merged.
