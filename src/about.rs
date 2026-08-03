use std::path::Path;

use gtk::glib;
use gtk::prelude::*;

const NEW_ISSUE_URL: &str = "https://github.com/molecule-man/scrolex/issues/new";
const VERSION: &str = env!("CARGO_PKG_VERSION");
const COPY_FEEDBACK_MS: u32 = 2000;

pub fn present(parent: &gtk::Window) {
    let builder = gtk::Builder::from_resource("/com/andr2i/scrolex/about.ui");
    let dialog: gtk::Window = builder
        .object("about_window")
        .expect("about.ui misses about_window");
    let version_button: gtk::Button = builder
        .object("btn_version")
        .expect("about.ui misses btn_version");
    let issue_link: gtk::LinkButton = builder
        .object("btn_issue")
        .expect("about.ui misses btn_issue");

    version_button.set_label(VERSION);
    version_button.connect_clicked(copy_debug_info);
    issue_link.set_uri(&issue_url());

    dialog.set_transient_for(Some(parent));
    dialog.present();
}

fn copy_debug_info(button: &gtk::Button) {
    button.display().clipboard().set_text(&debug_info());

    button.set_label("Copied");
    button.set_sensitive(false);
    glib::timeout_add_local_once(
        std::time::Duration::from_millis(u64::from(COPY_FEEDBACK_MS)),
        glib::clone!(
            #[weak]
            button,
            move || {
                button.set_label(VERSION);
                button.set_sensitive(true);
            }
        ),
    );
}

fn debug_info() -> String {
    format_debug_info(
        &gtk_version(),
        os_name().as_deref(),
        installed_as_flatpak(),
        &gdk_backend(),
        std::env::var("XDG_CURRENT_DESKTOP").ok().as_deref(),
    )
}

fn issue_url() -> String {
    build_issue_url(os_name().as_deref(), installed_as_flatpak())
}

fn format_debug_info(
    gtk_version: &str,
    os: Option<&str>,
    flatpak: bool,
    backend: &str,
    desktop: Option<&str>,
) -> String {
    let mut lines = vec![
        format!("scrolex {VERSION}"),
        format!("GTK {gtk_version}"),
        format!("OS: {}", os.unwrap_or("unknown")),
        format!("Install: {}", install_method(flatpak)),
        format!("Backend: {backend}"),
    ];
    if let Some(desktop) = desktop {
        lines.push(format!("Desktop: {desktop}"));
    }
    lines.join("\n")
}

// The bug report template is a GitHub issue form: every field can be prefilled with a query
// parameter named after its id.
fn build_issue_url(os: Option<&str>, flatpak: bool) -> String {
    let mut url = format!(
        "{NEW_ISSUE_URL}?template=bug_report.yml&version={}",
        escape(&format!("scrolex {VERSION}"))
    );
    if let Some(os) = os {
        url.push_str(&format!("&os={}", escape(os)));
    }
    // The dropdown accepts only its own options, and outside flatpak we can't tell how scrolex was
    // installed, so leave it for the reporter to pick.
    if flatpak {
        url.push_str(&format!("&install-method={}", escape("Flatpak")));
    }
    url
}

fn escape(value: &str) -> String {
    glib::Uri::escape_string(value, None, false).to_string()
}

fn install_method(flatpak: bool) -> &'static str {
    if flatpak {
        "Flatpak"
    } else {
        "unknown (not flatpak)"
    }
}

fn installed_as_flatpak() -> bool {
    Path::new("/.flatpak-info").exists()
}

fn gdk_backend() -> String {
    gtk::gdk::Display::default().map_or_else(
        || "unknown".to_string(),
        |display| short_backend_name(display.type_().name()),
    )
}

// "GdkWaylandDisplay" -> "Wayland"
fn short_backend_name(type_name: &str) -> String {
    type_name
        .strip_prefix("Gdk")
        .and_then(|name| name.strip_suffix("Display"))
        .unwrap_or(type_name)
        .to_string()
}

fn gtk_version() -> String {
    format!(
        "{}.{}.{}",
        gtk::major_version(),
        gtk::minor_version(),
        gtk::micro_version()
    )
}

fn os_name() -> Option<String> {
    // Inside flatpak /etc/os-release describes the runtime; the host one is bind-mounted.
    ["/run/host/os-release", "/etc/os-release"]
        .iter()
        .find_map(|path| std::fs::read_to_string(path).ok())
        .and_then(|content| parse_pretty_name(&content))
}

fn parse_pretty_name(os_release: &str) -> Option<String> {
    os_release.lines().find_map(|line| {
        line.strip_prefix("PRETTY_NAME=")
            .map(|value| value.trim_matches('"').to_string())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_url_prefills_version_and_os() {
        let url = build_issue_url(Some("Arch Linux"), false);
        assert!(url.contains("template=bug_report.yml"), "{url}");
        assert!(
            url.contains(&format!("version=scrolex%20{VERSION}")),
            "{url}"
        );
        assert!(url.contains("os=Arch%20Linux"), "{url}");
        assert!(!url.contains("install-method"), "{url}");
    }

    #[test]
    fn issue_url_prefills_flatpak_install_method() {
        let url = build_issue_url(None, true);
        assert!(url.contains("install-method=Flatpak"), "{url}");
        assert!(!url.contains("&os="), "{url}");
    }

    #[test]
    fn debug_info_lists_environment() {
        let info = format_debug_info("4.14.0", Some("Fedora 40"), true, "Wayland", Some("GNOME"));
        assert_eq!(
            info,
            format!(
                "scrolex {VERSION}\nGTK 4.14.0\nOS: Fedora 40\nInstall: Flatpak\nBackend: Wayland\nDesktop: GNOME"
            )
        );
    }

    #[test]
    fn debug_info_tolerates_unknown_environment() {
        let info = format_debug_info("4.14.0", None, false, "unknown", None);
        assert!(info.contains("OS: unknown"), "{info}");
        assert!(info.contains("Install: unknown (not flatpak)"), "{info}");
        assert!(!info.contains("Desktop:"), "{info}");
    }

    #[test]
    fn backend_name_is_shortened() {
        assert_eq!(short_backend_name("GdkWaylandDisplay"), "Wayland");
        assert_eq!(short_backend_name("GdkX11Display"), "X11");
        assert_eq!(short_backend_name("Whatever"), "Whatever");
    }

    #[test]
    fn pretty_name_is_unquoted() {
        let content = "ID=arch\nPRETTY_NAME=\"Arch Linux\"\nHOME_URL=\"x\"\n";
        assert_eq!(parse_pretty_name(content).as_deref(), Some("Arch Linux"));
    }

    #[test]
    fn pretty_name_missing() {
        assert_eq!(parse_pretty_name("ID=arch\n"), None);
    }
}
