use std::path::{Path, PathBuf};

use gtk::{glib, glib::clone, Application, ApplicationWindow, Button, DrawingArea};
use gtk::{prelude::*, EventControllerScrollFlags};
use poppler::Document;
//use poppler::PopplerDocument;

const APP_ID: &str = "com.andr2i.hallyview";

fn main() -> glib::ExitCode {
    let app = Application::builder().application_id(APP_ID).build();

    app.connect_open(move |app, files, _hint| {
        app.activate();

        dbg!(app.active_window());
        dbg!(files);
        //for file in files {
        //    let path = file.path();
        //    let doc = Document::from_file(&path, None).unwrap();
        //    println!("Opened file: {}", path.display());
        //}
        //app.quit();
    });

    app.connect_activate(build_ui);
    app.run()
}

fn build_ui(app: &Application) {
    let header_bar = gtk::HeaderBar::builder().build();

    let open_button = gtk::Button::from_icon_name("document-open");

    header_bar.pack_start(&open_button);

    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .build();

    window.set_titlebar(Some(&header_bar));

    let pages_box = gtk::Box::builder()
        .orientation(gtk::Orientation::Horizontal)
        .build();

    let scroll_controller = gtk::EventControllerScroll::new(
        EventControllerScrollFlags::DISCRETE | EventControllerScrollFlags::VERTICAL,
    );
    scroll_controller.connect_scroll(|_, dx, dy| {
        dbg!(std::time::Instant::now());
        dbg!(dx);
        dbg!(dy);
        glib::signal::Propagation::Stop
    });
    pages_box.add_controller(scroll_controller);

    let scroll_win = gtk::ScrolledWindow::builder()
        .hexpand(true)
        .hscrollbar_policy(gtk::PolicyType::Automatic)
        .child(&pages_box)
        .build();

    window.set_child(Some(&scroll_win));

    let load_doc = clone!(@weak scroll_win, @weak pages_box => move |fname: PathBuf| {
        let fname = fname.to_str().unwrap();
        let doc = Document::from_file(&format!("file://{fname}"), None).unwrap();

        while let Some(child) = pages_box.first_child() {
            pages_box.remove(&child);
        }

        dbg!(doc.n_pages());

        let page1 = doc.page(0).unwrap();
        let (width, height) = page1.size();

        dbg!(page1.size());

        let drawing_area = DrawingArea::new();
        drawing_area.set_size_request(width as i32, height as i32);
        drawing_area.set_draw_func(move |_, cr, _width, _height| {
            page1.render(cr);
        });

        pages_box.append(&drawing_area);

        let page2 = doc.page(1).unwrap();
        let (width, height) = page2.size();

        dbg!(page2.size());

        let drawing_area = DrawingArea::new();
        drawing_area.set_size_request(width as i32, height as i32);
        drawing_area.set_draw_func(move |_, cr, _width, _height| {
            page2.render(cr);
        });



        pages_box.append(&drawing_area);

        dbg!(scroll_win.hadjustment().value());
        dbg!(scroll_win.parent().unwrap().width());
        dbg!(pages_box.last_child().unwrap().width());
        dbg!(pages_box.last_child().unwrap().bounds());
        //scroll_win.hadjustment().unwrap().set_value(0.0);

        let page3 = doc.page(2).unwrap();
        let (width, height) = page3.size();

        dbg!(page3.size());

        let drawing_area = DrawingArea::new();
        drawing_area.set_size_request(width as i32, height as i32);
        drawing_area.set_draw_func(move |_, cr, _width, _height| {
            page3.render(cr);
        });

        pages_box.append(&drawing_area);
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

    window.present();
}

fn example_main() -> glib::ExitCode {
    // Create a new application
    let app = Application::builder().application_id(APP_ID).build();

    // Connect to "activate" signal of `app`
    app.connect_activate(build_example_ui);

    // Run the application
    app.run()
}

fn build_example_ui(app: &Application) {
    // Create a button with label and margins
    let button = Button::builder()
        .label("Press me!")
        .margin_top(12)
        .margin_bottom(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    // Connect to "clicked" signal of `button`
    button.connect_clicked(|button| {
        // Set the label to "Hello World!" after the button has been clicked on
        button.set_label("Hello World!");
    });

    // Create a window
    let window = ApplicationWindow::builder()
        .application(app)
        .title("My GTK App")
        .child(&button)
        .build();

    // Present window
    window.present();
}

//fn example_poppler_main() {
//    let app = Application::builder()
//        .application_id("com.example.pdfviewer")
//        //.flags(
//        //    gtk::gio::ApplicationFlags::HANDLES_OPEN
//        //        | gtk::gio::ApplicationFlags::HANDLES_COMMAND_LINE,
//        //)
//        .build();
//
//    app.connect_activate(|app| {
//        let window = ApplicationWindow::builder()
//            .application(app)
//            .title("PDF Viewer")
//            .default_width(800)
//            .default_height(600)
//            .build();
//
//        let drawing_area = DrawingArea::new();
//        drawing_area.set_draw_func(|_, cr, _width, _height| {
//            // Render the PDF here
//            if let Ok(document) = PopplerDocument::new_from_file("./test.pdf", None) {
//                if let Some(page) = document.pages().next() {
//                    page.render(cr);
//                }
//            }
//        });
//
//        window.set_child(Some(&drawing_area));
//        window.show();
//    });
//
//    app.run();
//}
