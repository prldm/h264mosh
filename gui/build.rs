//! The app icon: assets/icon.svg rendered at the sizes Windows asks for, packed into an
//! .ico that becomes the executable's icon on Windows, and the largest one as raw RGBA
//! for the window icon (`src/main.rs`).

use std::path::PathBuf;

const SIZES: [u32; 11] = [16, 20, 24, 32, 40, 48, 64, 72, 96, 128, 256];
const SVG: &str = "assets/icon.svg";

fn main() {
    println!("cargo::rerun-if-changed={SVG}");
    let out = PathBuf::from(std::env::var_os("OUT_DIR").expect("OUT_DIR"));
    let svg = std::fs::read(SVG).expect(SVG);
    let tree = resvg::usvg::Tree::from_data(&svg, &resvg::usvg::Options::default()).expect("the icon is a valid SVG");
    let mut ico = ico::IconDir::new(ico::ResourceType::Icon);
    for size in SIZES {
        let mut pixmap = resvg::tiny_skia::Pixmap::new(size, size).expect("icon size");
        let scale = size as f32 / tree.size().width().max(tree.size().height());
        resvg::render(&tree, resvg::tiny_skia::Transform::from_scale(scale, scale), &mut pixmap.as_mut());
        // tiny-skia keeps premultiplied alpha
        let rgba: Vec<u8> = pixmap
            .pixels()
            .iter()
            .flat_map(|p| {
                let c = p.demultiply();
                [c.red(), c.green(), c.blue(), c.alpha()]
            })
            .collect();
        if size == 256 {
            std::fs::write(out.join("icon-256.rgba"), &rgba).expect("writing the window icon");
        }
        let image = ico::IconImage::from_rgba_data(size, size, rgba);
        ico.add_entry(ico::IconDirEntry::encode(&image).expect("encoding an icon image"));
    }
    let path = out.join("icon.ico");
    ico.write(std::fs::File::create(&path).expect("creating the .ico")).expect("writing the .ico");
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        winresource::WindowsResource::new().set_icon(path.to_str().expect("a UTF-8 path")).compile().expect("embedding the icon");
    }
}
