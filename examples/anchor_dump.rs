fn main() {
    for f in std::env::args().skip(1) {
        let img = image::open(&f).unwrap().to_rgba8();
        println!("{f}\n{:#?}", anima::render::anchors::find(&img));
    }
}
