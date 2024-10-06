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

## Installation

### 0. Dependencies

If you are not installing the app via a package manager (currently only
available on AUR), you will need to manually install the `gtk4` and `poppler`
dependencies.

On arch, you can install these dependencies with:

```bash
sudo pacman -S gtk4 poppler
```

### 1. Download from GitHub Releases

You can download the latest pre-built binary directly from the [GitHub releases
page][1].

### 2. Install from AUR (Arch Linux)

If you're using Arch Linux or any Arch-based distribution, you can install
Scrolex from the Arch User Repository (AUR).

```bash
yay -S scrolex-bin
```

### 3. Download and install .deb package from GitHub Releases

If you are Debian (or Ubuntu) user, then you can download a `.deb` file directly from the [GitHub releases
page][1] and install it.

```bash
curl -LO 'https://github.com/molecule-man/scrolex/releases/download/0.1.0-alpha+3/scrolex_0.1.0-alpha+3.deb'
sudo dpkg -i scrolex_0.1.0-alpha+3.deb
```

### 4. Build from source

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
