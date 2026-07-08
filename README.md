# Scrolex - A Horizontally Scrolled PDF Viewer

> \- Scroll along the x coordinate  
> \- Scroll along the x  
> \- Scroll the x  
> \- **Scrolex**

🚧 **Under Heavy Development** 🚧

This project is still under active development and may contain bugs or
incomplete features. While it is functional and can be used, please be aware
that certain aspects might change rapidly, and stability is not guaranteed. Use
at your own discretion, and check back for updates as the app evolves.

---

Scrolex is a high-performance PDF viewer specifically optimized for HiDPI
displays and designed for distraction-free, efficient reading. With its
horizontal scrolling layout, Scrolex lets you see more pages at once, making it
ideal for large monitors and wide screens.

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

    Scrolex offers full Wayland support, ensuring compatibility with modern
    Linux systems. Whether you're using X11 or Wayland, Scrolex will run
    smoothly.

## Shortcuts

| Key / Action    | Description                              |
| --------------- | ---------------------------------------- |
| `o`             | Open a document                          |
| `l`             | Next page                                |
| `h`             | Previous page                            |
| `→`             | Scroll right                             |
| `←`             | Scroll left                              |
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

Packaged installs bundle everything they need, so **no manual dependency
installation is required** for the Flatpak, AUR, or `.deb` methods below.

You only need to install `gtk4` and `poppler` yourself when running the raw
pre-built binary or building from source.

On arch, you can install these dependencies with:

```bash
sudo pacman -S gtk4 poppler
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

You can download the latest pre-built binary directly from the [GitHub releases
page][1].

### 3. Install from AUR (Arch Linux)

If you're using Arch Linux or any Arch-based distribution, you can install
Scrolex from the Arch User Repository (AUR).

```bash
yay -S scrolex-bin
```

### 4. Download and install .deb package from GitHub Releases

If you are Debian (or Ubuntu) user, then you can download a `.deb` file directly from the [GitHub releases
page][1] and install it. Replace `<version>` with the release you want to install.

```bash
curl -LO "https://github.com/molecule-man/scrolex/releases/download/<version>/scrolex_<version>.deb"
sudo dpkg -i scrolex_<version>.deb
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


[1]: https://github.com/molecule-man/scrolex/releases/latest
