use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::{glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags};
use poppler::Document;

mod page;
mod zoom;

const APP_ID: &str = "com.andr2i.hallyview";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let header_bar = gtk::HeaderBar::builder().build();

    let open_button = Button::from_icon_name("document-open");

    header_bar.pack_start(&open_button);

    let zoom_out_button = Button::from_icon_name("zoom-out");
    let zoom_in_button = Button::from_icon_name("zoom-in");

    header_bar.pack_start(&zoom_out_button);
    header_bar.pack_start(&zoom_in_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .build();

    window.set_titlebar(Some(&header_bar));

    let pages_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();

    let scroll_win = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&pages_box)
        .build();

    let scroll_controller = gtk::EventControllerScroll::new(
        EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
    );
    scroll_controller.connect_scroll(clone!( @weak scroll_win, @weak pages_box => @default-return glib::Propagation::Stop, move |_, _dx, dy| {
        if let Some(last_page) = pages_box.last_child() {
            let increment = last_page.width();
            // scroll by one page
            if dy < 0.0 {
                // scroll left
                scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() - increment as f64);
            } else {
                // scroll right
                scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() + increment as f64);
                //scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() + scroll_win.hadjustment().page_increment());
            }
        }

        glib::Propagation::Stop
    }));
    pages_box.add_controller(scroll_controller);

    window.set_child(Some(&scroll_win));

    let zoom_handler = Rc::new(RefCell::new(zoom::ZoomHandler::new(pages_box.clone())));

    let zoom_handler_clone = zoom_handler.clone();
    zoom_in_button.connect_clicked(move |_| {
        zoom_handler_clone.borrow_mut().apply_zoom(1.1);
    });

    let zoom_handler_clone = zoom_handler.clone();
    zoom_out_button.connect_clicked(move |_| {
        zoom_handler_clone.borrow_mut().apply_zoom(1. / 1.1);
    });

    let test_pdf_path = Path::new("./test.pdf").canonicalize().unwrap();

    let pm = Rc::new(RefCell::new(page::PageManager::new(
        Document::from_file(&format!("file://{}", test_pdf_path.to_str().unwrap()), None).unwrap(),
        pages_box.clone(),
        zoom_handler.clone(),
    )));

    pm.borrow_mut().load();

    //let pm_clone = pm.clone();

    open_button.connect_clicked(clone!(@weak app, @strong pm => move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .build();
        let pm_clone = pm.clone();

        dialog.open(app.active_window().as_ref(), gtk::gio::Cancellable::NONE, move |file| {
            if let Ok(file) = file {
                let path = file.path().expect("File has no path");

                pm_clone.borrow_mut().reload(
                    Document::from_file(&format!("file://{}", path.to_str().unwrap()), None).unwrap(),
                );
            }
        })
    }));

    window.present();
}
