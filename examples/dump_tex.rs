use anyhow::Context;

fn main() -> anyhow::Result<()> {
    let mut args = std::env::args().skip(1);
    let input = args.next().context("usage: dump_tex <in.tex> <out.png>")?;
    let output = args.next().context("usage: dump_tex <in.tex> <out.png>")?;
    let data = std::fs::read(&input)?;
    let decoded = anima::pkg::decode_tex(&data)?;
    decoded.image.save(&output)?;
    println!(
        "{} -> {} ({}x{})",
        input,
        output,
        decoded.image.width(),
        decoded.image.height()
    );
    Ok(())
}
