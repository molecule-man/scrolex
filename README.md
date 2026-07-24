# Scrolex - A Horizontally Scrolled Document Viewer

> \- Scroll along the x coordinate  
> \- Scroll along the x  
> \- Scroll the x  
> \- **Scrolex**

Scrolex is a document viewer optimized for HiDPI displays and designed for
distraction-free, efficient reading. With its horizontal scrolling layout,
Scrolex lets you see more pages at once, making it ideal for large monitors and
wide screens.

https://github.com/user-attachments/assets/225c4b69-eb15-48d0-b978-f7bd747d463e

## Features

1. Horizontal Scrolling Layout

    Main Feature: Unlike traditional viewers, Scrolex uses a horizontal scroll
    layout along the X-axis. This layout lets users easily view multiple pages
    side by side, especially on widescreen or HiDPI monitors. It offers a
    refreshing and seamless reading experience for long documents.

2. Margin Cropping

    Scrolex includes a feature to crop document margins, allowing even more
    content to fit on-screen without unnecessary whitespace.

3. Scroll Wheel for Page Navigation

    Intuitive Navigation: Along with keyboard navigation Scrolex supports a
    simple scroll wheel action for moving between pages. Each scroll of the
    wheel moves the document one page to the left or right, offering quick and
    effortless navigation. This design choice minimizes interruptions and
    distractions, making it easy to stay focused on reading without needing to
    search for the needed keyboard key.

4. Wayland Support

    Scrolex supports both Wayland and X11 sessions.

## Supported Formats

Scrolex renders every format its bundled MuPDF engine handles. The most common
multipage document formats are:

- **PDF**
- **EPUB** (unencrypted)
- **MOBI**
- **FB2**
- **XPS / OpenXPS**
- **CBZ** (comic book archives)

Single-page formats such as SVG, plain text, and common raster images (PNG,
JPEG, TIFF, …) open as well. DjVu is not supported.

## Shortcuts

| Key / Action    | Description                              |
| --------------- | ---------------------------------------- |
| `o` / Ctrl + o  | Open a document                          |
| `t`             | Toggle table of contents                 |
| `l` / PageDown  | Next page                                |
| `h` / PageUp    | Previous page                            |
| Home            | First page                               |
| End             | Last page                                |
| `→`             | Scroll right                             |
| `←`             | Scroll left                              |
| `k` / `↑`       | Pan up (zoomed-in page)                  |
| `j` / `↓`       | Pan down (zoomed-in page)                |
| `]`             | Zoom in                                  |
| `[`             | Zoom out                                 |
| Mouse wheel     | Move one page left/right per notch       |
| Ctrl + scroll   | Zoom in/out (mouse wheel or touchpad)    |
| `f` / Ctrl + f  | Search in document                       |
| `n` / `F3`      | Next match                               |
| `N` / Shift + F3 | Previous match                          |
| Esc             | Close search                             |

## Installation

### 0. Dependencies

The Flatpak bundle includes the application dependencies and downloads its
GNOME runtime from Flathub. The AUR and `.deb` packages declare their system
dependencies so their package managers can install them.

The release artifacts currently support x86-64 Linux. The raw pre-built binary
needs GTK 4.12 or newer at runtime; the document engine (MuPDF) is statically
linked.

On arch:

```bash
sudo pacman -S gtk4
```

Building from source additionally needs a C/C++ toolchain and `clang`, since
the `mupdf` crate compiles its bundled C library and generates bindings with
bindgen:

```bash
sudo pacman -S gtk4 clang base-devel
```

### 1. Install the Flatpak bundle

Download the `.flatpak` bundle from the [GitHub releases page][1].

The bundle is self-contained, but it pulls its runtime (the GNOME Platform and
GPU/codec extensions) from Flathub, so Flathub must be configured first. If it
isn't already, add it:

```bash
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo
```

Then install the bundle by pointing `flatpak install` at the file directly, and
run it:

```bash
flatpak install --user scrolex_*.flatpak
flatpak run com.andr2i.scrolex
```

Drop `--user` from all commands to install system-wide instead (requires
root).

### 2. Download from GitHub Releases

You can download the latest x86-64 pre-built binary archive directly from the
[GitHub releases page][1]. Extract it and run the `scrolex` executable. GTK
4.12 or newer must already be installed.

### 3. Install from AUR (Arch Linux)

If you're using Arch Linux or any Arch-based distribution, you can install
Scrolex from the Arch User Repository (AUR).

```bash
yay -S scrolex-bin
```

### 4. Download and install .deb package from GitHub Releases

The `.deb` package supports Debian 13 or newer and Ubuntu 24.04 or newer.
Download it from the [GitHub releases page][1], then install it with APT so its
dependencies are resolved. Replace `<version>` with the release you want to
install.

```bash
curl -LO "https://github.com/molecule-man/scrolex/releases/download/<version>/scrolex_<version>.deb"
sudo apt install ./scrolex_<version>.deb
```

### 5. Build from source

```bash
# clone the repository
git clone https://github.com/molecule-man/scrolex.git
cd scrolex
# build the project using Cargo:
cargo build --release
```

After building, you will find the binary at the location
`target/release/scrolex`. You can move the binary to a directory in your
`$PATH`.

## License

Scrolex is licensed under the [GNU Affero General Public License v3.0 or
later](LICENSE) (AGPL-3.0-or-later).

The document engine, [MuPDF](https://mupdf.com/), is vendored and statically linked.
MuPDF is dual-licensed by Artifex Software under the AGPL v3 or a commercial
license; because it is statically linked, distributed Scrolex binaries are
covered by the AGPL. If you need to distribute Scrolex under different terms,
you must obtain a commercial MuPDF license from Artifex.


[1]: https://github.com/molecule-man/scrolex/releases/latest
