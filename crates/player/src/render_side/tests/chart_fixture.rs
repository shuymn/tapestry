//! A play skin keeping a chart layer and a chart set fitted to it, for
//! the uploader's tests of the attempt's chart images.

use std::sync::Arc;

use crate::render_decl::Look;

/// A green play skin keeping a chart layer, over its own atlas.
pub(super) fn charted(target: &render::Renderer) -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let dir = tempfile::tempdir().unwrap();
    let look = Look {
        chart: true,
        ..Look::left([0, 255, 0, 255])
    };
    let (b, _) = crate::render_decl::bundle(target, dir.path(), &look);
    assert!(b.chart_layer().is_some());
    (dir, b)
}

/// A stage file and a back image as large as their slots of a chart layer
/// of side `side`, read and fitted.
pub(super) fn chart_set(side: u32) -> (tempfile::TempDir, Arc<crate::chart_images::ChartImages>) {
    use crate::chart_images::{Want, derive_set, read_sources};
    let d = tempfile::tempdir().unwrap();
    let s = side / 2 - 8;
    for (name, rgb) in [("s.png", [200, 10, 10]), ("b.png", [10, 10, 200])] {
        let file = std::fs::File::create(d.path().join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), s, s);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let [r, g, b] = rgb;
        let texels = [r, g, b, 255].repeat(usize::try_from(s * s).unwrap());
        e.write_header().unwrap().write_image_data(&texels).unwrap();
    }
    let want = Want {
        dir: d.path().to_owned(),
        names: ["s.png".to_owned(), "b.png".to_owned(), String::new()],
        play: None,
        result: None,
    };
    let sources = read_sources(&want, None, &|| false);
    let set = derive_set(&sources, side, &|| false).unwrap();
    assert!(set.fitted()[..2].iter().all(Option::is_some));
    (d, Arc::new(set))
}
