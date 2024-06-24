use std::cell::Cell;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use gtk::{glib, glib::clone, Application, ApplicationWindow, Button, DrawingArea};
use gtk::{prelude::*, EventControllerScrollFlags};
use poppler::Document;
//use poppler::PopplerDocument;

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

    let zoom = Rc::new(Cell::new(1.0));

    let load_doc = clone!(@weak scroll_win, @weak pages_box, @strong zoom => move |fname: PathBuf| {
        zoom.set(1.0);

        let fname = fname.to_str().unwrap();
        let doc = Document::from_file(&format!("file://{fname}"), None).unwrap();

        while let Some(child) = pages_box.first_child() {
            pages_box.remove(&child);
        }

        for page_num in 0..doc.n_pages() {
            let page = doc.page(page_num).unwrap();
            let (width, height) = page.size();

            let drawing_area = DrawingArea::new();
            drawing_area.set_size_request(width as i32, height as i32);
            drawing_area.set_draw_func(clone!(@strong zoom => move |_, cr, _width, _height| {
                cr.scale(zoom.get(), zoom.get());
                page.render(cr);
            }));

            pages_box.append(&drawing_area);
        }
    });

    let test_pdf_path = Path::new("./test.pdf").canonicalize().unwrap();
    load_doc(test_pdf_path);

    open_button.connect_clicked(clone!(@weak app, @strong load_doc => move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .build();
        dialog.open(app.active_window().as_ref(), gtk::gio::Cancellable::NONE, clone!(@strong load_doc => move |file| {
            if let Ok(file) = file {
                let path = file.path().expect("File has no path");
                load_doc(path);
            }
        }))
    }));

    let apply_zoom = clone!(@weak pages_box, @strong zoom => move |zoom_factor: f64| {
        zoom.set(zoom.get() * zoom_factor);

        let mut child = pages_box.first_child();

        while let Some(c) = child {
            let page = c.downcast_ref::<DrawingArea>().unwrap();
            let width = page.width();
            let height = page.height();
            page.set_size_request((width as f64 * zoom_factor) as i32, (height as f64 * zoom_factor) as i32);
            page.queue_draw();
            child = c.next_sibling();
        }
    });

    zoom_in_button.connect_clicked(clone!(@strong apply_zoom => move |_| {
        apply_zoom(1.1);
    }));

    zoom_out_button.connect_clicked(clone!(@strong apply_zoom => move |_| {
        apply_zoom(1./1.1);
    }));

    window.present();
}
