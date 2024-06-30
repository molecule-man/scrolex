use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use gtk::{glib, glib::clone, Application, ApplicationWindow, Button};
use gtk::{prelude::*, EventControllerScrollFlags};
use poppler::Document;

mod page;
mod state;

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

    let crop_left_minus_button = Button::from_icon_name("pan-start");
    let crop_left_text = gtk::Label::new(Some("Left crop"));
    let crop_left_plus_button = Button::from_icon_name("pan-end");

    let crop_right_minus_button = Button::from_icon_name("pan-start");
    let crop_right_text = gtk::Label::new(Some("Right crop"));
    let crop_right_plus_button = Button::from_icon_name("pan-end");

    header_bar.pack_end(&crop_right_plus_button);
    header_bar.pack_end(&crop_right_text);
    header_bar.pack_end(&crop_right_minus_button);

    header_bar.pack_end(&crop_left_plus_button);
    header_bar.pack_end(&crop_left_text);
    header_bar.pack_end(&crop_left_minus_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .build();

    window.set_titlebar(Some(&header_bar));

    let pages_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .spacing(2)
        .build();

    let scroll_win = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&pages_box)
        .build();

    window.set_child(Some(&scroll_win));

    let test_pdf_path = Path::new("./test.pdf").canonicalize().unwrap();

    let last_loaded_document_path = Rc::new(RefCell::new(test_pdf_path.clone()));

    let pm = Rc::new(RefCell::new(page::PageManager::new(
        Document::from_file(&format!("file://{}", test_pdf_path.to_str().unwrap()), None).unwrap(),
        pages_box.clone(),
    )));

    let pm_clone = pm.clone();
    zoom_in_button.connect_clicked(move |_| {
        pm_clone.borrow_mut().apply_zoom(1.1);
    });

    let pm_clone = pm.clone();
    zoom_out_button.connect_clicked(move |_| {
        pm_clone.borrow_mut().apply_zoom(1. / 1.1);
    });

    let pm_clone = pm.clone();
    crop_left_plus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(1, 0));

    let pm_clone = pm.clone();
    crop_left_minus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(-1, 0));

    let pm_clone = pm.clone();
    crop_right_plus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(0, 1));

    let pm_clone = pm.clone();
    crop_right_minus_button.connect_clicked(move |_| pm_clone.borrow_mut().adjust_crop(0, -1));

    pm.borrow_mut().load(state::load(&test_pdf_path));

    open_button.connect_clicked(clone!(@weak app, @strong pm, @weak scroll_win, @strong last_loaded_document_path => move |_| {
        let dialog = gtk::FileDialog::builder()
            .title("Open PDF File")
            .modal(true)
            .build();

        dialog.open(app.active_window().as_ref(), gtk::gio::Cancellable::NONE, clone!(@strong pm, @weak scroll_win, @strong last_loaded_document_path => move |file| {
            if let Ok(file) = file {
                let path = file.path().expect("File has no path").canonicalize().unwrap();

                state::save(last_loaded_document_path.borrow().as_path(), &pm.borrow().current_state()).unwrap();

                pm.borrow_mut().reload(
                    Document::from_file(&format!("file://{}", path.to_str().unwrap()), None).unwrap(),
                    state::load(&path),
                );

                last_loaded_document_path.replace(path);
            }
        }))
    }));

    let scroll_controller = gtk::EventControllerScroll::new(
        EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
    );
    scroll_controller.connect_scroll(clone!( @weak scroll_win, @weak pages_box, @weak pm => @default-return glib::Propagation::Stop, move |_, _dx, dy| {
        if let Some(last_page) = pages_box.last_child() {
            let increment = last_page.width();
            // scroll by one page
            if dy < 0.0 {
                // scroll left
                if !pm.borrow_mut().shift_loading_buffer_left() {
                    scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() - increment as f64);
                }
            } else {
                // scroll right
                if !pm.borrow_mut().shift_loading_buffer_right() {
                    scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() + increment as f64);
                }
                //scroll_win.hadjustment().set_value(scroll_win.hadjustment().value() + scroll_win.hadjustment().page_increment());
            }
        }

        glib::Propagation::Stop
    }));
    pages_box.add_controller(scroll_controller);

    app.connect_shutdown(move |_| {
        state::save(
            last_loaded_document_path.borrow().as_path(),
            &pm.borrow().current_state(),
        )
        .unwrap();
    });

    window.present();
}
