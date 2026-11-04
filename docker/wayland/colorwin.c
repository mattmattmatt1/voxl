/* Fullscreen Wayland client that cycles saturated colors so a capture can be checked. */
#include <gtk/gtk.h>

static gboolean draw(GtkWidget *w, cairo_t *cr, gpointer data) {
    (void)data;
    int n = GPOINTER_TO_INT(g_object_get_data(G_OBJECT(w), "n"));
    static const double colors[3][3] = {
        {0.95, 0.08, 0.12},
        {0.08, 0.78, 0.18},
        {0.12, 0.28, 0.95},
    };
    const double *c = colors[(n / 2) % 3];
    cairo_set_source_rgb(cr, c[0], c[1], c[2]);
    cairo_paint(cr);
    cairo_set_source_rgb(cr, 1, 1, 1);
    cairo_move_to(cr, 64, 120);
    cairo_set_font_size(cr, 42);
    cairo_show_text(cr, "voxl wayland");
    return FALSE;
}

static gboolean tick(gpointer w) {
    int n = GPOINTER_TO_INT(g_object_get_data(G_OBJECT(w), "n"));
    g_object_set_data(G_OBJECT(w), "n", GINT_TO_POINTER(n + 1));
    gtk_widget_queue_draw(GTK_WIDGET(w));
    return TRUE;
}

int main(int argc, char **argv) {
    gtk_init(&argc, &argv);
    GtkWidget *w = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    // The draw handler has to be on a child. A bare GtkWindow stays the
    // compositor background, which a capture then reports as an empty desktop.
    GtkWidget *area = gtk_drawing_area_new();
    gtk_container_add(GTK_CONTAINER(w), area);
    gtk_window_set_default_size(GTK_WINDOW(w), 1280, 720);
    gtk_window_set_decorated(GTK_WINDOW(w), FALSE);
    gtk_window_fullscreen(GTK_WINDOW(w));
    gtk_window_maximize(GTK_WINDOW(w));
    g_signal_connect(area, "draw", G_CALLBACK(draw), NULL);
    g_signal_connect(w, "destroy", G_CALLBACK(gtk_main_quit), NULL);
    g_timeout_add(200, tick, area);
    gtk_widget_show_all(w);
    gtk_window_present(GTK_WINDOW(w));
    gtk_main();
    return 0;
}
