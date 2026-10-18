// rs-vips links `vips`/`glib-2.0`/`gobject-2.0` by name only; pkg-config supplies the search path.
fn main() {
    pkg_config::Config::new()
        .atleast_version("8.15")
        .probe("vips")
        .expect("libvips >= 8.15 (brew install vips / apt install libvips-dev)");
}
