# Scrolex - A Horizontally Scrolled Document Viewer

> \- Scroll along the x coordinate  
> \- Scroll along the x  
> \- Scroll the x  
> \- **Scrolex**

Scrolex is a document viewer optimized for HiDPI displays and designed for
distraction-free, efficient reading. With its horizontal scrolling layout,
Scrolex lets you see more pages at once, making it ideal for large monitors and
wide screens.

<a href="https://flathub.org/apps/com.andr2i.scrolex"><img width="190" alt="Get it on Flathub" src="https://flathub.org/api/badge?locale=en"></a>
<a href="https://snapcraft.io/scrolex"><img width="190" alt="Get it from the Snap Store" src="https://snapcraft.io/static/images/badges/en/snap-store-black.svg"></a>
<a href="https://apps.microsoft.com/detail/9p1k2szlqqlk"><img width="230" alt="Get it from Microsoft Store" src="https://get.microsoft.com/images/en-us%20dark.svg"></a>

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

4. Dark Mode

    Scrolex can recolor document pages for comfortable reading in low-light
    environments while preserving their original hues. Dark mode is an
    explicit setting and remains enabled across sessions until turned off.

5. Wayland Support

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
| Ctrl + t        | Open a document in a new tab             |
| Ctrl + w        | Close the current tab or window          |
| `t`             | Toggle table of contents                 |
| F11             | Toggle full screen                       |
| `l` / PageDown  | Next page                                |
| `h` / PageUp    | Previous page                            |
| `w`             | Zoom pages to fit the window's width     |
| Home            | First page                               |
| End             | Last page                                |
| `→`             | Scroll right                             |
| `←`             | Scroll left                              |
| `k` / `↑`       | Pan up (zoomed-in page)                  |
| `j` / `↓`       | Pan down (zoomed-in page)                |
| `]` / Ctrl + `+` / Ctrl + `=` | Zoom in                     |
| `[` / Ctrl + `-` | Zoom out                                |
| Ctrl + `0`      | Reset zoom to 100%                       |
| Mouse wheel     | Move one page left/right per notch       |
| Ctrl + scroll   | Zoom in/out (mouse wheel or touchpad)    |
| `f` / Ctrl + f  | Search in document                       |
| `n` / `F3`      | Next match                               |
| `N` / Shift + F3 | Previous match                          |
| Drag            | Select text (also copied to the primary selection) |
| Ctrl + c        | Copy the selected text to the clipboard  |
| Esc             | Close search / drop the selection / leave full screen |

## Installation. Linux

### 1. Install from the Snap Store (Ubuntu)

Scrolex is on [Snapcraft](https://snapcraft.io/scrolex).

```bash
sudo snap install scrolex
```

Linux Mint blocks snap by default so use Flathub if you are on Mint.

### 2. Install from Flathub (Fedora, openSUSE, Mint, SteamOS, others)

Scrolex is on [Flathub](https://flathub.org/apps/com.andr2i.scrolex).

```bash
flatpak install --user flathub com.andr2i.scrolex
flatpak run com.andr2i.scrolex
```

Drop `--user` from all commands to install system-wide instead (requires
root).

If installation fails, the Flathub remote is possibly not configured. Add it:

```bash
flatpak remote-add --if-not-exists --user flathub https://dl.flathub.org/repo/flathub.flatpakrepo
```

### 3. Install from AUR (Arch Linux)

If you're using Arch Linux or any Arch-based distribution, you can install
Scrolex from the Arch User Repository (AUR).

```bash
yay -S scrolex-bin
```

### 4. Download and install .deb package from GitHub Releases (Debian 13 or newer)

The `.deb` package supports Debian 13 or newer and Ubuntu 24.04 or newer.
Download it from the [GitHub releases page][1], then install it with APT so its
dependencies are resolved. Replace `<version>` with the release you want to
install.

```bash
curl -LO "https://github.com/molecule-man/scrolex/releases/download/<version>/scrolex_<version>.deb"
sudo apt install ./scrolex_<version>.deb
```

### 5. Download from GitHub Releases (any distribution)

GTK 4.14 or newer must already be installed. On Arch:

```bash
sudo pacman -S gtk4
```

Then download the latest x86-64 pre-built binary archive from the [GitHub
releases page][1], extract it, and run the `scrolex` executable.

### 6. Build from source

Needs GTK 4.14 or newer plus a C/C++ toolchain and `clang`, since the `mupdf`
crate compiles its bundled C library and generates bindings with bindgen. On
Arch:

```bash
sudo pacman -S gtk4 clang base-devel
```

Then:

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

## Installation. Windows

### 1. Install from the Microsoft Store

Scrolex is on the [Microsoft Store](https://apps.microsoft.com/detail/9p1k2szlqqlk).
The Store installs the app and keeps it updated.

You can also install it from a terminal:

```powershell
winget install --id 9P1K2SZLQQLK --source msstore
```

### 2. Download the zip from GitHub Releases

Download `scrolex-<version>-x86_64-windows.zip` from the [GitHub releases
page][1], extract it anywhere, and run `scrolex.exe`.

The zip carries its own GTK runtime, so you need nothing else. It installs
nothing. The data files Scrolex creates while running are stored in `%LOCALAPPDATA%`.

Windows shows a SmartScreen warning, because the executable is not signed yet.
Select **More info**, then **Run anyway**.

## License

Scrolex is licensed under the [GNU Affero General Public License v3.0 or
later](LICENSE) (AGPL-3.0-or-later).

The document engine, [MuPDF](https://mupdf.com/), is vendored and statically linked.
MuPDF is dual-licensed by Artifex Software under the AGPL v3 or a commercial
license; because it is statically linked, distributed Scrolex binaries are
covered by the AGPL. If you need to distribute Scrolex under different terms,
you must obtain a commercial MuPDF license from Artifex.


[1]: https://github.com/molecule-man/scrolex/releases/latest
