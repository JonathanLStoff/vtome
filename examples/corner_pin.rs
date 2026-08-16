//! The trapezoid, on paper: what a corner-pinned quad does to a picture, and
//! how far wrong the obvious implementation goes.
//!
//! ```sh
//! make corner-pin
//! cargo run --example corner_pin
//! ```
//!
//! No window and no GPU — this prints the geometry, so the maths can be checked
//! against a ruler before anything is drawn with it.

use vtome::geometry::{Mat3, Point, Quad, Rect};

fn main() -> Result<(), vtome::Error> {
    // A projector aimed upwards at a wall: the top of the image is narrower
    // than the bottom, which is what "keystone" means.
    let screen = Rect::from_size(1920.0, 1080.0);
    let quad = Quad::keystone(screen, 240.0);

    println!(
        "keystoned quad on a {}×{} screen",
        screen.width, screen.height
    );
    for (name, corner) in ["top-left", "top-right", "bottom-right", "bottom-left"]
        .iter()
        .zip(quad.corners)
    {
        println!("  {name:<13} ({:7.1}, {:7.1})", corner.x, corner.y);
    }

    let map = quad.homography()?;
    let Mat3(rows) = map;

    println!("\nthe projective map from the unit square:");
    for row in rows {
        println!("  [{:9.4} {:9.4} {:9.4}]", row[0], row[1], row[2]);
    }
    println!(
        "  the bottom row is the perspective part — zero only for a parallelogram \
         (this quad is {})",
        if quad.is_affine() { "affine" } else { "not" }
    );

    // Where the middle of the picture belongs, and where naive interpolation
    // would put it.
    let centre = map.transform(Point::new(0.5, 0.5));
    let average = Point::new(
        quad.corners.iter().map(|corner| corner.x).sum::<f64>() / 4.0,
        quad.corners.iter().map(|corner| corner.y).sum::<f64>() / 4.0,
    );

    println!("\nthe centre of the picture:");
    println!(
        "  projective   ({:7.1}, {:7.1})  ← where it belongs",
        centre.x, centre.y
    );
    println!(
        "  averaged     ({:7.1}, {:7.1})  ← what linear UV interpolation gives",
        average.x, average.y
    );
    println!(
        "  they differ by {:.1} px. That difference is the crease down the diagonal.",
        centre.distance(average)
    );

    // A row across the middle, to show the error is not confined to one point.
    println!("\nerror along the horizontal centre line:");
    println!(
        "  {:>6}  {:>10}  {:>10}  {:>8}",
        "u", "correct x", "naive x", "error"
    );

    for step in 0..=8 {
        let u = f64::from(step) / 8.0;
        let correct = map.transform(Point::new(u, 0.5));

        // What you get by interpolating the corners linearly: lerp the top and
        // bottom edges, then lerp between them.
        let top = lerp(quad.corners[0], quad.corners[1], u);
        let bottom = lerp(quad.corners[3], quad.corners[2], u);
        let naive = lerp(top, bottom, 0.5);

        println!(
            "  {u:>6.3}  {:>10.1}  {:>10.1}  {:>8.1}",
            correct.x,
            naive.x,
            correct.distance(naive)
        );
    }

    // And the failure that is refused rather than drawn.
    let bowtie = Quad::new([
        Point::new(0.0, 0.0),
        Point::new(100.0, 0.0),
        Point::new(0.0, 100.0),
        Point::new(100.0, 100.0),
    ]);

    println!("\na self-intersecting quad:");
    match bowtie.homography() {
        Ok(_) => println!("  ...was accepted, which is a bug"),
        Err(error) => println!("  {error}"),
    }

    Ok(())
}

fn lerp(a: Point, b: Point, t: f64) -> Point {
    Point::new(a.x + (b.x - a.x) * t, a.y + (b.y - a.y) * t)
}
