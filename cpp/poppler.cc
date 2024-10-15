#include <cairo.h>
#include <iostream>
#include <poppler.h>
#include <poppler/cpp/poppler-document.h>
#include <poppler/cpp/poppler-image.h>
#include <poppler/cpp/poppler-page-renderer.h>
#include <poppler/glib/poppler-page.h>
#include <poppler/glib/poppler.h>

extern "C" {

void render_page(PopplerPage *page, cairo_t *cairo) {
  /*g_return_if_fail(POPPLER_IS_PAGE(page));*/

  std::cout << "Rendering page from cpp 3" << std::endl;
}

void render_doc_page_segfaulting(const char *uri, int page_num,
                                 cairo_t *cairo) {
  std::string filename = std::string(uri);
  poppler::document *doc = poppler::document::load_from_file(filename);
  poppler::page *page = doc->create_page(page_num);

  poppler::page_renderer renderer;

  poppler::image img = renderer.render_page(page);

  if (img.is_valid()) {
    int width = img.width();
    int height = img.height();
    int stride = img.bytes_per_row();

    cairo_surface_t *surface = cairo_image_surface_create_for_data(
        (unsigned char *)img.data(), CAIRO_FORMAT_ARGB32, width, height,
        stride);

    if (cairo_surface_status(surface) == CAIRO_STATUS_SUCCESS) {
      cairo_set_source_surface(cairo, surface, 0, 0);
      cairo_paint(cairo);
      cairo_surface_destroy(surface);
    } else {
      fprintf(stderr, "Failed to create cairo surface\n");
    }
  } else {
    fprintf(stderr, "Failed to render page\n");
  }
}

void render_doc_page(const char *uri, int page_num, cairo_t *cr) {
  std::string filename = std::string(uri);
  poppler::document *doc = poppler::document::load_from_file(filename);
  if (!doc) {
    fprintf(stderr, "Unable to load document: %s\n", uri);
    return;
  }

  poppler::page *page = doc->create_page(page_num);
  if (!page) {
    fprintf(stderr, "Unable to create page: %d\n", page_num);
    return;
  }

  poppler::page_renderer renderer;
  renderer.set_render_hint(poppler::page_renderer::antialiasing, true);
  renderer.set_render_hint(poppler::page_renderer::text_antialiasing, true);
  renderer.set_render_hint(poppler::page_renderer::text_hinting, true);

  double zoom = 1.6;
  poppler::image img = renderer.render_page(page, 144.0 * zoom, 144.0 * zoom);

  if (img.is_valid()) {
    int width = img.width();
    int height = img.height();
    int stride = img.bytes_per_row(); // Stride: number of bytes per row
    const unsigned char *poppler_data =
        (unsigned char *)img.data(); // Poppler's image data

    // Create a copy of the image data to avoid memory ownership issues
    unsigned char *cairo_data = (unsigned char *)malloc(height * stride);
    if (!cairo_data) {
      fprintf(stderr, "Memory allocation failed\n");
      return;
    }
    memcpy(cairo_data, poppler_data, height * stride);

    // Create a Cairo surface using the copied data
    cairo_surface_t *surface = cairo_image_surface_create_for_data(
        cairo_data, CAIRO_FORMAT_ARGB32, width, height, stride);

    if (cairo_surface_status(surface) == CAIRO_STATUS_SUCCESS) {
      cairo_save(cr);
      cairo_scale(cr, 0.5 / zoom, 0.5 / zoom);
      // Render the Cairo surface to the given context
      cairo_set_source_surface(cr, surface, 0, 0);
      cairo_paint(cr);
      cairo_restore(cr);

      // Destroy the surface
      cairo_surface_destroy(surface);
    } else {
      fprintf(stderr, "Failed to create Cairo surface\n");
    }

    // Free the copied image data
    free(cairo_data);
  } else {
    fprintf(stderr, "Failed to render page\n");
  }

  // Clean up Poppler resources
  delete page;
  delete doc;
}
}
